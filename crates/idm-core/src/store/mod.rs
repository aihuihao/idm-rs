//! SQLite 状态持久化（Phase 1c）。
//!
//! `Store` 在**专用线程**中持有唯一的 `rusqlite::Connection`：所有 SQL 都在该线程
//! 内执行，绝不在 Tokio 工作线程上跑。对外提供 async 方法：通过无界 mpsc 发送命令、
//! 用 oneshot 接收结果。连接打开时设置 WAL / synchronous=NORMAL / busy_timeout=5000 /
//! foreign_keys=ON，并按 `PRAGMA user_version` 依次执行迁移（每条迁移一个事务）。
//!
//! 检查点的事务严格实现 ARCHITECTURE.md 9.2 的 `committed = MAX(committed, S)`：
//! 调用方必须先取 acked 快照 S、再 `sync`、成功后才把 S 交给本模块。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result, is_sharing_violation};

// ---------------------------------------------------------------------------
// 迁移：数组下标 i 对应 user_version = i + 1
// ---------------------------------------------------------------------------

/// 迁移脚本数组；当前为版本 2。
pub const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2];

/// 当前 schema 版本。
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// 版本 1：tasks / chunks。
///
/// 按 9.4 与任务第 3 点确认：任务记录可以在 Probe 之前创建，因此除
/// `url`/`dir`/`strategy`/`connections`/`state` 外所有列都允许为 NULL。
const MIGRATION_V1: &str = r#"
CREATE TABLE tasks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    url             TEXT    NOT NULL,
    final_url       TEXT,
    dir             TEXT    NOT NULL,
    file_name       TEXT,
    part_path       TEXT,
    total_size      INTEGER,
    supports_range  INTEGER NOT NULL DEFAULT 0,
    etag            TEXT,
    etag_weak       INTEGER NOT NULL DEFAULT 0,
    last_modified   TEXT,
    strategy        TEXT    NOT NULL,
    connections     INTEGER NOT NULL,
    state           TEXT    NOT NULL,
    pause_reason    TEXT,
    last_error      TEXT,
    resumable       INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL
);
CREATE TABLE chunks (
    task_id      INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    chunk_index  INTEGER NOT NULL,
    start        INTEGER NOT NULL,
    end          INTEGER NOT NULL,
    committed    INTEGER NOT NULL DEFAULT 0,
    state        TEXT    NOT NULL DEFAULT 'Pending',
    PRIMARY KEY (task_id, chunk_index)
);
"#;

/// 版本 2（Phase 2 动态切分）：分块记录增加来源列。
///
/// `parent_index` 为 `NULL` 表示初始划分出的分块；非 `NULL` 表示该分块是从
/// 第 `parent_index` 号分块切出来的（工作窃取的产物）。
const MIGRATION_V2: &str = r#"
ALTER TABLE chunks ADD COLUMN parent_index INTEGER;
"#;

fn db_err(e: rusqlite::Error) -> Error {
    Error::Database {
        message: e.to_string(),
    }
}

/// 毫秒级时间戳。秒级精度不足以让"检查点任务已停止"的断言区分
/// 两次相邻检查点（默认间隔 2 秒，测试中可更短），故统一使用毫秒。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_text() -> String {
    now_millis().to_string()
}

/// 兼容读取 Phase 1c 早期写入的秒级时间戳；统一返回毫秒。
fn parse_timestamp(s: &str) -> u64 {
    let v = s.parse::<u64>().unwrap_or(0);
    if v < 100_000_000_000 {
        v.saturating_mul(1000)
    } else {
        v
    }
}

/// 依次执行缺失的迁移；每条迁移在自己的事务内完成并更新 `user_version`。
fn apply_migrations(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_err)?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = i as i64 + 1;
        if version >= target {
            continue;
        }
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute_batch(sql).map_err(db_err)?;
        tx.pragma_update(None, "user_version", target)
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 公开数据类型
// ---------------------------------------------------------------------------

