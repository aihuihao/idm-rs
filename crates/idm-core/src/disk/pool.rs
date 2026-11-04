//! 落盘线程池：把异步写请求交给专用阻塞线程，形成背压。
//!
//! 一个进程内共享一个 `DiskWriterPool`，服务所有目标文件。调用方先用
//! `TargetFile::allocate_chunk` 取得 `ChunkWriter`，再通过 `register` 把所有权移交给
//! 线程池，得到一个可克隆的轻量句柄 `PoolChunk`。线程池按 `chunk_id` 取模分配到固定
//! 线程，该线程独占持有这个 `ChunkWriter`，因此 `ChunkWriter::write` 只需 `&mut self`，
//! 无需任何锁。
//!
//! 写入：通过 `write` 提交 `Bytes`，区间检查与位置推进都在落盘线程内由 `ChunkWriter`
//! 完成；越界时通过回执返回错误且一个字节都不写。
//!
//! 刷盘语义：`sync` 返回成功后，所有在调用 `sync` 之前已经收到回执的写入都保证已落盘。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::disk::TargetFile;
use crate::disk::write::ChunkWriter;
use crate::error::{Error, Result};

/// 轻量句柄：可 `Clone`，仅含 `chunk_id` 与文件路径，用于向线程池路由写入请求。
#[derive(Clone)]
pub struct PoolChunk {
    chunk_id: u64,
    path: PathBuf,
}

/// 线程池内部请求。
enum Request {
    /// 把 `ChunkWriter` 的所有权移交给其固定线程。
    Register { chunk_id: u64, writer: ChunkWriter },
    /// 写入一块数据（位置由 `ChunkWriter` 内部维护）。
    Write {
        chunk_id: u64,
        path: PathBuf,
        data: Bytes,
        reply: oneshot::Sender<Result<()>>,
    },
    /// 注销区间并返回最终写入位置。
    Release {
        chunk_id: u64,
        path: PathBuf,
        reply: oneshot::Sender<Result<u64>>,
    },
    /// 缩小区间尾部。
    Shrink {
        chunk_id: u64,
        path: PathBuf,
        new_end: u64,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// 落盘线程池。
pub struct DiskWriterPool {
    senders: Vec<mpsc::Sender<Request>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for DiskWriterPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskWriterPool")
            .field("threads", &self.senders.len())
            .field("handles", &self.handles.len())
            .finish()
    }
}

/// 单线程内处理请求：按 `chunk_id` 路由到本线程持有的 `ChunkWriter`。
fn worker(mut rx: mpsc::Receiver<Request>) {
    let mut writers: HashMap<u64, ChunkWriter> = HashMap::new();
    while let Some(req) = rx.blocking_recv() {
        match req {
            Request::Register { chunk_id, writer } => {
                writers.insert(chunk_id, writer);
            }
            Request::Write {
                chunk_id,
                path,
                data,
                reply,
            } => {
                let res = match writers.get_mut(&chunk_id) {
                    Some(w) => w.write(&data).map(|_| ()),
                    None => Err(unregistered(chunk_id, &path)),
                };
                let _ = reply.send(res);
            }
            Request::Release {
                chunk_id,
                path,
                reply,
            } => {
                let res = match writers.remove(&chunk_id) {
                    Some(w) => Ok(w.position()),
                    None => Err(unregistered(chunk_id, &path)),
                };
                let _ = reply.send(res);
            }
            Request::Shrink {
                chunk_id,
                path,
                new_end,
                reply,
            } => {
                let res = match writers.get_mut(&chunk_id) {
                    Some(w) => w.shrink_end(new_end),
                    None => Err(unregistered(chunk_id, &path)),
                };
                let _ = reply.send(res);
            }
        }
    }
}

fn unregistered(chunk_id: u64, path: &Path) -> Error {
    Error::Io {
        operation: "write to unregistered/already-released chunk",
        path: path.to_path_buf(),
        offset: chunk_id,
        source: io::Error::other("chunk not registered in disk writer pool"),
    }
}

impl DiskWriterPool {
    /// 创建 `threads` 个名为 `idm-disk-N` 的阻塞线程，每个线程持有一条容量为
    /// `queue_capacity` 的有界通道。线程池不绑定任何文件，可在整个进程内共享。
    pub fn new(threads: usize, queue_capacity: usize) -> Result<DiskWriterPool> {
        assert!(threads > 0, "disk writer pool needs at least one thread");
        assert!(queue_capacity > 0, "queue capacity must be positive");

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

        Ok(DiskWriterPool { senders, handles })
    }

    /// 把 `ChunkWriter` 的所有权移交给线程池，返回轻量句柄 `PoolChunk`。
    ///
    /// 调用后该 Chunk 的写入按 `chunk_id` 路由到固定线程，由该线程独占持有写器。
    pub async fn register(&self, chunk_writer: ChunkWriter) -> Result<PoolChunk> {
        let chunk_id = chunk_writer.chunk_id();
        let path = chunk_writer.path().to_path_buf();
        let idx = (chunk_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Register {
                chunk_id,
                writer: chunk_writer,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
        Ok(PoolChunk { chunk_id, path })
    }

    /// 异步提交一次写入。通道满时 `send().await` 等待（背压）。区间检查与位置推进
    /// 在落盘线程内由 `ChunkWriter` 完成；越界时返回错误且一个字节都不写。
    pub async fn write(&self, chunk: &PoolChunk, data: Bytes) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let idx = (chunk.chunk_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Write {
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
        let idx = (chunk.chunk_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Release {
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
        let idx = (chunk.chunk_id % self.senders.len() as u64) as usize;
        self.senders[idx]
            .send(Request::Shrink {
                chunk_id: chunk.chunk_id,
                path: chunk.path.clone(),
                new_end,
                reply: tx,
            })
            .await
            .map_err(|_| Error::PoolClosed)?;
        rx.await.map_err(|_| Error::PoolClosed)?
    }

    /// 刷盘。返回成功后，所有此前已收到回执的写入都保证已落盘。
    pub fn sync(&self, target: &TargetFile) -> Result<()> {
        target.file().sync_data().map_err(|source| Error::Io {
            operation: "sync_data",
            path: target.path().to_path_buf(),
            offset: 0,
            source,
        })
    }

    /// 关闭通道并等待所有线程退出。已提交但尚未处理的请求会被处理完（不丢失）；
    /// 之后再提交会返回 `Error::PoolClosed`。
    pub async fn shutdown(self) -> Result<()> {
        let mut this = self;
        // 丢弃所有发送端 → 各通道关闭，线程 draining 后退出。
        drop(std::mem::take(&mut this.senders));
        for h in this.handles.drain(..) {
            let _ = h.join();
        }
        Ok(())
    }
}
