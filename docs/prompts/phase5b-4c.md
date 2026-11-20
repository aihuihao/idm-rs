# 任务：idm-rs Phase 5b-4c —— 每主机上限即时生效、暂停分块显示、限速测试排查

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5b-4c.md
2. 不联网，不新增依赖，不执行 cargo update
3. 不得使用 #[allow]、unsafe、unwrap()、expect()
4. 修改产品代码前，先写能暴露问题的测试并确认它在当前代码上失败（第 2 步与第 3 步适用）
5. 不得结束任何不是你启动的进程；不得访问用户的默认数据库
6. 任何安全护栏拦截时立即停下报告，不得绕过（包括增量缓存清理）
7. 遇到不明确之处、或原有测试因修改而失败，停下报告，不得修改原有测试的断言
8. 每完成一步在 docs/phase5b-progress.md 追加一行

## 第 1 步：补齐上一轮的两项确认（只读）
从进度文件或重新运行中贴出：
1. phase3d::saturated_host_does_not_split_or_take_over_queued 的两次变异验证输出（把拆分阈值改回 inits.len() 并去掉 not_started 闸门时应失败；恢复后应通过）
2. phase2 中验证"许可未饱和时卡住的分块会被接管"的测试名，及其变异验证输出（让 try_split 直接返回 false 时应失败）。如果之前没做，现在补做；如果没有这样的测试，补写一个：connections=4、每主机上限 4，前 1 个请求延迟 6 秒，断言 takeover_count >= 1 且文件正确

## 第 2 步：每主机连接上限即时生效
### 设计（必须按此实现）
1. 先确认锁定版本的 tokio::sync::Semaphore 提供 add_permits 与 forget_permits，贴出版本号和方法签名所在位置；没有 forget_permits 就停下报告
2. HostConnectionLimiter 的上限改为可变（例如 AtomicUsize），新增 `pub fn set_max_per_host(&self, n: usize)`，n 必须在 1..=32，否则返回错误
3. 调大：对每个已存在的主机信号量 add_permits(差值)，排队中的分块应立即获得许可
4. 调小：不中断已持有许可的连接。对每个主机信号量先用 forget_permits 回收当前空闲的许可；不足的部分记为"欠额"，之后每当有许可被归还时，优先抵扣欠额（归还的许可被回收而不是放回池中），直到欠额为零。目标是稳定后同时持有的许可数不超过新上限
5. 新建的主机信号量按当前上限创建
6. 下载任务内的 effective_concurrency 与 max_chunks 改为每次判断时动态计算：min(任务 connections, limiter 当前上限)，以及 max(4 × 该值, 16)。初始分块数仍在任务开始时按当时的值决定，之后不变
7. DownloadQueue 增加 `pub fn set_max_connections_per_host(&self, n: usize)`，转调 limiter
8. 桌面程序 set_settings 保存后立即调用它；选项对话框去掉"修改后重启程序生效"的提示
9. CLI 行为不变

### 测试（先写，确认在当前代码上失败或无法编译，再实现）
- limiter 单元测试 `raise_limit_releases_waiters`：上限 1，两个获取者，第二个阻塞；调到 2 后第二个在 100 毫秒内获得许可
- `lower_limit_drains_without_interrupting`：上限 4，持有 4 个许可；调到 2：已持有的 4 个不受影响；依次归还后，同时持有数最终不超过 2（再尝试获取 3 个，只能拿到 2 个）
- `lower_then_raise_settles_correctly`：4 → 1 → 3，在有许可持有和归还的过程中交替调整，最终可同时持有恰好 3 个
- 集成测试 `running_task_follows_raised_host_limit`：慢速服务器，connections=8，每主机上限 2；下载开始 3 秒后调到 6；之后 6 秒内采样：正在下载（持有许可）的分块数上升到 3 以上且不超过 6；分块总数不超过 max(4×6,16)
- 集成测试 `running_task_follows_lowered_host_limit`：connections=8，上限 8；3 秒后调到 2；10 秒后同时持有许可的分块数不超过 2，下载最终完成且文件正确
- 每个测试做一次变异验证（例如让 set_max_per_host 不调用 add_permits，或不处理欠额），贴出失败输出并恢复

## 第 3 步：暂停任务的分块显示
用户反馈：任务状态为"已暂停"，底部状态栏显示下载中 0，但详情面板的分块全部显示"下载中"。
1. 先查清原因（例如暂停后队列仍保留了旧的进度快照，task_detail 优先取了它），在进度文件中记录
2. 写测试 `paused_task_chunks_not_shown_downloading`：开始下载、暂停、确认暂停后调用 task_detail，没有任何分块为 "active" 或 "queued"；在当前代码上应失败
3. 修复：非活动任务的分块状态只能是 done（已下完）、paused（任务已暂停或失败时未下完的块）、waiting（任务排队中未下完的块）。ChunkView.state 增加 "paused"，界面显示"已暂停"，颜色灰色
4. task_detail 判断"活动任务"时，以任务当前状态为准，不能只看队列里是否残留快照

## 第 4 步：偶发失败的限速测试
phase3a::dynamic_rate_decrease_clamps_burst 在全量并行运行时偶发失败，单独运行通过。
1. 单独连续运行 30 次，以及在 cargo test --workspace 中运行 5 次，统计失败次数，贴出失败时的完整输出
2. 阅读测试与 RateLimiter 相关代码，判断失败原因属于哪一类：(a) 测试的时间容差在机器繁忙时不够；(b) 限速器在并发或调速时确实超额。给出依据（代码位置、失败时的实测数值）
3. 如果是 (a)：可以调整该测试的测量方式或容差，但必须说明调整后的测试仍能抓住什么问题，并做一次变异验证（例如让 set_rate 不清空突发额度）
4. 如果是 (b)：停下报告，附修复方案，不要修改

## 第 5 步：核实总速度显示
用户曾看到全局限速 2048 KB/秒时界面显示 4.0 MB/秒。
写测试 `reported_speed_respects_global_limit`：通过 DesktopService 设全局限速 2 MiB/秒，connections=8，本地服务器不限速，下载 24 MiB；下载过程中每 500 毫秒读取 list_tasks 的 speed_bps 与 downloaded：
- 全程平均速度（downloaded 增量 ÷ 时间）不超过 2.4 MiB/秒
- 第 2 秒之后，任何一次 speed_bps 不超过 3 MiB/秒
如果失败，查明是限速超额还是速度计算方式的问题（例如瞬时窗口太短），在报告中说明，修复速度计算可以直接做，修复限速器本身则停下报告

## 第 6 步：验证与提交
1. 新测试各单独连续运行 10 次；phase2、phase3a、phase3d、phase5b4_split_runaway 各 3 次（用退出码判定）；两个崩溃测试（--ignored）
2. cargo fmt、cargo clippy -D warnings、npm run build、verify-workspace.ps1、phase4b-e2e.ps1 全部 PASS
3. 暂存 docs apps crates，提交信息 phase5b-4c: live host limit, paused chunk state, rate test triage，不 push、不 amend

## 报告
1. 第 1 步两项确认的输出
2. tokio 版本与 Semaphore 方法确认；第 2 步欠额机制的实现位置
3. 每个新测试修改前的失败输出与变异验证输出
4. 第 3 步原因与修复位置
5. 第 4 步统计数据、分类结论与依据
6. 第 5 步测试结果
7. 验证结果、提交 hash、手动验收建议、与本文不一致之处
