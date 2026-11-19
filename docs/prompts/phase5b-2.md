# 任务：idm-rs Phase 5b-2 —— 单实例、完成通知、选择文件夹

## 规则（必须遵守）
1. 先确认 `git status` 干净，把本任务原文原样保存为 `docs/prompts/phase5b-2.md`
2. 允许新增且只允许新增这三个 crate：`tauri-plugin-single-instance`、`tauri-plugin-notification`、`tauri-plugin-dialog`（均为 2.x，版本必须与现有 tauri 兼容）。不新增任何 npm 包
3. 允许执行一次 `cargo fetch`。之后核对 Cargo.lock：原有包版本必须全部不变，报告新增包数量。如果插件要求升级已有的 tauri 或其他包版本，停下报告
4. 插件只在 Rust 端调用。capabilities 必须仍然只有 `core:default`，不得为插件添加任何前端权限
5. 不得使用 `#[allow]`、unsafe、`unwrap()`、`expect()`；前端不得使用 `v-html`、`innerHTML`、`eval`
6. 任何安全护栏或权限拦截时立即停下报告，不得绕过
7. 遇到不明确之处或任何检查不通过，立即停下报告
8. 每完成一步在 `docs/phase5b-progress.md` 追加一行。不要启动桌面程序窗口

## 第 1 步：单实例
1. 使用 `tauri-plugin-single-instance`，并按其文档要求作为第一个注册的插件
2. 第二次启动程序时，第二个进程直接退出；已运行的实例把主窗口显示出来、取消最小化并置于前台
3. 如果第二次启动带有 `--minimized` 参数，已运行的实例不做任何事
4. 注意：这与观察模式不冲突。观察模式针对的是"CLI 运行器在运行"的情况，单实例针对的是"桌面程序已在运行"的情况

## 第 2 步：下载完成通知
1. `Settings` 增加 `notify_on_complete: bool`，数据库设置键 `notify-on-complete`，默认为真
2. 写一个不依赖 Tauri 的纯函数，放在 service 或新模块里：
   `pub fn finished_since(prev: &[TaskView], now: &[TaskView]) -> Vec<(i64, String, bool)>`
   返回在 prev 中不是已完成/失败、在 now 中变成已完成（true）或失败（false）的任务 id、显示名称。prev 中不存在的任务（新出现的）不算
3. 在现有的 250 毫秒推送循环中调用它；设置开启时，通过 `tauri-plugin-notification` 的 Rust API 发送通知：完成时标题"下载完成"、正文为文件名；失败时标题"下载失败"、正文为文件名
4. 程序启动后第一次获取列表时不发通知（避免把历史任务全报一遍）
5. 观察模式下不发通知
6. 设置面板增加"下载完成时通知"复选框

## 第 3 步：选择文件夹
1. 新增 Tauri 命令 `pick_download_dir`：在 Rust 端用 `tauri-plugin-dialog` 的 Rust API 弹出文件夹选择框，返回选中的路径（取消则返回空）。不在前端直接调用对话框
2. 设置面板下载目录输入框右侧加"选择…"按钮，选中后填入输入框（仍需点"保存"才生效）
3. 观察模式下按钮禁用

## 第 4 步：测试
1. `finished_since` 单元测试：排队变完成、下载中变失败、本来就已完成的不重复报、新出现且已完成的不报、暂停的不报、空列表
2. `notify_setting_roundtrip`：设置读写往返，默认值为真
3. 新测试各单独连续运行 10 次；原有桌面测试全部运行 1 次
4. 单实例、通知弹出、文件夹对话框不做自动测试，写进手动验收清单

## 第 5 步：验证与提交
1. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`npm run build`
2. `verify-workspace.ps1` 全部 PASS（其中 capabilities 只有 `core:default` 的检查必须继续通过）；`phase4b-e2e.ps1` 运行 1 次 PASS
3. 暂存 `docs apps scripts crates Cargo.toml Cargo.lock .gitignore`，提交信息 `phase5b-2: single instance, notifications, folder picker`，不 push、不 amend

## 报告（按顺序）
1. 三个插件的版本；Cargo.lock 新增包数量；原有包版本是否不变
2. 每步修改位置（文件:行号）
3. 测试自查表
4. verify 与 e2e 结果
5. 手动验收清单：单实例（双击两次、带 --minimized 再启动一次）、完成通知、失败通知（可用一个不存在的网址触发）、通知开关关闭后不弹、选择文件夹并保存
6. 提交 hash
7. 与本文不一致之处
