//! 落盘线程池：把异步写请求交给专用阻塞线程，形成背压。
//!
//! 一个进程内共享一个 `DiskWriterPool`，服务所有目标文件。调用方先用
//! `TargetFile::allocate_chunk` 取得 `ChunkWriter`，再通过 `register` 把所有权移交给
//! 线程池，得到一个可克隆的轻量句柄 `PoolChunk`。
//!
//! 线程池内部用自增的 `pool_id` 作为路由键与 `HashMap` 的键，**不使用** `ChunkWriter`
//! 自带的 `chunk_id`——否则两个不同文件的相同 `chunk_id` 会撞键、导致数据被写进错误的
//! 文件（数据安全）。`PoolChunk` 同时保留原来的 `chunk_id` 与路径，仅供日志使用。
//!
//! 写入：通过 `write` 提交 `Bytes`，区间检查与位置推进都在落盘线程内由 `ChunkWriter`
//! 完成；越界时通过回执返回错误且一个字节都不写。
//!
//! 刷盘语义：`sync` 返回成功后，所有在调用 `sync` 之前已经收到回执的写入都保证已落盘。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::disk::TargetFile;
use crate::disk::write::ChunkWriter;
use crate::error::{Error, Result};

/// 轻量句柄：可 `Clone`。`pool_id` 是线程池分配的全局唯一路由键；`chunk_id` 与 `path`
/// 仅用于日志/错误上下文。
#[derive(Clone)]
pub struct PoolChunk {
    pool_id: u64,
    chunk_id: u64,
    path: PathBuf,
}

impl PoolChunk {
    /// 原 `ChunkWriter` 的 chunk 编号（日志用）。
    pub fn chunk_id(&self) -> u64 {
        self.chunk_id
    }

    /// 目标文件路径（日志用）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 线程池内部请求。
enum Request {
    /// 把 `ChunkWriter` 的所有权移交给其固定线程。
    Register {
        pool_id: u64,
        writer: ChunkWriter,
        reply: oneshot::Sender<Result<()>>,
    },
    /// 写入一块数据（位置由 `ChunkWriter` 内部维护）。
    Write {
        pool_id: u64,
        chunk_id: u64,
        path: PathBuf,
        data: Bytes,
        reply: oneshot::Sender<Result<()>>,
    },
    /// 注销区间并返回最终写入位置。
    Release {
        pool_id: u64,
        chunk_id: u64,
        path: PathBuf,
        reply: oneshot::Sender<Result<u64>>,
    },
    /// 缩小区间尾部。
    Shrink {
        pool_id: u64,
        chunk_id: u64,
        path: PathBuf,
        new_end: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    /// 兜底清理：移除本线程内所有属于 `path` 的残留写入器，返回移除数量。
    /// 正常协作式释放后写入器已被 `Release` 移除；本请求仅用于分块 panic/泄露的兜底。
    Purge {
        path: PathBuf,
        reply: oneshot::Sender<usize>,
    },
}

/// 落盘线程池。
pub struct DiskWriterPool {
    senders: Vec<mpsc::Sender<Request>>,
    handles: Vec<std::thread::JoinHandle<()>>,
    /// 自增分配器：每次 `register` 取一个全局唯一的 `pool_id` 作为路由键。
    next_id: AtomicU64,
}

impl std::fmt::Debug for DiskWriterPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskWriterPool")
            .field("threads", &self.senders.len())
            .field("handles", &self.handles.len())
            .finish()
    }
}

