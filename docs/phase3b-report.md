# Phase 3b 提交前自查表与报告

## 一、A 部分（限速器重写，commit `c594457`）已完成测试

| 编号 | 要求摘要 | 测试名 | 单独连续运行 10 次结果 |
|---|---|---|---|
| A1 | 动态限速下降时钳制突增、唤醒等待者 | `dynamic_rate_decrease_clamps_burst` | 10/10 通过 |
| A2 | 取消的 `acquire` 退回已领走的令牌 | `cancelled_acquire_refunds_partial_tokens` | 10/10 通过 |
| A3 | 并发 `acquire` 交错放行，首个等待者不被拖到末尾 | `staggered_concurrent_acquires_do_not_stall_first_waiter` | 10/10 通过 |

## 二、B 部分（多任务下载队列与调度器）测试

| 编号 | 要求摘要 | 测试名 | 单独连续运行 10 次结果 |
|---|---|---|---|
| B1 | Schema V3 迁移 + 启动恢复（`Queued` 保持、`Downloading`→`Paused(Interrupted)`） | `migration_v2_to_v3_and_recover_preserves_queued` | 10/10 通过 |
| B2 | 并发上限与 FIFO 自动接续 | `queue_respects_max_concurrent_tasks_and_fifo_order` | 10/10 通过 |
| B3 | 暂停活动任务自动拉起排队任务 + 断点续传 | `pause_active_task_starts_next_queued_and_resume_completes` | 10/10 通过 |
| B4 | 队列重排（优先级调整） | `queue_reordering_changes_execution_order` | 10/10 通过 |
| B5 | 运行中动态调整最大并发数 | `dynamic_max_concurrent_tasks_scales_up_and_down` | 10/10 通过 |
| B6 | 队列共享全局限速与 Host 连接限制 | `queue_shares_global_limiter_and_host_limiter` | 10/10 通过 |
| B7 | CLI 队列子命令端到端（enqueue/move/run） | `cli_enqueue_move_and_queue_run` | 10/10 通过 |
| B8 | 回归：全量测试 + Phase 2 的 50 种子并发 + 两个 20 轮崩溃测试 | `cargo test --workspace` 全绿；`crash_resume` 两个用例各 20 轮通过 | 10/10 通过（每轮 verify-workspace 均 exit 0） |

## 三、第八部分报告

### 1. 提交哈希
- A 部分提交：`c594457`（`phase3b-pre: fix ratelimiter concurrent debt and starvation`）。
- B 部分提交：本提交（`phase3b: multi-task download queue and scheduler`）；哈希以 `git log` 为准。

### 2. 完整自查表
见上文第一、二节（共 11 项，每项均 10/10 通过）。

### 3. B 部分数据库 V3 表结构 / 队列调度器核心方法代码位置

**V3 迁移（Schema）** — `crates/idm-core/src/store/mod.rs:78`

```sql
ALTER TABLE tasks ADD COLUMN queue_order INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN max_speed INTEGER;
UPDATE tasks SET queue_order = id;
```

- 入队：`crates/idm-core/src/store/mod.rs:585` `enqueue_task`（SQL 实现 `:1057`）
- 列出排队任务（按 `queue_order ASC, id ASC`）：`crates/idm-core/src/store/mod.rs:590` `list_queued_tasks`（SQL 实现 `:1077`）
- 重新入队（Paused/Failed/Finalizing → Queued）：`crates/idm-core/src/store/mod.rs:596` `requeue_task`（SQL 实现 `:1121`）
- 队列内移动：`:606` `move_in_queue`（SQL 实现 `:1163`）

**调度器核心方法** — `crates/idm-core/src/queue/mod.rs`
- 事件枚举 `QueueEvent`：`:71`
- 队列句柄 `DownloadQueue`：`:120`
- 构造 `DownloadQueue::new`：`:126`
- 入队 `enqueue`：`:160`
- 暂停 `pause`：`:181`
- 恢复入队 `resume_queued`：`:234`
- 队列移动 `move_queue`：`:242`
- 等待空闲 `wait_for_idle`：`:281`
- 关闭 `shutdown`：`:295`
- 调度主循环 `run_scheduler`：`:313`
- 尝试调度 `try_schedule`：`:327`
- 拉起单任务 `spawn_task`：`:353`
- 任务结束回调 `on_finished`：`:398`
- 未完成计数 `bump_outstanding`：`:463`

### 4. B5 第 2–7 项实测关键指标
- **B2**：并发下载任务数峰值严格 `== 2`；任务启动顺序严格 `1→2→3→4→5`；5/5 文件逐字节校验通过。
- **B3**：暂停后 A 状态 `== Paused(User)`；B 在 A 暂停后立即被拉起并完成；A 续传时服务器观察到的 `Range` 首字节 `> 0`（确认断点续传而非重下），A 最终完成且文件正确。
- **B4**：任务 1 完成后后续启动顺序严格 `1→4→3→2`（Top 把 4 提到最前，Down 把 2 后移）。
- **B5**：`set_max_concurrent_tasks(3)` 后 100ms 内活动任务数升至 3；`set_max_concurrent_tasks(1)` 后已在运行的 3 个任务均不被强杀，全部正常完成。
- **B6**：服务器观测到的最大并发连接数 `<= 5`（`max_connections_per_host=5`）；总耗时约 3.0s（落在 2.5s~4.0s 区间）；3/3 文件逐字节校验通过。
- **B7**：`enqueue`×3 → `queue move` → `queue run --max-tasks 2`；子进程退出码 0；3/3 文件逐字节校验通过。

### 5. 测试总数与验收脚本输出
- 新增 B5 集成测试 7 个（覆盖 B1–B7），外加 B8 的回归口径（全量 `cargo test --workspace` + `crash_resume` 20 轮 ×2）。
- `scripts/verify-workspace.ps1` 16 步全部 PASS，`SCRIPT_EXIT=0`（含 `cargo fmt --check`、`cargo check`、`cargo test --workspace`、`cargo clippy -D warnings`、`cargo doc -D warnings`、静态检查与 `crash_resume` 崩溃恢复两轮测试）。

### 6. 与原文不一致之处
功能与验收口径与原文一致，仅实现层为修复环境/通道缺陷做了如下调整（不影响需求）：
- 未完成任务计数 `outstanding` 原设计为 `watch::Sender<u64>`，但 `watch::send` 在无接收者时会**丢弃数值**，导致 `enqueue` 阶段计数无法累积、`wait_for_idle` 提前返回；改为 `AtomicU64` 作为权威计数，并以 `idle: Notify` 做唤醒（见 `queue/mod.rs:463` 与 `:281`）。
- CLI 主进程退出前显式 `store.shutdown()`（执行 `PRAGMA wal_checkpoint(TRUNCATE)` 并 **join** 专用线程），避免 `std::process::exit` 跳过析构导致 WAL 未回写、跨进程读不到数据。
- `cli_enqueue_move_and_queue_run` 集成测试为在本沙箱中可靠运行，使用 `#[tokio::test(flavor = "multi_thread")]`（等待子进程时仍驱动回环服务器 accept 循环），并将临时目录放在工作区本地盘（`tempfile::Builder::tempdir_in(current_dir())`）而非系统 `R:` 临时盘（沙箱 `R:` 盘跨进程 WAL 不可靠）。
- 移除 `ActiveTask` 中未被读取的 `progress_rx` 字段（避免 `dead_code` 与 forbidden `allow`）。
