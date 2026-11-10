# 任务：idm-rs Phase 3b —— 限速器并发修正 + 多任务下载队列与调度器

沿用全部工作规则（见 `docs/prompts/` 下历次任务原文）。开始前先把本任务原文原样保存为 `docs/prompts/phase3b.md`，确认 `git status` 干净（基线为 `eab8095`）。
本任务分 A、B 两部分：先完成 A 部分并单独提交，再开始 B 部分。所有测试均使用本地回环服务器，不访问外网。
**提交前必须完成第七部分的自查表，任何一项测试缺失都不得提交。**

---

## A. 前置修正：消除 `RateLimiter` 的多连接群聚阻塞与欠债抹平漏洞（先做完 A，单独提交）

### A1. 问题现状（`crates/idm-core/src/net/ratelimit.rs`）
1. 当前 `acquire(n)` 在睡眠前先把 `n` 从共享的 `b.tokens` 中扣成负数，并在循环中以 `if b.tokens >= 0.0` 作为放行条件。当多个连接并发调用 `acquire` 时，第 1 个连接会被后续连接累加的负向欠债挡住，导致所有连接一起卡死到全部欠债还清才同时放行（群聚阻塞），且持续高速调用的连接会饿死其他等待中的连接。
2. `set_rate` 执行 `b.tokens.min(capacity).max(0.0)` 把负数清零后，所有正在等待 `b.tokens >= 0.0` 的连接会同时看到 `b.tokens >= 0.0` 而集体放行。
3. `acquire` 在睡眠期间若被外层 `select!` 取消丢弃，预先扣减的 `n` 不会退还。

### A2. 修正要求（改为非负分段消费模型 + 取消退还守卫）
1. **桶内不变式**：任何时刻 `0.0 <= b.tokens <= b.capacity`，绝不允许 `b.tokens` 变为负数。
2. **`acquire(&self, n: u64)` 实现**：
   - 使用局部变量 `remaining = n` 与已领字节计数 `taken = 0u64`（由局部 RAII 守卫持有：若 `acquire` 正常返回则调用 `guard.defuse()`；若在完成前被取消 drop 且 `taken > 0`，则在 `Drop` 中加锁将 `taken` 退回 `b.tokens = (b.tokens + taken as f64).min(b.capacity)`）。
   - 循环（当 `remaining > 0`）：
     1. 加锁：若 `b.rate <= 0.0`，`guard.defuse()` 并立即返回。
     2. 调用 `self.refill(&mut b, Instant::now())`。
     3. 若 `b.tokens >= 1.0`：
        - `let take = (b.tokens.floor() as u64).min(remaining);`
        - `b.tokens -= take as f64;`
        - `remaining -= take;`
        - `guard.taken += take;`
        - 若 `remaining == 0`，`guard.defuse()` 并立即返回。
     4. 计算本次还需等待的字节数：`let need = (remaining as f64).min(b.capacity) - b.tokens;`
     5. `let wait = (need / b.rate).max(MIN_WAIT_SECS);` 解锁。
     6. `tokio::select! { biased; _ = generation.changed() => {}, _ = tokio::time::sleep(Duration::from_secs_f64(wait)) => {} }`。
3. **`set_rate(&self, bytes_per_sec: Option<u64>)` 修正**：
   - 加锁后先按旧速率 `refill(&mut b, now)`。
   - 若新速率为降速（`new_rate > 0.0 && (b.rate <= 0.0 || new_rate < b.rate)`），将 `b.tokens` 清零（`b.tokens = 0.0`），确保降速从当前时刻立即按新速率生效，不把高限速阶段积攒的剩余令牌带入低限速阶段产生额外 1 秒突发；其他情况（升速或同速）截断为 `b.tokens = b.tokens.min(capacity)`。
   - 更新 `b.rate`、`b.capacity` 并递增 `generation`。
   - 同步更新 `dynamic_rate_change_clamps_tokens_and_wakes_waiters` 单元测试中对降速后 `tokens()` 的断言。
