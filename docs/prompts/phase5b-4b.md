# 任务：idm-rs Phase 5b-4b —— 修复分块拆分失控

## 背景
诊断结论见 docs/phase5b-progress.md 的 5b-4a 部分。根因：拆分条件 `in_flight < connections` 中，in_flight 扣除了等待主机许可的分块（split.rs:489-502）；当任务连接数大于每主机上限时，条件恒真，新分块不断被拆出并排队，且分块总数没有上限。另外桌面程序把每主机上限错设为"每任务默认连接数"（service.rs:310）。

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5b-4b.md
2. 不联网，不新增依赖
3. 不得使用 #[allow]、unsafe、unwrap()、expect()
4. **先写测试、后改代码**：第 1 步的回归测试必须在修改任何产品代码之前写好并运行，确认它在当前代码上失败，贴出失败输出。如果它在当前代码上通过，说明没有复现问题，停下报告，不要继续
5. 不得结束任何不是你启动的进程；不得访问用户的默认数据库
6. 任何安全护栏拦截时立即停下报告
7. 遇到不明确之处、或原有测试因修复而失败，停下报告，不得修改原有测试的断言
8. 每完成一步在 docs/phase5b-progress.md 追加一行

## 第 1 步：写回归测试（修改代码之前）
在 crates/idm-core/tests/ 新建 phase5b4_split_runaway.rs：
1. 先确认本地测试服务器的限速确实生效：单独一个辅助断言，用 SpeedPolicy 设 256 KiB/s 下载 2 MiB，耗时必须不少于 6 秒。如果限速不生效，先查清原因并在报告里说明（5b-4a 的复现中 64 MiB 在 2 秒内下完，说明之前的用法有误）
2. 主测试 `split_does_not_run_away_when_host_limited`：服务器每连接 128 KiB/s，文件 64 MiB，任务 connections=32，每主机上限 4，下载 20 秒后取消。每 2 秒采样一次分块数，断言：
   - 分块总数始终不超过 2 × min(connections, 每主机上限) + 一个小余量（在报告中说明你取的具体值和理由）
   - 任意时刻"从未开始下载（已下载为 0）"的分块数不超过 min(connections, 每主机上限)
3. 在当前代码上运行，确认失败，贴出每 2 秒的分块数

## 第 2 步：修复拆分逻辑（idm-core）
1. 引入"有效并发数" = min(任务 connections, 每主机上限)。初始分块数、拆分阈值都改用它
2. 拆分前增加闸门：从未开始下载的分块数 ≥ 有效并发数时不拆分
3. 分块总数硬上限：超过 4 × 有效并发数（且至少 16）时不再拆分
4. 不改变"排队块不被误判为卡住"这一 Phase 3d 的修复目的：被扣除的排队块仍然不能成为接管对象
5. 在代码注释中写明以上三条规则及原因

## 第 3 步：排队状态可见
1. 进度快照中让分块能区分"排队等待主机许可"与"正在下载"。如果现有快照没有这个信息，允许新增一个只读字段，不得改变下载逻辑
2. 桌面程序 ChunkView.state 增加 "queued"，界面中文显示"排队"，颜色与"等待"相同

## 第 4 步：每主机连接数设置（桌面程序）
1. 新增设置键 max-connections-per-host，默认 8，范围 1~32
2. service.rs:310 改为读取这个设置，不再使用 default-connections
3. 修改后立即生效（如果 HostConnectionLimiter 不支持运行中修改，停下报告）
4. Settings 增加字段 max_connections_per_host；选项对话框增加"每台服务器最大连接数（1~32）"
5. CLI 的行为保持不变

## 第 5 步：测试与验证
1. 第 1 步的测试修复后通过，单独连续运行 10 次
2. idm-core 中所有与拆分、接管、主机许可相关的原有测试（phase2、phase3d 等）各运行 3 次，全部通过
3. 新增桌面测试 host_limit_setting_roundtrip（默认 8、范围校验、保存后生效）
4. cargo fmt、cargo clippy -D warnings、npm run build、verify-workspace.ps1 全部 PASS；phase4b-e2e.ps1 运行 1 次 PASS；两个崩溃测试通过
5. 暂存 docs apps crates scripts，提交信息 phase5b-4b: fix split runaway under host limit，不 push、不 amend

## 报告
1. 第 1 步：限速确认结果；修复前测试的失败输出（每 2 秒分块数）
2. 第 2 步每条规则的代码位置
3. 修复后同一测试的每 2 秒分块数
4. 测试自查表；verify、e2e、崩溃测试结果
5. 手动验收建议
6. 与本文不一致之处
