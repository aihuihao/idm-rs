# Phase 3d 报告 —— Host 名额排队与接管的配合修正

任务原文：`docs/prompts/phase3d.md`。提交信息：`phase3d: host permit queueing vs takeover`。

## 一、每项修改的位置

| 项 | 要求 | 位置（文件:行号） | 实现 |
|---|---|---|---|
| 1 | `acquire` 可被接管唤醒，且不丢失 Semaphore 公平队列位置 | `crates/idm-core/src/net/host_limit.rs:112`（`acquire_future`）、`:137`（`HostPermitFuture`）、`:97`（`acquire` 改为复用它）、`:128`（`available_permits` 诊断接口）；`crates/idm-core/src/net/mod.rs:15`（重导出）；`crates/idm-core/src/task/download.rs:1234`（`acquire_with_shrink`）、`:1454`（调用点） | `HostPermitFuture` 把 `Semaphore::acquire_owned` 的 future **pin 住并返回**，调用方用 `&mut fut` 跨 `select!` 轮次反复 poll；被 `wake_on_shrink` 唤醒且 `acked >= end` 时返回 `Ok(None)`（丢弃等待、不占名额、以 `Completed` 结束），否则继续等**同一个** future。`acquire()` 行为不变 |
| 2 | 排队中的分块不参与切分选择；`is_fresh` 计时起点改为置位时刻 | `crates/idm-core/src/task/split.rs:85`（`eligible_for_split`）、`:126`（`mark_request_started`：置位两个标记并把 `started` 与速度采样重置到此刻）、`:430`（`select_victim` 只保留 `eligible_for_split()` 的分块，平均速度也只统计它们）；`download.rs:1472`（拿到名额、`req.send()` 前调用） | 排队等名额的分块从未发请求，`eligible_for_split=false`，不会被当作「卡住」接管；计时与速度采样都从发出请求起算 |
| 3 | 在途连接数只计已发出请求的分块，排队分块不触发新切分 | `split.rs:88`（`waiting_for_permit`）、`:155/:159/:163`（置位/清除/查询）、`:495`（`in_flight_count`）、`:366`（`ChunkSlot::shared`）；`download.rs:649`（主循环闭包）、`:702` 与 `:763`（两处 `join_set.len() + pending.len() < max_concurrent` 判断）、`:1261`（`WaitingForPermitGuard`，进入/离开名额申请时置位/清除） | `in_flight = join_set.len() + pending.len() − 正在排队等名额的分块数`（详见第四节第 1 条） |
| 4 | 重试重新申请名额；退避期间 `holds_permit=false`、`eligible_for_split` 保持 | `split.rs:82`（`holds_permit`）、`:140`（`release_permit`，幂等复位）；`download.rs:1454`（每次 `attempt_chunk_once` 都重新走 `acquire_with_shrink`）、`:1472`（acquire 成功后重新置位）、`:1272`（`HeldPermit`，`Drop` 时复位 `holds_permit`，`eligible_for_split` 不动） | 退避中仍可被接管（10.5），但不占在途连接槽位 |
| 测试钩子 | 让前 N 个请求停住 | `crates/idm-core/tests/support/mod.rs:157`（`ServerConfig::response_delay`）、`:197`（Default）、`:509`（`handle_conn` 在响应头前 sleep）；`crates/idm-core/tests/phase3d.rs:35`（前 2 个请求各停 6 秒） | 复现「2 个请求占满 Host 名额、其余分块排队」 |

## 二、测试名与单独连续运行 10 次的结果

| 编号 | 测试名 | 位置 | 10 次结果 |
|---|---|---|---|
| 1 | `queued_chunk_is_not_taken_over` | `crates/idm-core/tests/phase3d.rs:31` | 10/10 通过 |
| 2 | `queued_chunk_released_by_takeover_wakes_immediately` | `crates/idm-core/src/task/download.rs:1895`（单元测试） | 10/10 通过 |
| 3 | `in_flight_count_excludes_queued_chunks` | `crates/idm-core/src/task/split.rs:1089`（单元测试） | 10/10 通过 |
| 4 | `select_victim_ignores_chunks_without_request` | `crates/idm-core/src/task/split.rs:1048`（单元测试，原文第 4 项未给名字） | 10/10 通过 |
| 附 | `acquire_future_keeps_queue_position_when_interrupted` | `crates/idm-core/src/net/host_limit.rs:224`（单元测试，验证第 1 项「不重新排队到队尾」） | 10/10 通过 |

