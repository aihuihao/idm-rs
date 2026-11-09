# Phase 3b A 部分报告：`RateLimiter` 并发修正

- 任务原文：`docs/prompts/phase3b.md`；开始时 `git status` 干净，基线提交 `eab8095`。
- **重要：收到的任务原文在 B1 的 ```markdown 代码块中途被截断**（没有收尾的围栏，
  也没有 B2 实现要求、第五部分测试、第六部分提交、第七部分自查表、第八部分报告）。
  因此本提交**只完成 A 部分并单独提交**，B 部分（多任务下载队列与调度器）尚未开始，
  `ARCHITECTURE.md` 第 11 节也尚未写入。已收到的原文按原样保存，待补齐后追加。
- 提交信息：`phase3b-pre: fix ratelimiter concurrent debt and starvation`
  （只暂存 `crates docs`，未 push、未打 tag）。
- 所有测试均使用本地回环服务器，未访问外网。

## 一、提交前自查表（A 部分对应第七部分；B 部分的自查表待原文补齐后完成）

| 任务编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|
| A2.4-1 | 4 个并发 `acquire(128 KiB)` 不群聚阻塞 | `net::ratelimit::tests::staggered_concurrent_acquires_do_not_stall_first_waiter` | **10/10** |
| A2.4-2 | 取消 `acquire` 时已领令牌退回 | `net::ratelimit::tests::cancelled_acquire_refunds_partial_tokens` | **10/10** |
| A2.3 | 降速时 `tokens()` 清零、唤醒等待者 | `net::ratelimit::tests::dynamic_rate_change_clamps_tokens_and_wakes_waiters`（改动过） | **10/10** |
| A2.4-3 | 4 连接降速后 2 秒落盘增长 ≤ 2.4 MiB | `dynamic_rate_decrease_clamps_burst`（`tests/phase3a.rs`，改动过） | **10/10** |
| A2 回归 | 桶容量 / 单次等待 / 并发共享 / 双层限速 | `capacity_is_one_second_quota_with_256kib_floor`、`acquire_waits_for_one_second_quota`、`concurrent_acquires_share_one_bucket`、`bandwidth_limiter_applies_both_layers` | **各 10/10** |
| A2 回归 | 动态升速 / 共享全局限速的端到端行为 | `dynamic_rate_increase_and_unlimited`、`global_rate_limiter_shared_across_tasks` | **各 10/10** |
| A2 回归 | 全部原有测试、两个 20 轮崩溃测试 | `scripts/verify-workspace.ps1`（第 5、16 步） | **16/16 PASS，exit code 0** |

无一项测试名为空，无一项低于 10/10。

## 二、每项修改的位置

| A2 条目 | 位置 | 内容 |
|---|---|---|
| 桶内不变式 `0 <= tokens <= capacity` | `crates/idm-core/src/net/ratelimit.rs:45`（`Bucket` 文档）、`:205`（分段领取）、`:139`（降速清零）、`:82`（退还封顶） | 所有写 `tokens` 的路径都保持非负且不超容量 |
| 取消退还守卫 | `ratelimit.rs:61`（`TakenGuard`）、`:82`（`Drop` 退还 `(tokens + taken).min(capacity)`）、`:73`（`defuse`） | `acquire` 正常返回解除守卫；被取消 drop 且 `taken > 0` 时加锁退回 |
| `acquire` 非负分段消费（A2 第 2 条 1~6 步） | `ratelimit.rs:184`~`:226` | 局部 `remaining` / `guard.taken`；`:193` 不限速立即返回、`:199` `refill`、`:205` `take = floor(tokens).min(remaining)`、`:215` `need = min(remaining, capacity) - tokens`、`:217` `wait = (need/rate).max(MIN_WAIT_SECS)`、`:220` `select!` 与 `generation.changed()` 并列 |
| `set_rate` 降速清零 | `ratelimit.rs:133`~`:147` | 先按旧速率 `refill`；`decreasing = new_rate > 0 && (old_rate <= 0 || new_rate < old_rate)` 时 `tokens = 0.0`，否则 `tokens = tokens.min(capacity)`；更新速率/容量并递增 `generation` |
| 单元测试断言同步 | `ratelimit.rs:289`~`:330` | 降速后 `tokens()` 断言由「截到 1 MiB」改为「清零 0.0」，并增加非负/不超容量断言 |
| 新单元测试 | `ratelimit.rs:333`（staggered）、`:365`（refund） | 见第一节自查表 |
| 集成测试改造 | `crates/idm-core/tests/phase3a.rs:105`~`:160` | 4 个连接（`:117`）、去掉 300ms 固定稳定等待、同时统计客户端 `progress_rx` 落盘增长（`:135`/`:141`）与服务器 `bytes_sent` 增长，断言 `client_growth <= 2 MiB + 409 KiB`（≈2.4 MiB，`:145`） |
| 任务原文 | `docs/prompts/phase3b.md` | 已收到的原文原样保存（B1 处截断，待补齐） |

## 三、实测关键指标

- `staggered_concurrent_acquires_do_not_stall_first_waiter`（256 KiB/s，4×128 KiB）：
  **第 1 个 501.3~502.0 ms**（断言 400~750 ms），**最后 1 个 2.001 s**（断言 1.8~2.5 s）。
  旧「预扣 + 欠债」模型下 4 个会一起等到 2.0 s，第一个不可能落在 400~750 ms。
- `cancelled_acquire_refunds_partial_tokens`：取消后 `tokens ≈ 131,494~131,602` 字节
  （≈128 KiB 已领令牌 + 30ms 内按 256 KiB/s 生成的少量令牌），容量 262,144；
  断言 `>= 128 KiB` 通过。
- `dynamic_rate_decrease_clamps_burst`（4 连接、50 MiB/s → 1 MiB/s）：5 次取样
  客户端 2 秒落盘增长 **1,671,168 或 2,088,960 字节（1.59 / 1.99 MiB）**，
  服务器 2 秒发送 **2,031,616 ~ 2,490,368 字节**；阈值 2,515,968 字节（2.4 MiB），
  10 次单独运行全部通过。总耗时约 2.97~3.08 s。
- 其余回归：`dynamic_rate_increase_and_unlimited` 约 1.62 s（16 MiB，1.5 s 后不限速）、
  `global_rate_limiter_shared_across_tasks` 约 6.0 s（12 MiB / 2 MiB/s）。

## 四、测试总数与验收脚本输出

- `cargo test --workspace --locked`：**202 passed，0 failed，4 ignored**
  （idm-cli 8 + phase1d 1、idm-core lib 81、disk_concurrency 5、phase1b 39、
  phase1c1 12、phase1c_pre 2、phase1c_resume 9、phase1c_store 11、phase1d 13、
  phase2 12、phase3a 9）。
- `cargo fmt --all -- --check`：通过；`cargo clippy --workspace --all-targets -- -D warnings`：0 警告。
- `scripts/verify-workspace.ps1`：**16 个步骤全部 PASS，exit code 0**
  （第 5 步 `cargo test --workspace --locked`、第 16 步两个 20 轮崩溃测试均通过）。

## 五、与本文不一致之处（A 部分）

1. **`dynamic_rate_decrease_clamps_burst` 与进度通道做了一次同步**：任务要求
   「去掉 300ms 稳定等待、立即统计」。固定的 300 ms sleep 已删除；但
   `ProgressPublisher::publish_if_due` 按 200 ms 节流发布，若在 `set_rate` 瞬间直接读
   `progress_rx`，拿到的可能是 50 MiB/s 阶段（最多 200 ms 前、约 10 MiB）的旧值，
   会被算进「接下来 2 秒」的增长量。实现改为先把当前值标记已读，再等变更后的
   **第一次新发布**作为基准（`timeout(500ms, rx.changed())`，实际通常 ≤200 ms），
   然后才开始 2 秒窗口。这是与进度通道的同步，不是固定的速率稳定等待；服务器发送量
   在同一窗口内同时记录，仅作对照、不参与断言。
2. **`set_rate` 改为不限速时**：按 A2 第 3 条「其他情况截断为 `tokens.min(capacity)`」，
   此时 `capacity = 256 KiB`，令牌被截到 256 KiB。因为 `rate = 0` 时 `acquire` 直接放行、
   `refill` 不补充，这些令牌不会被消费，不影响行为。
3. **B 部分未开始**：任务原文在 B1 代码块中途截断，缺少 B2 实现要求、第五部分测试、
   第六部分提交、第七部分完整自查表与第八部分报告要求；本提交只覆盖 A 部分，
   未写入 `ARCHITECTURE.md` 第 11 节，也未创建 `DownloadQueue`。待补齐原文后继续。
