# 任务：idm-rs Phase 3c —— 队列并发修正

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase3c.md`，`git status` 必须干净。

## 一、`queue/mod.rs` 修正

1. `outstanding` 的所有增减改用 `fetch_add` / `fetch_update`（下限为 0），`adopt_queued` 的 `store(n)` 也要改成原子操作
2. `pause()` 和 `shutdown()` 的等待循环，改为先创建 `notified()`，再检查条件，最后 await
3. 任务返回错误时：如果数据库里该任务仍是 `Queued`，在 `on_finished` 中将其改为 `Failed` 并记录错误（`TaskInUse` 除外，该情况改回 `Paused(Interrupted)`）。调度器不能把同一个任务反复拉起
4. `shutdown()` 要先取消所有活动任务，再等待它们收尾，与文档一致
5. 恢复 CLI 对单个任务进度的访问：`ActiveTask` 保留 `progress_rx`，`DownloadQueue` 提供 `progress(task_id)` 方法

## 二、测试

1. `pause_does_not_hang_when_task_finishes_concurrently`：任务在 `pause` 调用的同一时刻自然结束，重复 200 次，`pause` 必须全部在 1 秒内返回
2. `outstanding_stays_consistent_under_concurrent_finish`：20 个任务同时结束，计数归零，`wait_for_idle` 正常返回，重复 50 次
3. `failed_before_state_write_is_not_rescheduled`：构造任务启动即失败且未写状态，断言只被尝试 1 次，数据库状态变为 `Failed`
4. `shutdown_cancels_active_tasks`：队列有 2 个活动任务和 1 个排队任务，`shutdown` 后活动任务变为 `Paused(User)`，排队任务保持 `Queued`
5. 回归：原有全部测试、两个崩溃测试

## 三、提交

每个新测试单独连续运行 10 次；验收脚本全部 PASS。提交信息 `phase3c: queue concurrency fixes`，只暂存 `docs crates scripts Cargo.lock`，不得 push。报告列出每项修改的位置、测试名和 10 次结果，并列出与本文不一致之处。