/// 数据库中的任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStateDb {
    Pending,
    Probing,
    Downloading,
    Finalizing,
    Completed,
    Failed,
    Paused,
}

impl TaskStateDb {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStateDb::Pending => "Pending",
            TaskStateDb::Probing => "Probing",
            TaskStateDb::Downloading => "Downloading",
            TaskStateDb::Finalizing => "Finalizing",
            TaskStateDb::Completed => "Completed",
            TaskStateDb::Failed => "Failed",
            TaskStateDb::Paused => "Paused",
        }
    }

    fn parse(s: &str) -> TaskStateDb {
        match s {
            "Pending" => TaskStateDb::Pending,
            "Probing" => TaskStateDb::Probing,
            "Downloading" => TaskStateDb::Downloading,
            "Finalizing" => TaskStateDb::Finalizing,
            "Completed" => TaskStateDb::Completed,
            "Failed" => TaskStateDb::Failed,
            "Paused" => TaskStateDb::Paused,
            _ => TaskStateDb::Failed,
        }
    }
}

/// 暂停原因（9.2 / D.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReasonDb {
    DiskFull,
    User,
    Interrupted,
}

impl PauseReasonDb {
    pub fn as_str(self) -> &'static str {
        match self {
            PauseReasonDb::DiskFull => "DiskFull",
            PauseReasonDb::User => "User",
            PauseReasonDb::Interrupted => "Interrupted",
        }
    }

    fn parse(s: &str) -> Option<PauseReasonDb> {
        match s {
            "DiskFull" => Some(PauseReasonDb::DiskFull),
            "User" => Some(PauseReasonDb::User),
            "Interrupted" => Some(PauseReasonDb::Interrupted),
            _ => None,
        }
    }
}

/// 创建任务时的必填信息（Probe 之前）。
#[derive(Debug, Clone)]
pub struct NewTask {
    pub url: String,
    pub dir: String,
    pub strategy: String,
    pub connections: usize,
}

/// Probe 成功后回填的元信息。
#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub final_url: String,
    pub file_name: String,
    pub part_path: String,
    pub total_size: Option<u64>,
    pub supports_range: bool,
    pub etag: Option<String>,
    pub etag_weak: bool,
    pub last_modified: Option<String>,
    pub resumable: bool,
}

/// 分块初始记录：`committed` 为绝对字节位置。
#[derive(Debug, Clone, Copy)]
pub struct ChunkRow {
    pub chunk_index: i64,
    pub start: u64,
    pub end: u64,
    pub committed: u64,
    /// 来源：`None` = 初始分块，`Some(i)` = 从第 i 号分块切出。
    pub parent: Option<i64>,
}

/// 分块记录。
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub chunk_index: i64,
    pub start: u64,
    pub end: u64,
    pub committed: u64,
    pub state: String,
    pub parent: Option<i64>,
}

/// 完整任务记录（含分块）。
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: i64,
    pub url: String,
    pub final_url: Option<String>,
    pub dir: String,
    pub file_name: Option<String>,
    pub part_path: Option<String>,
    pub total_size: Option<u64>,
    pub supports_range: bool,
    pub etag: Option<String>,
    pub etag_weak: bool,
    pub last_modified: Option<String>,
    pub strategy: String,
    pub connections: i64,
    pub state: TaskStateDb,
    pub pause_reason: Option<PauseReasonDb>,
    pub last_error: Option<String>,
    pub resumable: bool,
    pub created_at: String,
    pub updated_at: String,
    pub chunks: Vec<ChunkRecord>,
}

impl TaskRecord {
    /// 已提交字节总数 = Σ(committed - start)。
    pub fn committed_bytes(&self) -> u64 {
        self.chunks
            .iter()
            .map(|c| c.committed.saturating_sub(c.start))
            .sum()
    }
}

