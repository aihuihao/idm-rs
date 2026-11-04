//! 磁盘并发写入集成测试（仅 `cfg(windows)`）。
//!
//! 测试 6：16 个线程直写 ChunkWriter（共享句柄 + seek_write），逐字节校验。
//! 测试 7：同样的校验，但通过 `DiskWriterPool` 在 tokio 多线程运行时中由大量
//!         并发任务写入（队列容量 4，触发背压）。
//! 测试 8：`#[ignore]` 压力测试，2GB / 32 Chunk / 三种策略。
#![cfg(windows)]

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use idm_core::disk::{AllocationStrategy, ChunkWriter, DiskWriterPool, SpaceCheck, TargetFile};
use tempfile::tempdir;

const FILE_SIZE: u64 = 64 * 1024 * 1024; // 64MB
const N_CHUNKS: usize = 16;

/// 确定性字节值：每个字节由它的偏移量计算。
fn byte_at(offset: u64) -> u8 {
    ((offset.wrapping_mul(2654435761) ^ (offset >> 13)) & 0xFF) as u8
}

/// 测试内自写的 xorshift64（不引入 rand）。
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// 生成 n 个相邻且互不重叠的区间，覆盖 `[0, size)`。
fn make_chunks(state: &mut u64, n: usize, size: u64) -> Vec<(u64, u64)> {
    let mut pts: Vec<u64> = vec![0, size];
    while pts.len() < n + 1 {
        let c = (xorshift(state) % (size - 1)) + 1;
        if !pts.contains(&c) {
            pts.push(c);
        }
    }
    pts.sort_unstable();
    (0..n).map(|i| (pts[i], pts[i + 1])).collect()
}

/// 用确定性字节流把单个 Chunk 顺序写满（块大小 1 字节 ~ 256KB 随机）。
fn fill_chunk(
    writer: &mut ChunkWriter,
    start: u64,
    end: u64,
    state: &mut u64,
) -> idm_core::error::Result<()> {
    let mut offset = start;
    while offset < end {
        let remaining = (end - offset) as usize;
        let max_blk = 256 * 1024;
        let blk = std::cmp::min(remaining, (xorshift(state) % max_blk as u64 + 1) as usize);
        let mut buf = vec![0u8; blk];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = byte_at(offset + i as u64);
        }
        writer.write(&buf)?;
        offset += blk as u64;
    }
    Ok(())
}

/// 用独立的文件句柄逐字节校验。
fn verify_file(path: &Path, size: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = File::open(path)?;
    let mut off = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for (i, &b) in buf[..n].iter().enumerate() {
            assert_eq!(
                b,
                byte_at(off + i as u64),
                "byte mismatch at {}",
                off + i as u64
            );
        }
        off += n as u64;
    }
    assert_eq!(off, size, "file length mismatch");
    Ok(())
}

fn run_std_concurrency(
    strategy: AllocationStrategy,
    seeds: u64,
    size: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    for seed in 0..seeds {
        let dir = tempdir()?;
        let path = dir.path().join("conc.bin");
        let target = TargetFile::create(&path, size, strategy, SpaceCheck::Enforce)?;
        let mut st = seed.wrapping_add(0x9E37);
        let boundaries = make_chunks(&mut st, N_CHUNKS, size);

        let mut writers = Vec::new();
        for (i, &(s, e)) in boundaries.iter().enumerate() {
            writers.push(target.allocate_chunk(i as u64, s, e)?);
        }

        let mut handles = Vec::new();
        for (mut w, &(s, e)) in writers.into_iter().zip(boundaries.iter()) {
            let mut ls = (s ^ e.wrapping_mul(2654435761)) | 1;
            let h = std::thread::spawn(move || fill_chunk(&mut w, s, e, &mut ls));
            handles.push(h);
        }
        for h in handles {
            h.join().map_err(|_| "chunk writer thread panicked")??;
        }
        drop(target);
        verify_file(&path, size)?;
    }
    Ok(())
}

#[test]
fn concurrency_setlen() -> Result<(), Box<dyn std::error::Error>> {
    run_std_concurrency(AllocationStrategy::SetLen, 20, FILE_SIZE)?;
    Ok(())
}

#[test]
fn concurrency_reserve() -> Result<(), Box<dyn std::error::Error>> {
    run_std_concurrency(AllocationStrategy::Reserve, 20, FILE_SIZE)?;
    Ok(())
}