4. **新增/强化测试**：
   - 单元测试 `staggered_concurrent_acquires_do_not_stall_first_waiter`：限速 `256 KiB/s`，4 个并发任务同时调用 `acquire(128 KiB)`。断言**最快完成的第 1 个任务**在 `400ms ~ 750ms` 内返回（证明没有被其余 3 个任务的配额拖到 2 秒才返回），最后一个任务在 `1800ms ~ 2500ms` 内返回。
   - 单元测试 `cancelled_acquire_refunds_partial_tokens`：限速 `256 KiB/s`，先等待积攒 `128 KiB` 令牌，然后启动 `acquire(256 KiB)`（它会先领走 `128 KiB` 并睡眠等待剩余 `128 KiB`），在 30ms 后将其取消 drop；断言取消后 `rl.tokens() >= 128 KiB`（已领走的部分被退回）。
   - 改造 `phase3a.rs` 中的 `dynamic_rate_decrease_clamps_burst`：改为 **4 个连接**（不再用 1 个连接），去掉 300ms 稳定等待，在调用 `set_rate(Some(1 MiB/s))` 后立即统计接下来 2 秒内的客户端落盘进度增长量（`committed` 或 `progress_rx` 已写字节数）与服务器发送量，断言客户端落盘增长量 `<= 2.4 MiB`。
5. **A 部分提交**：验收脚本全部 PASS 后，只暂存 `crates docs`，提交信息：`phase3b-pre: fix ratelimiter concurrent debt and starvation`。

---

## B. 多任务下载队列与调度器

### B1. 写入 `ARCHITECTURE.md`（原样追加为第 11 节，共 6 行）

## 11. 多任务队列与调度器（Phase 3b 确立）
- 队列持久化：SQLite `tasks` 表维护 `Queued` 状态、单任务限速 `max_speed` 与队列序号 `queue_order`。
- 调度规则：`DownloadQueue` 按 `queue_order ASC, id ASC` 自动拉起 `Queued` 任务，任何时刻活动下载任务数（处于 `Downloading` / `Probing` / `Finalizing` 的受管任务）严格不超过 `max_concurrent_tasks`（默认 3，可动态调整）。
- 全局资源共享：同一队列启动的所有任务共用同一个 `DiskWriterPool`、全局 `RateLimiter` 与按服务器 `HostConnectionLimiter`。
- 状态联动：活动任务完成、失败或被用户暂停后立即释放并发槽位并自动唤醒调度器拉起下一个 `Queued` 任务；对已暂停或失败的任务执行队列恢复时，将其置为 `Queued` 交由调度器按序执行。
- 启动恢复：程序重启执行 `recover` 时，保留数据库中原有的 `Queued` 任务状态与顺序不变。

### B2. 数据库迁移（版本 3，`crates/idm-core/src/store/`）
1. **`TaskStateDb` 新增变体 `Queued`**（数据库文本值为 `"queued"`）；`TaskState` 状态机同步新增 `Queued` 状态及合法转换（`Pending -> Queued`、`Queued -> Probing`、`Queued -> Paused`、`Queued -> Cancelled`、`Paused -> Queued`、`Failed -> Queued`）。
2. **Schema 迁移 V3（`PRAGMA user_version = 3`）**：
   - 为 `tasks` 表新增两列：
     - `queue_order INTEGER NOT NULL DEFAULT 0`
     - `max_speed INTEGER`（可空，记录单任务限速字节/秒）
   - 迁移时将已有记录的 `queue_order` 初始化为 `id`。
3. **Store 新增方法**：
   - `enqueue_task(...) -> Result<i64>`：插入一条状态为 `Queued` 的任务，`queue_order` 自动设为当前表内 `MAX(queue_order) + 1`，保存 `max_speed`。
   - `list_queued_tasks() -> Result<Vec<TaskRecord>>`：按 `queue_order ASC, id ASC` 返回所有状态为 `Queued` 的任务。
   - `requeue_task(id: i64, to_front: bool) -> Result<()>`：将 `Paused`、`Failed` 或 `Finalizing` 状态的任务改为 `Queued`（`to_front = true` 时令 `queue_order = MIN(queue_order) - 1`，否则保持原 `queue_order` 或置于队尾；若任务处于其他状态则返回非法状态转换错误）。
   - `move_in_queue(id: i64, position: QueueMove) -> Result<()>`：支持 `Top`（移到队首）、`Bottom`（移到队尾）、`Up`（与前一个 `Queued` 任务交换 `queue_order`）、`Down`（与后一个交换）。
