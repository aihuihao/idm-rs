//! 落盘线程池：把异步写请求交给专用阻塞线程，形成背压。
//!
//! 网络层到落盘层之间用有界 `tokio::sync::mpsc` 通道传递缓冲区，保证内存占用
//! 有上限。内存上限 = `线程数 × 队列容量 × 单块最大字节数`。
//!
//! 写请求按 `chunk_id % 线程数` 取模分配到固定线程，保证同一个 Chunk 的写入
//! 顺序处理。通道满时 `send().await` 等待，形成背压。
//!
//! **刷盘语义**：`sync()` 返回成功后，所有在调用 `sync()` 之前已经收到回执的写入
//! （回执在 `seek_write` 完成之后才发出），都保证已经落盘。Phase 1c 只能依据这一
//! 语义记录进度——回执只代表"已交给操作系统"，不代表已落盘。

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::HANDLE;

use crate::disk::win32;
use crate::error::{Error, Result};

/// 单个写请求：定位到 `offset` 写入 `data`，完成后通过 `reply` 回执。
///
/// 回执的含义是"已交给操作系统（seek_write 已返回）"，**不代表已落盘**。
pub struct WriteRequest {
    pub chunk_id: u64,
    pub offset: u64,
    pub data: Bytes,
    pub reply: oneshot::Sender<Result<()>>,
}

/// 落盘线程池。
pub struct DiskWriterPool {
    senders: Vec<mpsc::Sender<WriteRequest>>,
    handles: Vec<std::thread::JoinHandle<()>>,
    file: Arc<File>,
}

impl std::fmt::Debug for DiskWriterPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskWriterPool")
            .field("threads", &self.senders.len())
            .field("handles", &self.handles.len())
            .finish()
    }
}

/// 在线程内把一块数据写到文件的指定偏移（共享句柄上只用 `seek_write`）。
fn write_chunk(file: &File, path: &Path, offset: u64, data: &Bytes) -> Result<()> {
    crate::disk::write::write_all_at(file, data.as_ref(), offset).map_err(|e| {
        if e.kind() == io::ErrorKind::WriteZero {
            Error::WriteZero {
                path: path.to_path_buf(),
                offset,
            }
        } else {
            Error::Io {
                operation: "write",
                path: path.to_path_buf(),
                offset,
                source: e,
            }
        }
    })
}

impl DiskWriterPool {
    /// 创建 `threads` 个名为 `idm-disk-N` 的阻塞线程，每个线程持有一条容量为
    /// `queue_capacity` 的有界通道。
    pub fn new(
        threads: usize,
        queue_capacity: usize,
        file: Arc<File>,
        path: PathBuf,
    ) -> Result<DiskWriterPool> {
        assert!(threads > 0, "disk writer pool needs at least one thread");
        assert!(queue_capacity > 0, "queue capacity must be positive");

        let mut senders = Vec::with_capacity(threads);
        let mut handles = Vec::with_capacity(threads);
        for i in 0..threads {
            let (tx, mut rx) = mpsc::channel::<WriteRequest>(queue_capacity);
            senders.push(tx);

            let file = file.clone();
            let path = path.clone();
            let path_for_err = path.clone();
            let handle = std::thread::Builder::new()
                .name(format!("idm-disk-{i}"))
                .spawn(move || {
                    // 线程内用 blocking_recv 接收请求；通道关闭后 draining 剩余请求再退出。
                    while let Some(req) = rx.blocking_recv() {
                        let result = write_chunk(&file, &path, req.offset, &req.data);
                        // 回执仅在写入完成后发出：这是 sync() 保证语义的基础。
                        let _ = req.reply.send(result);
                    }
                })
                .map_err(|e| Error::Io {
                    operation: "spawn disk writer thread",
                    path: path_for_err,
                    offset: 0,
                    source: e,
                })?;
            handles.push(handle);
        }

        Ok(DiskWriterPool {
            senders,
            handles,
            file,
        })
    }

    /// 异步提交一次写入。通道满时 `send().await` 等待（背压）。按 `chunk_id`
    /// 取模分配到固定线程，保证同一 Chunk 顺序处理。
    pub async fn write(&self, chunk_id: u64, offset: u64, data: Bytes) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let req = WriteRequest {
            chunk_id,
            offset,
            data,
            reply: tx,
        };
        let idx = (chunk_id % self.senders.len() as u64) as usize;
        // 通道已关闭（shutdown 之后）时 send 失败，映射为 PoolClosed。
        self.senders[idx]
            .send(req)
            .await
            .map_err(|_| Error::PoolClosed)?;
        // 等待回执；回执发送方（线程）已退出则视为池已关闭。
        rx.await.map_err(|_| Error::PoolClosed)?
    }

    /// 刷盘。返回成功后，所有此前已收到回执的写入都保证已落盘。
    pub fn sync(&self) -> Result<()> {
        let handle = self.file.as_raw_handle() as HANDLE;
        win32::flush_buffers(handle)
    }

    /// 关闭通道并等待所有线程退出。已提交但尚未处理的请求会被处理完（不丢失）；
    /// 之后再提交会返回 [`Error::PoolClosed`]。
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