/// `list` 用的一行摘要。
#[derive(Debug, Clone)]
pub struct TaskSummary {
    pub id: i64,
    pub file_name: Option<String>,
    pub state: TaskStateDb,
    pub pause_reason: Option<PauseReasonDb>,
    pub committed: u64,
    pub total_size: Option<u64>,
    pub last_error: Option<String>,
    pub part_path: Option<String>,
}

/// 启动恢复统计（9.4 / 9.5）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub interrupted: u64,
    pub kept_finalizing: u64,
    pub completed: u64,
    pub failed: u64,
    /// 因 `.idm-part` 被另一个进程占用（错误码 32）而跳过的 Downloading 任务数。
    pub skipped: u64,
    /// 因 Pending / Probing 状态停留超过 5 分钟而改为 Failed 的任务数。
    pub stale_before_download: u64,
}

// ---------------------------------------------------------------------------
// 命令与专用线程
// ---------------------------------------------------------------------------

type Reply<T> = oneshot::Sender<Result<T>>;

enum Command {
    CreateTask(NewTask, Reply<i64>),
    UpdateProbe {
        id: i64,
        info: ProbeInfo,
        reply: Reply<()>,
    },
    SaveChunks {
        id: i64,
        chunks: Vec<ChunkRow>,
        reply: Reply<()>,
    },
    Checkpoint {
        id: i64,
        positions: Vec<(i64, u64)>,
        reply: Reply<()>,
    },
    /// Phase 2 切分事务：同一事务内缩小被切分分块的 end 并插入新分块。
    SplitChunk {
        id: i64,
        victim: i64,
        split: u64,
        new_index: i64,
        reply: Reply<()>,
    },
    UpdateState {
        id: i64,
        state: TaskStateDb,
        pause: Option<PauseReasonDb>,
        last_error: Option<String>,
        reply: Reply<()>,
    },
    SetFinalName {
        id: i64,
        name: String,
        reply: Reply<()>,
    },
    GetTask {
        id: i64,
        reply: Reply<TaskRecord>,
    },
    ListTasks(Reply<Vec<TaskSummary>>),
    DeleteTask {
        id: i64,
        reply: Reply<Option<String>>,
    },
    Recover(Reply<RecoveryReport>),
    Shutdown(Reply<()>),
}

