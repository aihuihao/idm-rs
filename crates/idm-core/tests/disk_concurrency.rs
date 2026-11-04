//! 磁盘模块并发集成测试：三种分配策略下，多线程并发写同一文件，
//! 再逐字节校验内容一致。同时覆盖 Phase 1a.1 新增的落盘线程池 API。
//!
//! 测试代码刻意不使用 `unwrap`/`expect`（workspace lint 已将其设为 `warn`，
//! 验收脚本以 `-D warnings` 运行 clippy），全部改用 `?` 传播错误。

use bytes::Bytes;
use idm_core::disk::{AllocationStrategy, ChunkWriter, DiskWriterPool, SpaceCheck, TargetFile};
use idm_core::error::{Error, Result};
use std::sync::Arc;
use tempfile::tempdir;

const BLOCK: u64 = 64 * 1024;
const F: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB 压力测试文件

/// 测试用的极小确定性 PRNG（xorshift64*），避免引入 `rand` 依赖。
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    /// `[lo, hi]` 闭区间内生成一个数（hi >= lo）。
    fn gen_range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + (self.next_u64() % (hi - lo + 1))
    }
}

/// 把 size 切成若干不重叠区间（用于并发写入压力测试）。
fn make_chunks(rng: &mut Lcg, n: usize, size: u64) -> Vec<(u64, u64)> {
    let mut chunks = Vec::with_capacity(n);
    let mut offset = 0u64;
    for i in 0..n {
        // 最后一块补齐到 size，避免遗漏尾部。
        let remaining = size - offset;
        if i == n - 1 {
            chunks.push((offset, size));
            break;
        }
        let max = (remaining / 2).max(BLOCK);
        let len = rng.gen_range(BLOCK, max);
        let end = (offset + len).min(size);
        chunks.push((offset, end));
        offset = end;
        if offset >= size {
            break;
        }
    }
    chunks
}

/// 确定性字节：offset 决定内容，便于逐字节校验。
fn byte_at(offset: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for k in 0..len {
        v.push(((offset + k as u64) & 0xff) as u8);
    }
    v
}

/// 逐字节校验文件内容是否等于 byte_at 的期望值。
fn verify_file(path: &std::path::Path, chunks: &[(u64, u64)], size: u64) -> Result<()> {
    let data = std::fs::read(path).map_err(|e| Error::Io {
        operation: "read-back",
        path: path.to_path_buf(),
        offset: 0,
        source: e,
    })?;
    assert_eq!(data.len() as u64, size);
    for &(s, e) in chunks {
        for off in s..e {
            let expected = byte_at(off, 1)[0];
            assert_eq!(data[off as usize], expected, "mismatch at offset {off}");
        }
    }
    Ok(())
}

/// 单线程顺序写满一个区间（不依赖线程池）。
fn fill_chunk(w: &mut ChunkWriter, _base: u64, s: u64, e: u64) -> Result<()> {
    let mut off = s;
    while off < e {
        let left = (e - off).min(BLOCK);
        let buf = byte_at(off, left as usize);
        w.write(&buf)?;
        off += left;
    }
    assert_eq!(w.position(), e, "chunk must be fully written");
    Ok(())
}

/// 标准并发：每个 chunk 一个任务，直接持有 ChunkWriter 顺序写入（不走线程池）。
async fn run_std_concurrency(
    strategy: AllocationStrategy,
    seeds: usize,
    size: u64,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let tmp = tempdir()?;
    let path = tmp.path().join("std.bin");
    let target = TargetFile::create(&path, size, strategy, SpaceCheck::Enforce)?;

    let boundaries = make_chunks(&mut Lcg::new(0xBEEF), seeds, size);
    let mut handles = Vec::new();
    for (i, &(s, e)) in boundaries.iter().enumerate() {
        let mut w = target.allocate_chunk(i as u64, s, e)?;
        handles.push(tokio::spawn(async move {
            fill_chunk(&mut w, i as u64, s, e)?;
            Ok::<(), Error>(())
        }));
    }
    for h in handles {
        let r: Result<()> = h.await?;
        r?;
    }
    target.sync_data()?;
    drop(target);
    verify_file(&path, &boundaries, size)?;
    Ok(())
}

