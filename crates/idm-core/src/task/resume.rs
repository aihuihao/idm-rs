//! 断点续传（D.4 / 9.4）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::Url;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::disk::DiskWriterPool;
use crate::disk::win32;
use crate::error::{Error, Result, is_sharing_violation};
use crate::net::client::DownloadConfig;
use crate::net::filename::resolve_conflict;
use crate::net::probe::Probe;
use crate::store::{Store, TaskRecord, TaskStateDb};
use crate::task::cancel::CancelSignal;
use crate::task::download::download_with_store;
use crate::task::download::{
    DownloadContext, DownloadOptions, DownloadOutcome, ResumeChunk, ResumePlan,
};
use crate::task::progress::ProgressSnapshot;

fn resume_err(message: impl Into<String>) -> Error {
    Error::Resume {
        message: message.into(),
    }
}

/// 续传指定任务。
///
/// - `Paused` / `Failed`：普通续传（或按条件从头开始）。
/// - `Finalizing`：跳过下载，先校验再走 9.4 的剩余步骤。
/// - `Downloading` / `Completed` / `Pending`：拒绝续传。
pub async fn resume(
    task_id: i64,
    store: Arc<Store>,
    pool: Arc<DiskWriterPool>,
    opts: &DownloadOptions,
    cancel: Option<&CancelSignal>,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    let rec = store.get_task(task_id).await?;

    // 9.5：无论任务当前处于什么状态，只要它记录的 .idm-part 正被另一个进程
    // 以可写方式占用（错误码 32），就返回 TaskInUse。此判断必须早于状态检查，
    // 否则"正在下载"的 Downloading 任务会先被普通的状态拒绝挡住。
    // 这里没有执行任何数据库写入，因此状态保持不变。
    if let Some(p) = rec.part_path.as_deref() {
        match std::fs::OpenOptions::new().read(true).write(true).open(p) {
            Ok(f) => drop(f), // 打开成功后立即关闭
            Err(e) if is_sharing_violation(&e) => {
                return Err(Error::TaskInUse { id: task_id });
            }
            Err(_) => {} // 文件不存在或其他错误，交给后续流程如实报告
        }
    }

    match rec.state {
        TaskStateDb::Finalizing => return finish_finalizing(&rec, &store).await,
        // `Queued` 也允许续传：调度器把暂停/失败的任务重新入队后状态为 `Queued`，
        // 但其 `.idm-part` 仍存在，需要走续传路径而不是从头重新下载。
        TaskStateDb::Paused | TaskStateDb::Failed | TaskStateDb::Queued => {}
        other => {
            return Err(resume_err(format!(
                "task {task_id} is in state {other:?}, which cannot be resumed"
            )));
        }
    }

    // 还没有 part_path 的 Failed 任务（Probe 阶段失败）：重新执行完整流程。
    let Some(part_path) = rec.part_path.clone() else {
        info!(
            task_id,
            "no .idm-part recorded; restarting Probe and full download"
        );
        let ctx = DownloadContext::new(store, task_id);
        return download_with_store(&rec.url, pool, opts, cancel, Some(ctx)).await;
    };
    let part = PathBuf::from(&part_path);

    // 不可续传（总长未知或不支持 Range）：明确说明后删除 .idm-part，从头下载。
    let total = rec.total_size;
    if total.is_none() || !rec.supports_range || !rec.resumable {
        info!(
            task_id,
            "task is not resumable; deleting .idm-part and downloading from scratch"
        );
        let _ = std::fs::remove_file(&part);
        let ctx = DownloadContext::new(store, task_id);
        return download_with_store(&rec.url, pool, opts, cancel, Some(ctx)).await;
    }
    let total = total.unwrap_or(0);

    // .idm-part 必须存在且长度等于 total_size；失败时任务状态保持不变。
    let actual = std::fs::metadata(&part)
        .map(|m| m.len())
        .map_err(|e| resume_err(format!("cannot open .idm-part {}: {e}", part.display())))?;
    if actual != total {
        return Err(resume_err(format!(
            ".idm-part length mismatch on {}: expected {total}, actual {actual}",
            part.display()
        )));
    }

    // 重新执行 Probe，比较总长度、ETag、Last-Modified。
    let raw_url = rec.final_url.as_deref().unwrap_or(&rec.url);
    let url = Url::parse(raw_url).map_err(|e| resume_err(format!("stored URL is invalid: {e}")))?;
    DownloadConfig::check_scheme(&url)?;
    let client = opts.config.build_client()?;
    let probe = Probe::new(&client, &opts.config)
        .probe(&url)
        .await
        .map_err(|e| resume_err(format!("re-probe failed: {e}")))?;

    let db_etag = rec.etag.as_deref().map(|t| (t, rec.etag_weak));
    let new_etag = probe.etag.as_ref().map(|(t, w)| (t.as_str(), *w));
    let remote_changed = probe.total_size != Some(total)
        || new_etag != db_etag
        || probe.last_modified.as_deref() != rec.last_modified.as_deref();
    if remote_changed {
        // 任务改为 Failed，.idm-part 保留。
        store
            .update_state(
                task_id,
                TaskStateDb::Failed,
                None,
                Some("remote changed during resume".to_string()),
            )
            .await?;
        return Err(Error::RemoteChanged);
    }

    let file_name = rec
        .file_name
        .clone()
        .ok_or_else(|| resume_err(format!("task {task_id} has no recorded file name")))?;
    let final_path = PathBuf::from(&rec.dir).join(&file_name);
    let plan = ResumePlan {
        final_path,
        tmp_path: part,
        file_name,
        final_url: probe.final_url.clone(),
        total_size: total,
        etag: probe.etag.clone(),
        last_modified: probe.last_modified.clone(),
        // 读取**全部**分块记录（含 Phase 2 切分出来的分块与来源），
        // `committed < end` 的由下载流程继续下载，并且仍可再切分。
        chunks: rec
            .chunks
            .iter()
            .map(|c| ResumeChunk {
                index: c.chunk_index,
                start: c.start,
                end: c.end,
                committed: c.committed,
                parent: c.parent,
            })
            .collect(),
    };
    let mut ctx = DownloadContext::new(store, task_id);
    ctx.resume = Some(plan);
    download_with_store(&rec.url, pool, opts, cancel, Some(ctx)).await
}