/// 持久化句柄，可 `Arc` 共享；所有方法都是 async。
pub struct Store {
    tx: mpsc::UnboundedSender<Command>,
    fail_checkpoints: Arc<AtomicBool>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// 打开（或创建）数据库：自动建目录、启动专用线程、执行迁移。
    /// 返回时迁移已完成，后续命令可立即使用。
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Store>> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                operation: "create db directory",
                path: parent.to_path_buf(),
                offset: 0,
                source: e,
            })?;
        }
        let (tx, rx) = mpsc::unbounded_channel::<Command>();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let fail_checkpoints = Arc::new(AtomicBool::new(false));
        let fail_for_thread = fail_checkpoints.clone();
        std::thread::Builder::new()
            .name("idm-store".to_string())
            .spawn(move || store_thread(path, rx, ready_tx, fail_for_thread))
            .map_err(|e| Error::Io {
                operation: "spawn store thread",
                path: PathBuf::new(),
                offset: 0,
                source: e,
            })?;
        ready_rx.recv().map_err(|e| Error::Database {
            message: format!("store thread did not report readiness: {e}"),
        })??;
        Ok(Arc::new(Store {
            tx,
            fail_checkpoints,
        }))
    }

    /// 默认路径：`%LOCALAPPDATA%\idm-rs\idm.db`。
    pub fn open_default() -> Result<Arc<Store>> {
        Self::open(default_db_path()?)
    }

    /// 仅供测试：让下一次及后续 `checkpoint` 事务直接失败，用于验证事务原子性。
    pub fn set_fail_checkpoints(&self, fail: bool) {
        self.fail_checkpoints.store(fail, Ordering::SeqCst);
    }

    async fn call<T>(&self, build: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(build(reply)).map_err(|_| Error::Database {
            message: "store thread stopped".to_string(),
        })?;
        rx.await.map_err(|_| Error::Database {
            message: "store reply dropped".to_string(),
        })?
    }

    pub async fn create_task(&self, t: NewTask) -> Result<i64> {
        self.call(|reply| Command::CreateTask(t, reply)).await
    }

    pub async fn update_probe(&self, id: i64, info: ProbeInfo) -> Result<()> {
        self.call(|reply| Command::UpdateProbe { id, info, reply })
            .await
    }

    pub async fn save_chunks(&self, id: i64, chunks: Vec<ChunkRow>) -> Result<()> {
        self.call(|reply| Command::SaveChunks { id, chunks, reply })
            .await
    }

    /// 单个事务写检查点；`positions` 为 (chunk_index, 绝对 acked 位置)。
    /// 只允许增大：`committed = MAX(committed, S)`。
    pub async fn checkpoint(&self, id: i64, positions: &[(i64, u64)]) -> Result<()> {
        let positions = positions.to_vec();
        self.call(|reply| Command::Checkpoint {
            id,
            positions,
            reply,
        })
        .await
    }

    /// Phase 2 切分事务（ARCHITECTURE 10.3 第 5 步）：一个事务内把 `victim`
    /// 分块的 end 改为 `split`，并插入 `[split, 原 end)` 的新分块，`committed = split`。
    /// 事务提交成功后才允许为新区间发起网络请求。
    pub async fn split_chunk(
        &self,
        id: i64,
        victim: i64,
        split: u64,
        new_index: i64,
    ) -> Result<()> {
        self.call(|reply| Command::SplitChunk {
            id,
            victim,
            split,
            new_index,
            reply,
        })
        .await
    }

    pub async fn update_state(
        &self,
        id: i64,
        state: TaskStateDb,
        pause: Option<PauseReasonDb>,
        last_error: Option<String>,
    ) -> Result<()> {
        self.call(|reply| Command::UpdateState {
            id,
            state,
            pause,
            last_error,
            reply,
        })
        .await
    }

    pub async fn set_final_name(&self, id: i64, name: &str) -> Result<()> {
        let name = name.to_string();
        self.call(|reply| Command::SetFinalName { id, name, reply })
            .await
    }

    pub async fn get_task(&self, id: i64) -> Result<TaskRecord> {
        self.call(|reply| Command::GetTask { id, reply }).await
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskSummary>> {
        self.call(Command::ListTasks).await
    }

    /// 删除任务（chunks 级联删除），返回原 `part_path` 供 `--delete-file` 使用。
    pub async fn delete_task(&self, id: i64) -> Result<Option<String>> {
        self.call(|reply| Command::DeleteTask { id, reply }).await
    }

    /// 启动恢复：按 ARCHITECTURE.md 9.4 规则处理 Downloading / Finalizing 任务。
    pub async fn recover(&self) -> Result<RecoveryReport> {
        self.call(Command::Recover).await
    }

    /// 显式关闭专用线程（Drop 时线程也会随通道关闭自行退出）。
    pub async fn shutdown(&self) -> Result<()> {
        self.call(Command::Shutdown).await
    }
}

/// 默认数据库路径：`%LOCALAPPDATA%\idm-rs\idm.db`。
pub fn default_db_path() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidArgument {
            message: "LOCALAPPDATA is not set".to_string(),
        })?;
    Ok(base.join("idm-rs").join("idm.db"))
}

fn open_connection(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path).map_err(db_err)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(db_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(db_err)?;
    conn.busy_timeout(Duration::from_millis(5000))
        .map_err(db_err)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(db_err)?;
    apply_migrations(&mut conn)?;
    Ok(conn)
}