/// 线程池并发：一个全局共享的 `DiskWriterPool` 服务本文件的所有 chunk。
///
/// 每个 chunk 通过 `register` 取得 `PoolChunk` 句柄，提交写入按 `chunk_id` 路由到固定
/// 线程，由该线程独占的 `ChunkWriter` 顺序写入；区间检查与位置推进都在落盘线程内完成。
async fn run_pool_concurrency(
    strategy: AllocationStrategy,
    seeds: usize,
    size: u64,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let tmp = tempdir()?;
    let path = tmp.path().join("pool.bin");
    let target = TargetFile::create(&path, size, strategy, SpaceCheck::Enforce)?;
    let pool = Arc::new(DiskWriterPool::new(8, 4)?);

    let boundaries = make_chunks(&mut Lcg::new(0xBEEF), seeds, size);
    let mut pool_chunks = Vec::with_capacity(boundaries.len());
    for (i, &(s, e)) in boundaries.iter().enumerate() {
        let w = target.allocate_chunk(i as u64, s, e)?;
        pool_chunks.push(pool.register(w).await?);
    }

    let mut tasks = Vec::new();
    for (cid, &(s, e)) in boundaries.iter().enumerate() {
        let pool = pool.clone();
        let pc = pool_chunks[cid].clone();
        tasks.push(tokio::spawn(async move {
            let mut off = s;
            while off < e {
                let left = (e - off).min(BLOCK);
                let buf = byte_at(off, left as usize);
                pool.write(&pc, Bytes::from(buf)).await?;
                off += left;
            }
            Ok::<(), Error>(())
        }));
    }
    for t in tasks {
        let r: Result<()> = t.await?;
        r?;
    }
    pool.sync(&target)?;

    let pool = Arc::try_unwrap(pool)
        .map_err(|_| -> Box<dyn std::error::Error> { "pool still referenced".into() })?;
    pool.shutdown().await?;
    verify_file(&path, &boundaries, size)?;
    Ok(())
}

