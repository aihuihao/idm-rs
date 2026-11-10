# Phase 3c 报告 —— 队列并发修正

任务原文：`docs/prompts/phase3c.md`。提交信息：`phase3c: queue concurrency fixes`。

## 一、每项修改的位置

| 项 | 要求 | 位置（文件:行号） | 实现 |
|---|---|---|---|
| 1 | `outstanding` 增减改用原子 `fetch_add` / `fetch_update`（下限 0）；`adopt_queued` 的 `store(n)` 改原子 | `crates/idm-core/src/queue/mod.rs:527`（`bump_outstanding`：增 `fetch_add` 在 `:529`，减 `try_update` 在 `:534`，`saturating_sub` 下限 0）；`crates/idm-core/src/queue/mod.rs:288`（`adopt_queued` 改为 `outstanding.fetch_add(n, SeqCst)`） | 移除全部 `load` + `store` 读改写，杜绝并发收尾丢失递减 |
| 2 | `pause()` / `shutdown()` 等待循环：先创建 `notified()`，再检查条件，最后 `await` | `crates/idm-core/src/queue/mod.rs:201`（`pause`）；`crates/idm-core/src/queue/mod.rs:339`（`shutdown`）。`wait_for_idle` 原本已是该顺序（`:297`），未改动 | `Notified` 构造时捕获 `notify_waiters` 调用计数，窗口内发生的通知在首次 poll 即可见 |
| 3 | 任务返回错误且 DB 仍 `Queued` → 写 `Failed` + `last_error`（`TaskInUse` → `Paused(Interrupted)`）；不得反复拉起 | `crates/idm-core/src/queue/mod.rs:436`（`on_finished`），错误分支 `:447-459`，状态写入 `:458`；无条件唤醒 `:490` | 状态落库早于「移出 active」（`:464`）与「唤醒调度器」（`:491`），期间 `try_schedule` 仍按 `active` 排除该 id；`list_queued_tasks` 随后查不到它 |
| 4 | `shutdown()` 先取消所有活动任务，再等待收尾 | `crates/idm-core/src/queue/mod.rs:308`（`shutdown`）；快照活动句柄 `:317-327`；逐个 `cancel()` `:331-333`；等待 active 清空 `:336-343` | 置停机标志 → 快照 → 减未启动排队计数 → 取消全部活动任务 → 等待收尾。未启动排队任务计数改为按 id 精确统计（`:322`） |
| 5 | `ActiveTask` 保留 `progress_rx`；`DownloadQueue` 提供 `progress(task_id)` | `crates/idm-core/src/queue/mod.rs:97`（字段）；`:402`（`spawn_task` 存入 `prx`）；`:277`（`progress` 方法）。CLI：`crates/idm-cli/src/main.rs:275`（导入 `QueueEvent`）、`:1301-1331`（`queue run` 每秒对每个活动任务调 `progress(id)` 打印 `TASK PROGRESS`） | 进度接收端不再被丢弃，CLI 恢复逐任务进度访问 |

## 二、测试名与单独连续运行 10 次结果

| 编号 | 测试名 | 位置 | 10 次结果 |
|---|---|---|---|
| 1 | `pause_does_not_hang_when_task_finishes_concurrently` | `crates/idm-core/src/queue/mod.rs:615`（单元测试，200 次迭代 / 每次 1s 超时） | 10/10 通过 |
| 2 | `outstanding_stays_consistent_under_concurrent_finish` | `crates/idm-core/src/queue/mod.rs:648`（单元测试，50 轮 × 20 任务并发收尾） | 10/10 通过 |
| 3 | `failed_before_state_write_is_not_rescheduled` | `crates/idm-core/tests/phase3c.rs:29` | 10/10 通过 |
| 4 | `shutdown_cancels_active_tasks` | `crates/idm-core/tests/phase3c.rs:111` | 10/10 通过 |
| 5 | 回归：原有全部测试 + 两个崩溃测试 | `cargo test --workspace --locked`（含 Phase 3b 全部 7 项）；`cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1`（2 个用例各 20 轮） | 全通过 |