fn store_thread(
    path: PathBuf,
    mut rx: mpsc::UnboundedReceiver<Command>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
    fail: Arc<AtomicBool>,
) {
    let mut conn = match open_connection(&path) {
        Ok(c) => {
            let _ = ready.send(Ok(()));
            c
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    while let Some(cmd) = rx.blocking_recv() {
        let is_shutdown = matches!(cmd, Command::Shutdown(_));
        handle_command(&mut conn, cmd, &fail);
        if is_shutdown {
            break;
        }
    }
}

fn handle_command(conn: &mut Connection, cmd: Command, fail: &AtomicBool) {
    match cmd {
        Command::CreateTask(t, reply) => {
            let _ = reply.send(create_task(conn, t));
        }
        Command::UpdateProbe { id, info, reply } => {
            let _ = reply.send(update_probe(conn, id, info));
        }
        Command::SaveChunks { id, chunks, reply } => {
            let _ = reply.send(save_chunks(conn, id, chunks));
        }
        Command::Checkpoint {
            id,
            positions,
            reply,
        } => {
            let res = if fail.load(Ordering::SeqCst) {
                Err(Error::Database {
                    message: "injected checkpoint transaction failure".to_string(),
                })
            } else {
                checkpoint(conn, id, &positions)
            };
            let _ = reply.send(res);
        }
        Command::SplitChunk {
            id,
            victim,
            split,
            new_index,
            reply,
        } => {
            let _ = reply.send(split_chunk(conn, id, victim, split, new_index));
        }
        Command::UpdateState {
            id,
            state,
            pause,
            last_error,
            reply,
        } => {
            let _ = reply.send(update_state(conn, id, state, pause, last_error));
        }
        Command::SetFinalName { id, name, reply } => {
            let _ = reply.send(set_final_name(conn, id, &name));
        }
        Command::GetTask { id, reply } => {
            let _ = reply.send(get_task(conn, id));
        }
        Command::ListTasks(reply) => {
            let _ = reply.send(list_tasks(conn));
        }
        Command::DeleteTask { id, reply } => {
            let _ = reply.send(delete_task(conn, id));
        }
        Command::Recover(reply) => {
            let _ = reply.send(recover(conn));
        }
        Command::Shutdown(reply) => {
            let _ = reply.send(Ok(()));
        }
    }
}

// ---------------------------------------------------------------------------
// SQL（全部只在 store_thread 内执行）
// ---------------------------------------------------------------------------

fn create_task(conn: &Connection, t: NewTask) -> Result<i64> {
    let now = now_text();
    conn.execute(
        "INSERT INTO tasks (url, dir, strategy, connections, state, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            t.url,
            t.dir,
            t.strategy,
            t.connections as i64,
            TaskStateDb::Pending.as_str(),
            now
        ],
    )
    .map_err(db_err)?;
    Ok(conn.last_insert_rowid())
}

fn update_probe(conn: &Connection, id: i64, p: ProbeInfo) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE tasks SET final_url=?2, file_name=?3, part_path=?4, total_size=?5, \
             supports_range=?6, etag=?7, etag_weak=?8, last_modified=?9, resumable=?10, \
             state=?11, last_error=NULL, pause_reason=NULL, updated_at=?12 WHERE id=?1",
            params![
                id,
                p.final_url,
                p.file_name,
                p.part_path,
                p.total_size.map(|v| v as i64),
                p.supports_range as i64,
                p.etag,
                p.etag_weak as i64,
                p.last_modified,
                p.resumable as i64,
                TaskStateDb::Downloading.as_str(),
                now_text()
            ],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(Error::TaskNotFound { id });
    }
    Ok(())
}

fn save_chunks(conn: &mut Connection, id: i64, chunks: Vec<ChunkRow>) -> Result<()> {
    let tx = conn.transaction().map_err(db_err)?;
    tx.execute("DELETE FROM chunks WHERE task_id=?1", params![id])
        .map_err(db_err)?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO chunks (task_id, chunk_index, start, end, committed, state, parent_index) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'Pending', ?6)",
            )
            .map_err(db_err)?;
        for c in &chunks {
            stmt.execute(params![
                id,
                c.chunk_index,
                c.start as i64,
                c.end as i64,
                c.committed as i64,
                c.parent
            ])
            .map_err(db_err)?;
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(())
}

