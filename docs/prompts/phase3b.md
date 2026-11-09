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

### B1. 写入 `ARCHITECTURE.md`（原样追加为第 11 节）

```markdown
## 11. 多任务队列与调度器（Phase 3b 确立）
- 队列持久化：SQLite `tasks` 表维护 `Queued` 状态、单任务限速 `max_speed` 与队列序号 `queue_order`。
- 调度规则：`DownloadQueue` 按 `queue_order ASC, id ASC` 自动拉起 `Queued` 任务，任何时刻活动下载任务数（处于 `Downloading` / `Probing` / `Finalizing` 的受管任务）严格不超过 `max_concurrent_tasks`（默认 3，可动态调整）。
- 全局资源共享：同一队列启动的所有任务共用同一个 `DiskWriterPool`、全局 `RateLimiter` 与按服务器 `HostConnectionLimiter`。
- 状态联动：活动任务完成、失败或被用户暂停后立即释放并发槽位并自动唤醒调度器拉起下一个 `Queued` 任务；对已暂停或失败的任务执行队列恢复时，将其置为 `Queued` 交由调度器按序执行。
- 启动恢复：程序重启执行 `recover` 时，保留数据库中原有的 `Queued` 任务状态与顺序不变。