4. **启动恢复（`recover`）确认**：保持 `Queued` 状态的任务不变，不将其误改为 `Paused` 或 `Failed`。

### B3. 队列调度器（新建模块 `crates/idm-core/src/queue/mod.rs`，不膨胀 `download.rs`）
1. **核心配置与句柄 `DownloadQueue`**：
   - `QueueConfig`：`max_concurrent_tasks: usize`（默认 3，合法范围 1..=16）、`global_max_speed: Option<u64>`、`default_download_config: DownloadConfig`、`default_strategy: AllocationStrategy`。
   - 内部持有：`Arc<Store>`、`Arc<DiskWriterPool>`、全局 `Arc<RateLimiter>`、全局 `Arc<HostConnectionLimiter>`。
   - 后台运行一个调度循环（通过 `Notify` / `mpsc` 驱动），维护当前活动任务表（`task_id -> ActiveTask { cancel: CancelSignal, progress_rx: watch::Receiver<ProgressSnapshot> }`）。
2. **核心操作 API**：
   - `enqueue(&self, req: EnqueueRequest) -> Result<i64>`：写入数据库（`Queued`）并唤醒调度器。若当前活动任务数 `< max_concurrent_tasks`，立即在后台启动该任务。
   - `pause(&self, task_id: i64) -> Result<()>`：
     - 若任务正在活动中：触发其 `CancelSignal`，等待该任务正常收尾退出（数据库变为 `Paused(User)`），释放槽位并自动拉起下一个 `Queued` 任务；
     - 若任务处于 `Queued`：直接将数据库状态改为 `Paused(User)`。
   - `resume_queued(&self, task_id: i64, to_front: bool) -> Result<()>`：调用 `store.requeue_task` 并唤醒调度器。当调度器分配到槽位执行该任务时：
     - 若该任务已有非空 `part_path`（或原本处于 `Finalizing`），走 `resume()` 续传路径（复用队列的 `global_limiter` 与 `host_limiter`）；
     - 若 `part_path` 为空，走 `download_with_store()` 新建下载路径。
   - `move_queue(&self, task_id: i64, pos: QueueMove) -> Result<()>`：调整队列顺序并唤醒调度器。
   - `set_max_concurrent_tasks(&self, n: usize) -> Result<()>`：动态修改并发任务数上限（1..=16）。调大时立即唤醒调度器补足新槽位；调小时不中断已在运行的任务，待活动任务自然减少到 `< n` 后再生效。
   - `set_global_max_speed(&self, bytes_per_sec: Option<u64>)`：直接调用内部全局 `RateLimiter::set_rate`，对所有正在运行的任务立即生效。
   - `subscribe_events(&self) -> tokio::sync::broadcast::Receiver<QueueEvent>`：对外发布 `TaskEnqueued`、`TaskStarted`、`TaskCompleted`、`TaskPaused`、`TaskFailed` 事件。
   - `wait_for_idle(&self) -> Result<QueueSummary>`：等待队列中所有 `Queued` 任务与所有活动任务全部结束（供 CLI 和测试使用）。
   - `shutdown(&self) -> Result<()>`：停止接纳新任务，取消所有当前活动任务并等待它们安全收尾退出。

### B4. CLI 扩展（`crates/idm-cli/src/main.rs`）
1. `idm-cli enqueue <URL> [--dir <目录>] [--connections <N>] [--out <文件名>] [--max-speed <值>] [--db <路径>]`：
   - 将任务以 `Queued` 状态入队，打印任务 ID 与 `queue_order` 后退出。
2. `idm-cli queue run [--max-tasks <N>] [--global-max-speed <值>] [--max-host-connections <N>] [--proxy <URL> | --no-proxy] [--db <路径>]`：
   - 启动恢复 → 创建 `DownloadQueue` → 自动调度数据库中所有 `Queued` 任务直至队列完全空闲（`wait_for_idle`）→ 打印汇总（完成数、失败数、暂停数、总耗时）后退出。支持 Ctrl+C 安全停止所有活动任务。
3. `idm-cli queue move <id> <top|bottom|up|down> [--db <路径>]`：调整排队任务的顺序。
4. `idm-cli list`：兼容显示 `Queued` 状态与 `queue_order`。