第 2 项通过测试钩子直接把排队分块的 `end` 缩到 `acked`，断言 100ms 内以 `Completed` 退出且名额可用数不变。
第 1 项断言任务 `Completed`、文件逐字节正确、`takeover_count` 落在 `1..=2`（只有持有名额、已发请求的 2 个分块可被接管），排队分块未被接管且不触发 5 秒接管超时。

## 三、回归与验收脚本输出

- `cargo test --workspace --locked` 全绿：`idm-core` lib 88 个单元测试、phase3a 9 项、phase2 12 项（含 50 种子并发，73.36s）、phase1d 13 项、phase3b 7 项、phase3c 2 项、phase3d 1 项、其余全部；两个崩溃测试以 `--ignored` 由脚本 STEP 16 单独运行。
- `scripts/verify-workspace.ps1`：16 个 `[STEP]`、15 个 `PASS`（STEP 14 为 informational 不打 PASS）、0 个 `FAIL`，
  `SUMMARY: all 16 steps executed; final result: PASS`，进程退出码 `0`。
  含 `cargo fmt --check`、`cargo check --workspace --all-targets`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings`、`cargo doc ... -D warnings`、静态检查、
  以及 STEP 16 的两个崩溃恢复测试（20 轮杀进程续传）。

## 四、与本文不一致之处

1. **第 3 点公式按可回归的方式实现**：原文写「`join_set.len() + pending.len() < max_concurrent` 改为统计已置位的分块与 pending 之和」「在途连接数只计已持有名额的分块」。若严格只数 `holds_permit=true` 的分块，退避期间（第 4 点明确要求 `holds_permit=false`）`in_flight` 会掉到 0，使 `phase1d::persistent_500_exhausts_retries_then_resume_completes` 在每次退避时多切分一个分块，服务器请求数由 6 变成 9，回归失败。因此实际实现为
   `in_flight = join_set.len() + pending.len() − 正在排队等名额的分块数`：只扣除**从未发出请求、卡在 `acquire` 里**的分块，退避中/下载中/已置位的分块仍计入。这与原文正文「统计已置位的分块与 pending 之和」等价（已置位 = 非排队），也满足「排队中的分块不触发新的切分」，但未逐字落实「只计已持有名额」。
2. **新增第三个标记 `waiting_for_permit`**：原文要求区分 `holds_permit` 与 `eligible_for_split` 两个标记；为满足上一条的回归，额外引入 `waiting_for_permit`（当前是否正卡在名额申请中排队）。三个标记职责：`holds_permit` = 本次请求是否持有名额；`eligible_for_split` = 是否发过请求（可参与 `select_victim`，退避中保持）；`waiting_for_permit` = 是否正在排队等名额（从在途连接数扣除）。
3. **`is_fresh` 计时起点连带重置速度采样**：原文只要求 `is_fresh` 的计时起点改为置位时刻；`mark_request_started()` 同时把 `started` 与 `samples` 重置到此刻（样本位置取当前 `acked`）。否则排队超过 4 秒的分块一拿到名额就会因旧样本被判速度 0 / 卡住，立刻成为接管对象，与「速度采样从创建时算起」这一背景问题相悖。
4. **`HostConnectionLimiter` 新增公开接口**：`acquire_future` / `HostPermitFuture` / `available_permits`，并在 `net/mod.rs` 重导出。原文「如需要，提供可被 `select!` 中断而不丢失排队位置的接口」，属允许范围；`acquire()` 语义未变。
5. **测试文件位置与命名**：原文未指定文件位置。第 1 项放 `tests/phase3d.rs`；第 2、3、4 项放各自模块的 `#[cfg(test)]` 单元测试（需要访问私有 `acquire_with_shrink` / `select_victim` / `in_flight_count`）；第 4 项命名为 `select_victim_ignores_chunks_without_request`。另加一个单测验证排队位置保持。
6. **测试服务器钩子**：原文只说「服务器让前 2 个请求各停 6 秒」，未指定实现。新增 `ServerConfig::response_delay: Option<(u64, Duration)>`，让前 N 个非 Probe 响应在发送响应头前 sleep。
7. **回归范围**：原文第 5 项列 phase3a / phase2 / 两个崩溃测试；验收脚本 STEP 5 的 `cargo test --workspace` 实际覆盖全部原有测试（含 phase1d、phase3b、phase3c），STEP 16 单独覆盖两个崩溃测试。

## 五、提交

- 暂存范围：`docs crates scripts Cargo.lock`（本次 `Cargo.lock`、`scripts/` 无改动）。
- 未 push、未打 tag。