/// 单线程内处理请求：按 `pool_id` 路由到本线程持有的 `ChunkWriter`。
fn worker(mut rx: mpsc::Receiver<Request>) {
    let mut writers: HashMap<u64, ChunkWriter> = HashMap::new();
    while let Some(req) = rx.blocking_recv() {
        match req {
            Request::Register {
                pool_id,
                writer,
                reply,
            } => {
                // pool_id 是线程池分配的全局唯一路由键，作为本线程内 HashMap 的键。
                writers.insert(pool_id, writer);
                let _ = reply.send(Ok(()));
            }
            Request::Write {
                pool_id,
                chunk_id,
                path,
                data,
                reply,
            } => {
                let res = match writers.get_mut(&pool_id) {
                    Some(w) => w.write(&data).map(|_| ()),
                    None => Err(Error::ChunkNotRegistered {
                        pool_id,
                        chunk_id,
                        path,
                    }),
                };
                let _ = reply.send(res);
            }
            Request::Release {
                pool_id,
                chunk_id,
                path,
                reply,
            } => {
                let res = match writers.remove(&pool_id) {
                    Some(w) => Ok(w.position()),
                    None => Err(Error::ChunkNotRegistered {
                        pool_id,
                        chunk_id,
                        path,
                    }),
                };
                let _ = reply.send(res);
            }
            Request::Shrink {
                pool_id,
                chunk_id,
                path,
                new_end,
                reply,
            } => {
                let res = match writers.get_mut(&pool_id) {
                    Some(w) => w.shrink_end(new_end),
                    None => Err(Error::ChunkNotRegistered {
                        pool_id,
                        chunk_id,
                        path,
                    }),
                };
                let _ = reply.send(res);
            }
            Request::Purge { path, reply } => {
                // 移除所有属于该路径的残留写入器（含其持有的文件句柄）。
                let before = writers.len();
                writers.retain(|_, w| w.path() != path.as_path());
                let removed = before - writers.len();
                let _ = reply.send(removed);
            }
        }
    }
}

impl DiskWriterPool {
    /// 创建 `threads` 个名为 `idm-disk-N` 的阻塞线程，每个线程持有一条容量为
    /// `queue_capacity` 的有界通道。线程池不绑定任何文件，可在整个进程内共享。
    ///
    /// 参数非法（线程数或队列容量为 0）返回 [`Error::InvalidPoolConfig`]，不得 panic。
    pub fn new(threads: usize, queue_capacity: usize) -> Result<DiskWriterPool> {
        if threads == 0 {
            return Err(Error::InvalidPoolConfig {
                message: "thread count must be at least 1".to_string(),
            });
        }
        if queue_capacity == 0 {
            return Err(Error::InvalidPoolConfig {
                message: "queue capacity must be at least 1".to_string(),
            });
        }

        let mut senders = Vec::with_capacity(threads);
        let mut handles = Vec::with_capacity(threads);
        for i in 0..threads {
            let (tx, rx) = mpsc::channel::<Request>(queue_capacity);
            senders.push(tx);
            let handle = std::thread::Builder::new()
                .name(format!("idm-disk-{i}"))
                .spawn(move || worker(rx))
                .map_err(|e| Error::Io {
                    operation: "spawn disk writer thread",
                    path: PathBuf::new(),
                    offset: 0,
                    source: e,
                })?;
            handles.push(handle);
        }

        Ok(DiskWriterPool {
            senders,
            handles,
            next_id: AtomicU64::new(0),
        })
    }