### B5. 测试要求（统一放在 `crates/idm-core/tests/phase3b.rs` 及对应单元测试）
1. **Schema V3 迁移与启动恢复测试**：
   - `migration_v2_to_v3_and_recover_preserves_queued`：验证 V3 迁移成功；插入 `Queued` 与 `Downloading` 任务后执行 `recover`，断言 `Downloading` 变为 `Paused(Interrupted)`，而 `Queued` 保持 `Queued` 且 `queue_order` 不变。
2. **并发任务上限与自动接续测试**：
   - `queue_respects_max_concurrent_tasks_and_fifo_order`：设置 `max_concurrent_tasks = 2`，依次入队 5 个下载任务。服务器端与事件流同时验证：任何时刻同时处于下载中的任务数峰值严格 `== 2`，且任务启动顺序严格为 `1, 2, 3, 4, 5`，最终 5 个文件全部逐字节正确。
3. **暂停活动任务自动拉起排队任务 + 续传恢复测试**：
   - `pause_active_task_starts_next_queued_and_resume_completes`：`max_concurrent_tasks = 1`，入队任务 A（慢速）和任务 B（快速）。等任务 A 开始下载并写入部分数据后，调用 `queue.pause(A)`：断言任务 A 变为 `Paused(User)`，任务 B 立即自动开始并完成；随后调用 `queue.resume_queued(A, false)`，任务 A 从断点续传完成且文件逐字节正确（通过服务器收到的 Range 头确认 A 是续传而非重下）。
4. **队列重排（优先级调整）测试**：
   - `queue_reordering_changes_execution_order`：`max_concurrent_tasks = 1`，任务 1 正在运行，任务 2、3、4 处于 `Queued`。对任务 4 执行 `QueueMove::Top`、对任务 2 执行 `QueueMove::Down`，断言任务 1 完成后，后续启动顺序严格为 `4 -> 3 -> 2`。
5. **运行中动态调整最大并发任务数测试**：
   - `dynamic_max_concurrent_tasks_scales_up_and_down`：初始 `max_concurrent_tasks = 1`，入队 4 个慢速任务（此时只有 1 个运行）；调用 `set_max_concurrent_tasks(3)`，断言 100ms 内活动任务数升至 3；随后调用 `set_max_concurrent_tasks(1)`，断言已在运行的 3 个任务不会被强杀，全部正常完成。
6. **队列共享全局限速与 Host 连接限制测试**：
   - `queue_shares_global_limiter_and_host_limiter`：`max_concurrent_tasks = 3`，每个任务 `connections = 4`（共 12 个分块需求），队列配置 `max_connections_per_host = 5`、`global_max_speed = Some(2 MiB/s)`，3 个任务各下载 2 MiB（共 6 MiB）。断言服务器观察到的最大并发连接数 `<= 5`，总耗时在 `2.5s ~ 4.0s` 之间，3 个文件全部逐字节正确。
7. **CLI 队列子命令端到端测试**：
   - `cli_enqueue_move_and_queue_run`：通过 `idm-cli enqueue` 入队 3 个任务，用 `idm-cli queue move` 调整顺序，再用 `idm-cli queue run --max-tasks 2` 跑完，断言退出码为 0 且 3 个文件全部逐字节正确。
8. **回归测试**：原有全部测试、Phase 2 的 50 种子并发测试、两个 20 轮崩溃测试全部通过。

## 六、提交

- B 部分每个新增测试单独连续运行 10 次，全部通过；`scripts/verify-workspace.ps1` 16 步全部 PASS，exit code 0。
- B 部分只暂存 `docs crates scripts ARCHITECTURE.md Cargo.toml Cargo.lock`，提交信息：`phase3b: multi-task download queue and scheduler`，不得 push，不打 tag。

## 七、提交前自查表（必须完成）

提交前在 `docs/phase3b-report.md` 中列出下表（含 A 部分已完成的 3 项与 B 部分的 8 项测试），任何一项测试名为空或未满 10/10 通过均不得提交：

| 编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|

## 八、报告

- A、B 两次提交的 commit hash
- 第七部分的完整自查表
- B 部分数据库 V3 表结构、队列调度器核心方法的代码位置（文件:行号）
- B5 第 2、3、4、5、6、7 项测试的实测关键指标
- 测试总数与验收脚本输出
- 与本文不一致之处（没有就写“无”）