#[test]
fn concurrency_sparse() -> Result<(), Box<dyn std::error::Error>> {
    run_std_concurrency(AllocationStrategy::Sparse, 20, FILE_SIZE)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试 7：DiskWriterPool + tokio 多线程运行时
// ---------------------------------------------------------------------------

async fn run_pool_concurrency(
    strategy: AllocationStrategy,
    seeds: u64,
    size: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    for seed in 0..seeds {
        let dir = tempdir()?;
        let path = dir.path().join("pool.bin");
        let target = TargetFile::create(&path, size, strategy, SpaceCheck::Enforce)?;
        let pool = Arc::new(DiskWriterPool::new(8, 4, target.file(), path.clone())?);

        let mut st = seed.wrapping_add(0x85EB);
        let boundaries = make_chunks(&mut st, N_CHUNKS, size);
        let mut writes: Vec<(u64, u64, Bytes)> = Vec::new();
        for (i, &(s, e)) in boundaries.iter().enumerate() {
            let mut ls = (s ^ e.wrapping_mul(2654435761)) | 1;
            let mut offset = s;
            while offset < e {
                let remaining = (e - offset) as usize;
                let max_blk = 256 * 1024;
                let blk =
                    std::cmp::min(remaining, (xorshift(&mut ls) % max_blk as u64 + 1) as usize);
                let mut buf = vec![0u8; blk];
                for (k, b) in buf.iter_mut().enumerate() {
                    *b = byte_at(offset + k as u64);
                }
                writes.push((i as u64, offset, Bytes::from(buf)));
                offset += blk as u64;
            }
        }

        let mut tasks = Vec::new();
        for (cid, off, data) in writes {
            let pool = pool.clone();
            tasks.push(tokio::spawn(
                async move { pool.write(cid, off, data).await },
            ));
        }
        for t in tasks {
            // 外层 JoinError，内层 idm_core::Result
            t.await??;
        }

        pool.sync()?;
        Arc::try_unwrap(pool)
            .map_err(|_| "disk writer pool still referenced after all tasks joined")?
            .shutdown()
            .await?;
        verify_file(&path, size)?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn pool_setlen() -> Result<(), Box<dyn std::error::Error>> {
    run_pool_concurrency(AllocationStrategy::SetLen, 20, FILE_SIZE).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn pool_reserve() -> Result<(), Box<dyn std::error::Error>> {
    run_pool_concurrency(AllocationStrategy::Reserve, 20, FILE_SIZE).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn pool_sparse() -> Result<(), Box<dyn std::error::Error>> {
    run_pool_concurrency(AllocationStrategy::Sparse, 20, FILE_SIZE).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试 8：压力测试（ignored）
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn stress_three_strategies() -> Result<(), Box<dyn std::error::Error>> {
    const BIG: u64 = 2 * 1024 * 1024 * 1024u64; // 2GB
    const N: usize = 32;
    for strategy in [
        AllocationStrategy::SetLen,
        AllocationStrategy::Reserve,
        AllocationStrategy::Sparse,
    ] {
        for seed in 0..3u64 {
            let dir = tempdir()?;
            let path = dir.path().join("stress.bin");
            let start = std::time::Instant::now();
            let target = TargetFile::create(&path, BIG, strategy, SpaceCheck::Enforce)?;
            let mut st = seed.wrapping_add(0x1234);
            let boundaries = make_chunks(&mut st, N, BIG);

            let mut writers = Vec::new();
            for (i, &(s, e)) in boundaries.iter().enumerate() {
                writers.push(target.allocate_chunk(i as u64, s, e)?);
            }
            let mut handles = Vec::new();
            for (mut w, &(s, e)) in writers.into_iter().zip(boundaries.iter()) {
                let mut ls = (s ^ e.wrapping_mul(2654435761)) | 1;
                let h = std::thread::spawn(move || fill_chunk(&mut w, s, e, &mut ls));
                handles.push(h);
            }
            for h in handles {
                h.join().map_err(|_| "stress writer thread panicked")??;
            }
            target.sync_data()?;
            let elapsed = start.elapsed();
            println!("stress {strategy:?} seed {seed} 2GB in {elapsed:.2?}");
            drop(target);
            verify_file(&path, BIG)?;
        }
    }
    Ok(())
}