    /// 把 `ChunkWriter` 的所有权移交给线程池，返回轻量句柄 `PoolChunk`。
    ///
    /// 调用后该 Chunk 的写入按线程池分配的 `pool_id` 路由到固定线程，由该线程独占持有
    /// 写器。`Register` 携带 oneshot 回执：注册成功后才返回 `PoolChunk`，确保后续写入
    /// 必有对应的写器。
    pub async fn register(&self, chunk_writer: ChunkWriter) -> Result<PoolChunk> {
        let pool_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let chunk_id = chunk_writer.chunk_id();
        let path = chunk_writer.path().to_path_buf();
        let (tx, rx) = oneshot::channel();
        let idx = (pool_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Register {
                pool_id,
                writer: chunk_writer,
                reply: tx,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
        // 等待注册回执，确认写器已就位。
        rx.await.map_err(|_| Error::PoolClosed)??;
        Ok(PoolChunk {
            pool_id,
            chunk_id,
            path,
        })
    }

    /// 异步提交一次写入。通道满时 `send().await` 等待（背压）。区间检查与位置推进
    /// 在落盘线程内由 `ChunkWriter` 完成；越界时返回错误且一个字节都不写。
    pub async fn write(&self, chunk: &PoolChunk, data: Bytes) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let idx = (chunk.pool_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Write {
                pool_id: chunk.pool_id,
                chunk_id: chunk.chunk_id,
                path: chunk.path.clone(),
                data,
                reply: tx,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
        rx.await.map_err(|_| Error::PoolClosed)?
    }

    /// 注销区间并返回最终写入位置（`ChunkWriter` 被 drop 时也会自动注销）。
    ///
    /// Phase 1c 据此记录已落盘进度。
    pub async fn release(&self, chunk: PoolChunk) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        let idx = (chunk.pool_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Release {
                pool_id: chunk.pool_id,
                chunk_id: chunk.chunk_id,
                path: chunk.path.clone(),
                reply: tx,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
        rx.await.map_err(|_| Error::PoolClosed)?
    }

    /// 通过线程池缩小某已注册 Chunk 的尾部（Phase 2 工作窃取需要）。
    pub async fn shrink_end(&self, chunk: &PoolChunk, new_end: u64) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let idx = (chunk.pool_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Shrink {
                pool_id: chunk.pool_id,
                chunk_id: chunk.chunk_id,
                path: chunk.path.clone(),
                new_end,
                reply: tx,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
        rx.await.map_err(|_| Error::PoolClosed)?
    }

    /// 刷盘。内部用 `spawn_blocking` 执行阻塞的 `sync_data`，不阻塞异步线程。返回成功后，
    /// 所有此前已收到回执的写入都保证已落盘。
    pub async fn sync(&self, target: &TargetFile) -> Result<()> {
        let file = target.file();
        let path = target.path().to_path_buf();
        tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                file.sync_data().map_err(|source| Error::Io {
                    operation: "sync_data",
                    path,
                    offset: 0,
                    source,
                })
            }
        })
        .await
        .map_err(|e| Error::Io {
            operation: "spawn_blocking sync_data",
            path,
            offset: 0,
            source: io::Error::other(format!("join failed: {e}")),
        })?
    }

    /// 兜底清理：移除所有落盘线程中属于 `target` 路径的残留写入器，返回移除总数。
    ///
    /// 正常协作式释放流程中，每个分块在退出时都会走 `release` 把自己从线程本地
    /// 登记表移除（写器随之 drop、文件句柄关闭）。本方法仅作为**兜底**——当某个
    /// 分块任务 panic / 泄露、未能走到 `release` 时使用，避免残留的文件句柄导致
    /// 最终重命名失败。每次下载以非 `Completed` 结果结束时调用一次即可。
    pub async fn purge(&self, target: &TargetFile) -> Result<usize> {
        let path = target.path().to_path_buf();
        let mut total = 0usize;
        for s in &self.senders {
            let (tx, rx) = oneshot::channel();
            s.send(Request::Purge {
                path: path.clone(),
                reply: tx,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
            total += rx.await.map_err(|_| Error::PoolClosed)?;
        }
        Ok(total)
    }

    /// 关闭通道并等待所有线程退出。已提交但尚未处理的请求会被处理完（不丢失）；
    /// 之后再提交会返回 [`Error::PoolClosed`]。等待 `join` 在 `spawn_blocking` 中进行，
    /// 不阻塞异步线程；任何线程 panic 都会作为错误返回，不得吞掉。
    pub async fn shutdown(mut self) -> Result<()> {
        // 丢弃所有发送端 → 各通道关闭，线程 draining 后退出。
        drop(std::mem::take(&mut self.senders));
        let handles = std::mem::take(&mut self.handles);
        let panicked = tokio::task::spawn_blocking(move || {
            let mut panicked = 0usize;
            for h in handles {
                if h.join().is_err() {
                    panicked += 1;
                }
            }
            panicked
        })
        .await
        .map_err(|e| Error::Io {
            operation: "spawn_blocking join",
            path: PathBuf::new(),
            offset: 0,
            source: io::Error::other(format!("join task failed: {e}")),
        })?;
        if panicked > 0 {
            return Err(Error::Io {
                operation: "disk writer thread panicked",
                path: PathBuf::new(),
                offset: 0,
                source: io::Error::other(format!("{panicked} disk writer thread(s) panicked")),
            });
        }
        Ok(())
    }
}
