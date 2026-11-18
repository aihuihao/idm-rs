# 任务：idm-rs Phase 5a-4 —— 界面、托盘、观察模式

## 规则（必须遵守）
1. 先确认 `git status` 干净，把本任务原文原样保存为 `docs/prompts/phase5a-4.md`
2. 不新增 npm 包，不新增 crate。只允许给已有的 `tauri` 依赖开启托盘所需的 feature（`tray-icon`）；如果因此需要联网拉取新 crate，允许执行一次 `cargo fetch`，之后核对 Cargo.lock：原有包版本必须全部不变，报告新增包数量
3. 不得使用 `#[allow]`、unsafe、`unwrap()`、`expect()`；前端不得使用 `v-html`、`innerHTML`、`eval`
4. 任何安全护栏或权限拦截时立即停下报告，不得绕过
5. 遇到不明确之处、或任何检查不通过，立即停下报告
6. 每完成一步在 `docs/phase5a-progress.md` 追加一行
7. 不要启动桌面程序窗口，界面验收由用户手动完成

## 第 1 步：修复删除正在下载的任务
`DesktopService::remove_task`：如果任务正在下载或排队中，先通过队列让它停下（使用 idm-core 现有的取消或暂停 API），等它不再活动后，再删除数据库记录和文件。现有 API 做不到就停下报告。
补测试 `remove_active_task`：用慢速本地服务器开始下载，下载中途调用 `remove_task(id, true)`：返回成功；任务从列表消失；目录里没有 .idm-part 和最终文件；之后再添加一个新任务仍能正常下载完成。

## 第 2 步：运行器停止时程序退出
运行器模式下，后台等待 `Runner::wait_stopped()`（即用户执行了 `idm-cli runner stop`）：触发后调用 shutdown 并退出程序。如果 DesktopService 目前没暴露这个能力，给它增加一个方法 `pub async fn wait_stopped(&self)`（观察模式下永不返回），在报告中记录。

## 第 3 步：托盘与窗口行为（Rust 侧）
1. 托盘图标使用已有的应用图标，提示文字 `idm-rs`
2. 左键单击：窗口隐藏则显示并置于前台，显示则隐藏
3. 右键菜单两项："显示主窗口"、"退出"
4. 点击窗口右上角关闭按钮：只隐藏窗口，不退出程序
5. "退出"：调用 DesktopService 的 shutdown（活动任务变为 Paused(User)），完成后退出程序
6. capabilities 保持只有 `core:default`

## 第 4 步：界面（前端，全部中文）
页面从上到下：
1. **观察模式横幅**：`is_read_only` 为真时显示黄色横幅："后台运行器正在运行，界面为只读。请先执行 idm-cli runner stop 后重启本程序。"同时禁用所有操作按钮和输入框
2. **添加栏**：网址输入框 +"添加"按钮。添加失败时在输入框下方用红字显示错误信息，成功后清空输入框
3. **任务表格**，列：
   - 文件名（没有时显示 url_display）
   - 大小（自动换算 B / KB / MB / GB，保留 1 位小数；未知显示"未知"）
   - 进度（进度条 + 百分比；大小未知时只显示已下载量）
   - 速度（同样自动换算单位，加"/秒"；为 0 时显示"—"）
   - 剩余时间（由剩余大小除以当前速度计算，显示为"时:分:秒"；无法计算时显示"—"）
   - 状态（中文：排队中、下载中、已暂停、已完成、失败；暂停原因为用户暂停时显示"已暂停"，为中断时显示"已中断"；失败时鼠标悬停显示 last_error）
   - 操作：暂停、继续、删除。按钮只在当前状态可用时可点
4. **删除确认**：自己实现一个简单的确认框（不用浏览器自带的 confirm），包含一个"同时删除已下载的文件"复选框，默认不勾选
5. **设置面板**（页面底部可折叠）：下载目录（文本框）、全局限速（单位 KB/秒，0 表示不限速）、同时下载任务数（1~16）、每任务默认连接数（1~32）、"保存"按钮。保存失败用红字显示错误
6. **数据刷新**：启动时调用一次 `list_tasks`；之后监听 `tasks-updated` 事件。注意该事件的内容是 JSON 字符串，需要先 `JSON.parse` 再使用
7. 纯 CSS，风格朴素清晰即可，美化留到 5b

## 第 5 步：静态检查（加入 verify-workspace.ps1，各自独立成步）
1. `apps/idm-desktop/src` 下的 `.vue`、`.ts` 文件不含 `v-html`、`innerHTML`、`eval(`、`new Function`
2. `tauri.conf.json` 的 CSP 以 `default-src 'self'` 开头
3. `capabilities` 目录下所有 json 文件的权限只有 `core:default`

## 第 6 步：验证
1. `remove_active_task` 单独连续运行 10 次；原有 12 个桌面测试各运行 1 次
2. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `verify-workspace.ps1` 全部 PASS
4. `phase4b-e2e.ps1` 运行 1 次 PASS

## 第 7 步：提交
暂存 `docs apps scripts crates Cargo.toml Cargo.lock .gitignore`，提交信息 `phase5a-4: desktop ui, tray, observer mode`。不 push，不打 tag，不 amend。

## 报告（按顺序）
1. 第 1 步使用了 idm-core 的哪个 API 让活动任务停下
2. 托盘 feature 是否导致 Cargo.lock 新增包，数量多少，原有版本是否不变
3. 测试结果、verify 步骤与结果、e2e 结果
4. 新增和修改的文件（每个一句话）
5. 给用户的手动验收步骤（见下方要求）
6. 提交 hash
7. 与本文不一致之处

手动验收步骤请写成用户能直接照做的清单，包括：如何停止 CLI 运行器、如何启动桌面程序、添加一个小文件下载、暂停/继续/删除、修改限速、关闭窗口后从托盘恢复、托盘退出、观察模式的验证方法。