/// 9.4：Finalizing 任务跳过下载，直接完成剩余步骤。
///
/// 先校验所有分块 `committed == end` 且 `.idm-part` 长度等于 `total_size`；
/// 任何一项不满足都返回明确错误，任务状态保持不变。
async fn finish_finalizing(
    rec: &TaskRecord,
    store: &Store,
) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)> {
    let total = rec
        .total_size
        .ok_or_else(|| resume_err(format!("finalizing task {} has no total_size", rec.id)))?;
    let part = rec
        .part_path
        .as_deref()
        .ok_or_else(|| resume_err(format!("finalizing task {} has no .idm-part", rec.id)))?;
    let actual = std::fs::metadata(part)
        .map(|m| m.len())
        .map_err(|e| resume_err(format!("cannot open .idm-part {part}: {e}")))?;
    if actual != total {
        return Err(resume_err(format!(
            "finalizing task {}: .idm-part length expected {total}, actual {actual}",
            rec.id
        )));
    }
    if rec.chunks.is_empty() || rec.chunks.iter().any(|c| c.committed != c.end) {
        return Err(resume_err(format!(
            "finalizing task {}: not all chunks are fully committed",
            rec.id
        )));
    }
    let file_name = rec
        .file_name
        .as_deref()
        .ok_or_else(|| resume_err(format!("finalizing task {} has no file name", rec.id)))?;
    let base = PathBuf::from(&rec.dir);
    let mut final_path = base.join(file_name);
    if final_path.exists() {
        // 与正常下载流程保持一致：绝不覆盖，另选一个不冲突的名字。
        final_path = resolve_conflict(&base, file_name);
        warn!(
            task_id = rec.id,
            path = ?final_path,
            "finalizing: final file name already taken; chose a non-conflicting name"
        );
    }

    // 本函数没有打开任何文件句柄，因此"释放句柄"一步无需操作。
    // 9.4 剩余顺序：不覆盖地重命名 → 记录最终文件名并改为 Completed。
    win32::move_file(Path::new(part), &final_path).map_err(|e| Error::Io {
        operation: "rename (finalizing resume)",
        path: final_path.clone(),
        offset: 0,
        source: std::io::Error::other(format!("{e}")),
    })?;
    let final_name = final_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    store.set_final_name(rec.id, &final_name).await?;
    info!(task_id = rec.id, "finalizing task completed by resume");
    let (_tx, rx) = watch::channel(ProgressSnapshot::default());
    Ok((DownloadOutcome::Completed, rx))
}