/// 三种策略各跑一遍并发压力测试，文件 2 GiB、20 个种子。
async fn stress_three_strategies() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let big = F;
    let seeds = 20;
    for strategy in [
        AllocationStrategy::SetLen,
        AllocationStrategy::Reserve,
        AllocationStrategy::Sparse,
    ] {
        let t0 = std::time::Instant::now();
        run_std_concurrency(strategy, seeds, big).await?;
        let t1 = std::time::Instant::now();
        run_pool_concurrency(strategy, seeds, big).await?;
        let t2 = std::time::Instant::now();
        println!(
            "STRESS {strategy:?} size={big} seeds={seeds} std={:?} pool={:?}",
            t1 - t0,
            t2 - t1
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "heavy 2GiB concurrency stress; run via `cargo test --release -- --ignored`"]
async fn concurrency_std_and_pool() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // 三种策略 × 20 个种子，覆盖 SetLen / Reserve / Sparse。
    stress_three_strategies().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_target(
        path: &Path,
        size: u64,
    ) -> std::result::Result<TargetFile, Box<dyn std::error::Error>> {
        Ok(TargetFile::create(
            path,
            size,
            AllocationStrategy::Sparse,
            SpaceCheck::Enforce,
        )?)
    }

    #[tokio::test]
    async fn pool_write_out_of_range_rejected()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let path = tmp.path().join("oor.bin");
        let target = make_target(&path, 1024)?;
        let pool = DiskWriterPool::new(2, 4)?;
        let w = target.allocate_chunk(0, 0, 16)?;
        let pc = pool.register(w).await?;
        // 区间仅为 [0,16)，写入 32 字节越过尾部 → 必须被拒绝，且不写任何字节。
        let res = pool.write(&pc, Bytes::from(vec![5u8; 32])).await;
        assert!(res.is_err(), "out-of-range write must be rejected");
        pool.release(pc).await?;
        pool.shutdown().await?;
        let data = std::fs::read(&path)?;
        assert!(
            data.iter().all(|&b| b == 0),
            "file content must be unchanged (zeros)"
        );
        Ok(())
    }

    #[tokio::test]
    async fn one_pool_serves_two_files() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let p1 = tmp.path().join("f1.bin");
        let p2 = tmp.path().join("f2.bin");
        let t1 = make_target(&p1, 512)?;
        let t2 = make_target(&p2, 512)?;
        let pool = Arc::new(DiskWriterPool::new(2, 4)?);
        // 两个不同的目标文件，各自的 chunk 都通过同一个全局线程池提交。
        // 注意：同一池内 chunk_id 必须全局唯一（线程池按 chunk_id 路由并以它为键），
        // 因此两个文件使用不同的 chunk_id（0 与 1）。
        let pc1 = pool.register(t1.allocate_chunk(0, 0, 256)?).await?;
        let pc2 = pool.register(t2.allocate_chunk(1, 256, 512)?).await?;
        for i in 0..4u64 {
            pool.write(&pc1, Bytes::from(vec![(i + 1) as u8; 64]))
                .await?;
            pool.write(&pc2, Bytes::from(vec![(10 + i) as u8; 64]))
                .await?;
        }
        pool.sync(&t1)?;
        pool.sync(&t2)?;
        let pool = Arc::try_unwrap(pool)
            .map_err(|_| -> Box<dyn std::error::Error> { "pool still referenced".into() })?;
        pool.shutdown().await?;
        let d1 = std::fs::read(&p1)?;
        let d2 = std::fs::read(&p2)?;
        assert_eq!(d1.len(), 512);
        assert_eq!(d2.len(), 512);
        for i in 0..4u64 {
            for k in 0..64usize {
                assert_eq!(d1[((i * 64) + k as u64) as usize], (i + 1) as u8);
                assert_eq!(d2[((256 + i * 64) + k as u64) as usize], (10 + i) as u8);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn release_returns_write_position() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let tmp = tempdir()?;
        let path = tmp.path().join("rel.bin");
        let target = make_target(&path, 1024)?;
        let pool = DiskWriterPool::new(1, 4)?;
        let w = target.allocate_chunk(0, 0, 1024)?;
        let pc = pool.register(w).await?;
        pool.write(&pc, Bytes::from(vec![1u8; 100])).await?;
        pool.write(&pc, Bytes::from(vec![2u8; 50])).await?;
        let pos = pool.release(pc).await?;
        assert_eq!(pos, 150, "release must return final write position");
        pool.shutdown().await?;
        let data = std::fs::read(&path)?;
        assert_eq!(data.len(), 1024);
        assert!(data[..150].iter().all(|&b| b == 1 || b == 2));
        assert!(data[150..].iter().all(|&b| b == 0));
        Ok(())
    }

    #[tokio::test]
    async fn shrink_end_via_pool_normal_and_illegal()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let path = tmp.path().join("shrink.bin");
        let target = make_target(&path, 1024)?;
        let pool = DiskWriterPool::new(1, 4)?;
        let w = target.allocate_chunk(0, 0, 1024)?;
        let pc = pool.register(w).await?;
        pool.write(&pc, Bytes::from(vec![9u8; 500])).await?;
        // 合法：new_end(600) >= 已写位置(500) 且 <= 原 end(1024)。
        let ok = pool.shrink_end(&pc, 600).await;
        assert!(ok.is_ok(), "shrink to >= pos allowed");
        // 非法：new_end(100) < 已写位置(500)。
        let bad = pool.shrink_end(&pc, 100).await;
        assert!(bad.is_err(), "shrink below written position must fail");
        pool.release(pc).await?;
        pool.shutdown().await?;
        Ok(())
    }
}