新增测试 1、2 的辅助设施：`queue/mod.rs:554`（`test_queue`）、`:562`（`install_active`）。

## 三、验收脚本输出

`scripts/verify-workspace.ps1` 在 Windows PowerShell 5.1 下执行：STEP 1~16 全部 `PASS`，
`SUMMARY: all 16 steps executed; final result: PASS`，`SCRIPT_EXIT=0`
（含 `cargo fmt --check`、`cargo check --workspace --all-targets`、`cargo test --workspace`、
`cargo clippy --workspace --all-targets -- -D warnings`、`cargo doc ... RUSTDOCFLAGS=-D warnings`、
静态检查与两个崩溃恢复测试）。

## 四、与本文不一致之处

1. **`fetch_update` 实际使用 `try_update`（工具链所迫）**：本机 `rustc 1.99.1 (58cf8f9c3 2026-10-13)`
   把 `Atomic*::fetch_update` 重命名为 `try_update`，旧名被标记 `deprecated`。验收脚本 STEP 6 使用
   `-D warnings`，保留旧名会直接失败，且项目规则禁止用 `#[allow]` 抑制（STEP 13）。因此减少计数
   使用语义完全相同的 `try_update(SeqCst, SeqCst, |cur| Some(cur.saturating_sub(n)))`（下限 0），
   并在 `bump_outstanding` 的注释里注明旧名。增加计数仍是 `fetch_add`，`adopt_queued` 仍为 `fetch_add(n)`。
2. **两项并发测试放在 `queue/mod.rs` 的 `#[cfg(test)]` 单元测试，而非集成测试文件**：测试 1、2 需要
   精确模拟「任务与 `pause` 同一时刻结束」「20 个任务同时收尾」，必须直接访问私有的 `Inner`、
   `ActiveTask`、`on_finished`、`bump_outstanding`；集成测试 crate 无法访问。测试 3、4 放在
   `crates/idm-core/tests/phase3c.rs`。原文未指定测试文件位置，此处沿用 Phase 3b「及对应单元测试」的口径。
3. **`progress(task_id)` 的签名**：实现为 `pub async fn progress(&self, task_id: i64) -> Option<ProgressSnapshot>`。
   活动表由 `tokio::sync::Mutex` 保护，同步方法无法读取；`None` 表示任务不在活动表中。原文只要求
   「提供 `progress(task_id)` 方法」，未指定签名。
4. **附加修正（原文未列，为第 3、4 点正确性所必需）**：
   - `shutdown()` 的「未启动排队任务数」由 `queued.len() - active.len()` 改为按 id 排除活动项精确统计
     （`:322`）：活动任务的 DB 状态可能已是 `Downloading`，不在 `list_queued_tasks()` 结果中，
     旧公式会漏减计数，导致 Ctrl+C 后 `wait_for_idle` 悬挂。
   - `on_finished` 由「仅当计数归零才 `idle.notify_waiters()`」改为无条件通知（`:490`）：
     `shutdown()` 等待的是活动表清空，若只在计数归零时通知，`shutdown` 可能永远等不到唤醒。
   - 错误状态写入被安排在「移出 active / 唤醒调度器」之前（`:447` 早于 `:464`、`:491`），
     保证并发的 `try_schedule` 不会在状态落库前重复拉起同一任务。
5. **CLI 新增输出**：`queue run` 期间每秒打印若干 `TASK PROGRESS id=... downloaded=... total=... speed=...`
   行，以真正落实「恢复 CLI 对单个任务进度的访问」。不影响退出码、文件结果与原有输出解析。

## 五、提交

- 暂存范围：`docs crates scripts Cargo.lock`（本次 `Cargo.lock`、`scripts/`、`Cargo.toml` 均无改动）。
- 未 push、未打 tag。