/// 9.2 第三步：一个事务内把快照 S 写进 committed，只允许增大。
fn checkpoint(conn: &mut Connection, id: i64, positions: &[(i64, u64)]) -> Result<()> {
    let tx = conn.transaction().map_err(db_err)?;
    {
        let mut stmt = tx
            .prepare(
                "UPDATE chunks SET committed = CASE WHEN committed > ?3 THEN committed ELSE ?3 END \
                 WHERE task_id=?1 AND chunk_index=?2",
            )
            .map_err(db_err)?;
        for (idx, pos) in positions {
            stmt.execute(params![id, *idx, *pos as i64])
                .map_err(db_err)?;
        }
    }
    tx.execute(
        "UPDATE tasks SET updated_at=?2 WHERE id=?1",
        params![id, now_text()],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(())
}

/// Phase 2 切分事务（10.3 第 5 步）：同一事务内缩小被切分分块并插入新分块。
///
/// 事务提交成功之前不得发起任何网络请求。被切分分块的 `committed` 必须 ≤ `split`，
/// 否则说明调用方违反了 `committed ≤ end` 不变式，直接拒绝整个事务。
fn split_chunk(
    conn: &mut Connection,
    id: i64,
    victim: i64,
    split: u64,
    new_index: i64,
) -> Result<()> {
    let tx = conn.transaction().map_err(db_err)?;
    let (start, old_end, committed): (i64, i64, i64) = tx
        .query_row(
            "SELECT start, end, committed FROM chunks WHERE task_id=?1 AND chunk_index=?2",
            params![id, victim],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(db_err)?
        .ok_or(Error::TaskNotFound { id })?;
    let (start, old_end, committed) = (start as u64, old_end as u64, committed as u64);
    if committed > split || split >= old_end || split < start {
        return Err(Error::InvalidArgument {
            message: format!(
                "invalid split for chunk {victim}: split={split}, range=[{start}, {old_end}), committed={committed}"
            ),
        });
    }
    tx.execute(
        "UPDATE chunks SET end=?3 WHERE task_id=?1 AND chunk_index=?2",
        params![id, victim, split as i64],
    )
    .map_err(db_err)?;
    tx.execute(
        "INSERT INTO chunks (task_id, chunk_index, start, end, committed, state, parent_index) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'Pending', ?6)",
        params![
            id,
            new_index,
            split as i64,
            old_end as i64,
            split as i64,
            victim
        ],
    )
    .map_err(db_err)?;
    tx.execute(
        "UPDATE tasks SET updated_at=?2 WHERE id=?1",
        params![id, now_text()],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(())
}

fn update_state(
    conn: &Connection,
    id: i64,
    state: TaskStateDb,
    pause: Option<PauseReasonDb>,
    last_error: Option<String>,
) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE tasks SET state=?2, pause_reason=?3, last_error=?4, updated_at=?5 WHERE id=?1",
            params![
                id,
                state.as_str(),
                pause.map(|p| p.as_str()),
                last_error,
                now_text()
            ],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(Error::TaskNotFound { id });
    }
    Ok(())
}

fn set_final_name(conn: &Connection, id: i64, name: &str) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE tasks SET file_name=?2, state=?3, pause_reason=NULL, last_error=NULL, \
             updated_at=?4 WHERE id=?1",
            params![id, name, TaskStateDb::Completed.as_str(), now_text()],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(Error::TaskNotFound { id });
    }
    Ok(())
}

