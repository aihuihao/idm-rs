# 任务：idm-rs Phase 5b-4d —— 继续下载时可选用当前默认连接数

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5b-4d.md
2. 不联网，不新增依赖
3. 不得使用 #[allow]、unsafe、unwrap()、expect()
4. 先写测试并确认在当前代码上失败（或无法编译），再实现；每个新测试做一次变异验证
5. 不得结束任何不是你启动的进程；不得访问用户的默认数据库
6. 任何安全护栏拦截或需要审批超时，立即停下报告，不得绕过
7. 遇到不明确之处、现有 API 不够用、或原有测试因修改而失败，停下报告，不得修改原有测试的断言
8. 每完成一步在 docs/phase5b-progress.md 追加一行

## 背景
用户反馈：任务以 16 段创建，暂停后把"每任务默认连接数"改为 8 再继续，任务仍按 16 下载。IDM 的习惯是暂停再继续后采用新的连接数。用户要求做成选项。

## 第 1 步：设置项
1. 新增设置键 resume-uses-current-connections，默认为真
2. Settings 增加字段 resume_uses_current_connections: bool
3. 选项对话框中"每任务默认连接数"下方增加复选框"继续下载时使用当前的默认连接数"，下方灰色小字："勾选后，暂停的任务继续时改用上面的连接数；已下载的分段保留"
4. 测试 resume_connections_setting_roundtrip：默认为真，写入假后读回为假

## 第 2 步：继续时更新任务连接数
1. DesktopService::resume_task：设置为真时，在恢复任务之前，把该任务在数据库中的 connections 更新为当前的 default-connections；设置为假时不改
2. 如果 Store 没有更新单个任务连接数的公开方法，允许新增 `pub async fn set_task_connections(&self, id: i64, n: usize) -> Result<()>`，只更新 tasks 表的 connections 列，不得改动分块记录
3. 已存在的分段一律保留，不得重新切分或丢弃已下载的数据
4. 只影响桌面程序的"继续"操作；CLI 与浏览器接管的行为不变
5. 先确认：恢复下载时，下载引擎读取的连接数是否来自数据库中任务的 connections，以及动态拆分的上限是否按 min(任务 connections, 每主机上限) 实时计算。在报告中给出代码位置。如果不是，停下报告

## 第 3 步：测试
都用慢速本地服务器，每主机上限设为 8：
1. resume_lowers_connections：以 connections=8 创建并开始下载，暂停，把默认连接数改为 2，继续。断言：数据库中任务 connections 变为 2；继续后每 500 毫秒采样，同时持有许可（正在下载）的分段数不超过 2；分段总数与暂停前相同（未重新切分）；最终文件逐字节正确
2. resume_raises_connections：以 connections=2 创建，暂停，默认连接数改为 6，继续。断言：connections 变为 6；继续后 8 秒内，正在下载的分段数达到 3 以上且不超过 6；最终文件正确
3. resume_keeps_connections_when_disabled：设置为假，以 connections=8 创建，暂停，默认改为 2，继续。断言：connections 仍为 8
4. 变异验证：(a) 让 resume_task 不更新 connections，测试 1、2 应失败；(b) 让设置判断取反，测试 3 应失败。贴出输出后恢复

## 第 4 步：详情面板标题
1. TaskDetail 增加字段：segments（分段数）、downloading（正在下载的分段数）、queued（排队的分段数）、max_concurrent（min(任务 connections, 当前每主机上限)）
2. 详情面板标题改为："分段 N　下载中 a　排队 b　最多同时 c"，去掉原来的"连接数 32"
3. 非活动任务的 downloading 与 queued 为 0
4. 在测试 1 中顺带断言：继续下载过程中 max_concurrent 为 2，downloading 不超过 2

## 第 5 步：验证与提交
1. 新测试各单独连续运行 10 次；phase5b4_split_runaway、phase3d、桌面全部测试各运行 1 次
2. cargo fmt、cargo clippy -D warnings、npm run build、verify-workspace.ps1、phase4b-e2e.ps1 全部 PASS
3. 暂存 docs apps crates，提交信息 phase5b-4d: optional resume with current connections，不 push、不 amend

## 报告
1. 第 2 步第 5 条的代码位置确认
2. 每个新测试修改前的失败输出与变异验证输出
3. 新增或修改的公开方法签名
4. 验证结果、提交 hash
5. 手动验收步骤（写成用户能照做的：创建任务、暂停、改默认连接数、继续，观察详情面板标题变化；取消勾选后重复，观察不变）
6. 与本文不一致之处
