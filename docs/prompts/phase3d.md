# 任务：idm-rs Phase 3d —— Host 名额排队与接管的配合修正

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase3d.md`，`git status` 必须干净。

## 一、背景

`attempt_chunk_once` 在发请求前 `host_limiter.acquire` 只监听取消。分块排队等名额时：
1. 速度采样从创建时算起，满 4 秒会被当作"卡住"成为接管对象；
2. 接管零进度分块时，主循环等它 5 秒内以 Completed 退出，但它在 acquire 里收不到 end 缩小的通知，退不出来，任务因超时失败；
3. 排队中的分块也被计入在途连接数。

## 二、修正

1. **acquire 可被接管唤醒**：名额等待与 `wake_on_shrink` 并列监听。被唤醒且 `acked >= end` 时丢弃等待，返回 Completed；否则继续等同一个 acquire（不得重新排队到队尾）。`HostConnectionLimiter` 如需要，提供可被 `select!` 中断而不丢失排队位置的接口（保持 Semaphore 公平队列的位置）
2. **排队中的分块不参与切分选择**：`ChunkShared` 增加"已发出请求"标记（拿到名额、`req.send()` 即将开始时置位）。`select_victim` 只在已置位的分块中选择；`is_fresh` 的计时起点改为置位时刻
3. **在途连接数只计已持有名额的分块**：`join_set.len() + pending.len() < max_concurrent` 的判断改为统计已置位的分块与 pending 之和。排队中的分块不触发新的切分
4. 重试时：每次重试发请求前重新申请名额，标记在 acquire 成功后重新置位，退避期间标记复位为未置位（退避中的分块仍可被接管，这与 10.5 一致；这里复位的是"持有名额"而不是"可被接管"，请据此区分两个标记：`holds_permit` 与 `eligible_for_split`，退避期间 `holds_permit = false`，`eligible_for_split` 保持原来的判定）

## 三、测试

1. `queued_chunk_is_not_taken_over`：`max_per_host = 2`，一个任务 4 个连接，服务器让前 2 个请求各停 6 秒。断言：在前 2 个请求停顿期间，排队中的另外 2 个分块不会被接管；任务不因 5 秒接管超时失败；最终文件逐字节正确
2. `queued_chunk_released_by_takeover_wakes_immediately`：构造一个排队中的分块恰好被接管到 acked 的情形（可通过测试钩子直接缩小其 end），断言它在 100 毫秒内以 Completed 退出，且不占用名额
3. `in_flight_count_excludes_queued_chunks`：断言排队分块不会使"在途连接数"判断提前触发新切分
4. 单元测试：`select_victim` 忽略未发出请求的分块
5. 回归：phase3a 全部、phase2 全部（含 50 种子）、两个崩溃测试

## 四、提交

每个新测试单独连续运行 10 次；验收脚本全部 PASS。提交信息 `phase3d: host permit queueing vs takeover`，只暂存 `docs crates scripts Cargo.lock`，不得 push。报告列出每项修改位置、测试名、10 次结果、与本文不一致之处。