fn query_chunks(conn: &Connection, id: i64) -> Result<Vec<ChunkRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT chunk_index, start, end, committed, state, parent_index FROM chunks \
             WHERE task_id=?1 ORDER BY chunk_index",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![id], |row| {
            Ok(ChunkRecord {
                chunk_index: row.get(0)?,
                start: row.get::<_, i64>(1)? as u64,
                end: row.get::<_, i64>(2)? as u64,
                committed: row.get::<_, i64>(3)? as u64,
                state: row.get(4)?,
                parent: row.get(5)?,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    Ok(rows)
}

fn get_task(conn: &Connection, id: i64) -> Result<TaskRecord> {
    let mut stmt = conn
        .prepare(
            "SELECT id, url, final_url, dir, file_name, part_path, total_size, supports_range, \
             etag, etag_weak, last_modified, strategy, connections, state, pause_reason, \
             last_error, resumable, created_at, updated_at FROM tasks WHERE id=?1",
        )
        .map_err(db_err)?;
    let rec = stmt
        .query_row(params![id], |row| {
            let state: String = row.get(13)?;
            let pause: Option<String> = row.get(14)?;
            Ok(TaskRecord {
                id: row.get(0)?,
                url: row.get(1)?,
                final_url: row.get(2)?,
                dir: row.get(3)?,
                file_name: row.get(4)?,
                part_path: row.get(5)?,
                total_size: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                supports_range: row.get::<_, i64>(7)? != 0,
                etag: row.get(8)?,
                etag_weak: row.get::<_, i64>(9)? != 0,
                last_modified: row.get(10)?,
                strategy: row.get(11)?,
                connections: row.get(12)?,
                state: TaskStateDb::parse(&state),
                pause_reason: pause.as_deref().and_then(PauseReasonDb::parse),
                last_error: row.get(15)?,
                resumable: row.get::<_, i64>(16)? != 0,
                created_at: row.get(17)?,
                updated_at: row.get(18)?,
                chunks: Vec::new(),
            })
        })
        .optional()
        .map_err(db_err)?
        .ok_or(Error::TaskNotFound { id })?;
    let chunks = query_chunks(conn, id)?;
    Ok(TaskRecord { chunks, ..rec })
}

fn list_tasks(conn: &Connection) -> Result<Vec<TaskSummary>> {
    let mut stmt = conn
        .prepare(
            "SELECT t.id, t.file_name, t.state, t.pause_reason, t.last_error, t.total_size, \
             t.part_path, COALESCE((SELECT SUM(c.committed - c.start) FROM chunks c \
             WHERE c.task_id = t.id), 0) FROM tasks t ORDER BY t.id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            let state: String = row.get(2)?;
            let pause: Option<String> = row.get(3)?;
            Ok(TaskSummary {
                id: row.get(0)?,
                file_name: row.get(1)?,
                state: TaskStateDb::parse(&state),
                pause_reason: pause.as_deref().and_then(PauseReasonDb::parse),
                last_error: row.get(4)?,
                total_size: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                part_path: row.get(6)?,
                committed: row.get::<_, i64>(7)? as u64,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?;
    Ok(rows)
}

fn delete_task(conn: &Connection, id: i64) -> Result<Option<String>> {
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT part_path FROM tasks WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let Some(part) = row else {
        return Err(Error::TaskNotFound { id });
    };
    conn.execute("DELETE FROM tasks WHERE id=?1", params![id])
        .map_err(db_err)?;
    Ok(part)
}

/// 启动恢复时从 Finalizing 任务读出的一行。
struct FinalizingRow {
    id: i64,
    part_path: Option<String>,
    file_name: Option<String>,
    total_size: Option<i64>,
    dir: String,
}

/// 启动恢复（9.4）：
/// - Downloading → Paused(Interrupted)
/// - Finalizing：.idm-part 存在 → 保持；否则最终文件存在且大小正确 → Completed；其余 → Failed
fn recover(conn: &Connection) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    let now = now_millis();

    // 第四部分 2：Pending / Probing 且停留超过 5 分钟 → Failed 并记录原因。
    // Probe 的总超时只有 60 秒，正在运行的进程不可能在这两个状态停留 5 分钟。
    struct StaleRow {
        id: i64,
        updated_at: String,
    }
    let stale_rows: Vec<StaleRow> = {
        let mut stmt = conn
            .prepare("SELECT id, updated_at FROM tasks WHERE state IN ('Pending','Probing')")
            .map_err(db_err)?;
        stmt.query_map([], |r| {
            Ok(StaleRow {
                id: r.get(0)?,
                updated_at: r.get(1)?,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?
    };
    for row in stale_rows {
        if now.saturating_sub(parse_timestamp(&row.updated_at)) > 5 * 60 * 1000 {
            conn.execute(
                "UPDATE tasks SET state=?2, last_error=?3, updated_at=?4 WHERE id=?1",
                params![
                    row.id,
                    TaskStateDb::Failed.as_str(),
                    "interrupted before download started",
                    now_text()
                ],
            )
            .map_err(db_err)?;
            report.stale_before_download += 1;
        }
    }

    // 9.5：Downloading 任务先探测 .idm-part 是否被另一个进程占用。
    // 只有"无人占用"的任务才改为 Paused(Interrupted)。
    struct DownloadingRow {
        id: i64,
        part_path: Option<String>,
    }
    let downloading: Vec<DownloadingRow> = {
        let mut stmt = conn
            .prepare("SELECT id, part_path FROM tasks WHERE state='Downloading'")
            .map_err(db_err)?;
        stmt.query_map([], |r| {
            Ok(DownloadingRow {
                id: r.get(0)?,
                part_path: r.get(1)?,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?
    };
    for row in downloading {
        // 以读写方式打开：若另一个进程持有 FILE_SHARE_READ 句柄，Windows 返回
        // ERROR_SHARING_VIOLATION(32)，说明它正在下载，跳过且状态不变。
        let in_use = match row.part_path.as_deref() {
            Some(p) if !p.is_empty() => match OpenOptions::new().read(true).write(true).open(p) {
                Ok(f) => {
                    drop(f); // 打开成功后立即关闭
                    false
                }
                Err(e) => is_sharing_violation(&e),
            },
            _ => false,
        };
        if in_use {
            report.skipped += 1;
            continue;
        }
        conn.execute(
            "UPDATE tasks SET state=?2, pause_reason=?3, updated_at=?4 WHERE id=?1",
            params![
                row.id,
                TaskStateDb::Paused.as_str(),
                PauseReasonDb::Interrupted.as_str(),
                now_text()
            ],
        )
        .map_err(db_err)?;
        report.interrupted += 1;
    }

    let rows: Vec<FinalizingRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, part_path, file_name, total_size, dir FROM tasks \
                 WHERE state='Finalizing'",
            )
            .map_err(db_err)?;
        stmt.query_map([], |r| {
            Ok(FinalizingRow {
                id: r.get(0)?,
                part_path: r.get(1)?,
                file_name: r.get(2)?,
                total_size: r.get(3)?,
                dir: r.get(4)?,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)?
    };

    for row in rows {
        let id = row.id;
        let part_exists = row
            .part_path
            .as_deref()
            .is_some_and(|p| Path::new(p).exists());
        if part_exists {
            report.kept_finalizing += 1;
            continue;
        }
        let final_ok = match (row.file_name.as_deref(), row.total_size) {
            (Some(n), Some(t)) => {
                std::fs::metadata(Path::new(&row.dir).join(n)).is_ok_and(|m| m.len() == t as u64)
            }
            _ => false,
        };
        if final_ok {
            conn.execute(
                "UPDATE tasks SET state=?2, pause_reason=NULL, last_error=NULL, updated_at=?3 \
                 WHERE id=?1",
                params![id, TaskStateDb::Completed.as_str(), now_text()],
            )
            .map_err(db_err)?;
            report.completed += 1;
        } else {
            conn.execute(
                "UPDATE tasks SET state=?2, pause_reason=NULL, last_error=?3, updated_at=?4 \
                 WHERE id=?1",
                params![
                    id,
                    TaskStateDb::Failed.as_str(),
                    "finalizing recovery: .idm-part missing and final file missing or size mismatch",
                    now_text()
                ],
            )
            .map_err(db_err)?;
            report.failed += 1;
        }
    }
    Ok(report)
}
