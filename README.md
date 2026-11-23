# idm-rs 下载管理器

idm-rs 是一个本地下载管理器，由桌面程序（Tauri）、命令行工具和 Firefox 扩展组成。桌面程序负责下载调度、分段与限速；Firefox 扩展把浏览器中的 http/https 下载交给本机程序接管。

## 功能简介

- 多任务下载，支持断点续传与分段下载（分段数默认 8，可调 1–32）。
- 每台服务器连接数限制、全局限速、每任务连接数设置。
- Firefox 下载接管：浏览器点击下载后，任务出现在 idm-rs 中；未成功接管时下载仍由 Firefox 正常进行。
- 托盘常驻、开机自启、下载完成通知。
- 数据（下载记录、设置、日志）保存在 `%LOCALAPPDATA%\idm-rs`，卸载时不删除。

当前版本仅支持 Windows 与 Firefox。

## 安装

1. 运行安装包 `idm-rs_0.1.0_x64-setup.exe`。安装采用"当前用户"模式，默认安装到 `%LOCALAPPDATA%\Programs\idm-rs`，不需要管理员权限。
2. Windows 可能弹出"未知发布者"（Windows 已保护你的电脑）提示：这是因为程序尚未做代码签名。确认发布者来源为你自己构建/信任的副本后，点击"更多信息"→"仍要运行"继续安装。
3. 安装完成后，开始菜单出现 `idm-rs` 快捷方式；安装目录含 `idm-desktop.exe`、`idm-cli.exe`、`idm-nmhost.exe` 三个程序。

## 首次使用

1. **启用浏览器集成**：打开 idm-rs →「选项」→「浏览器集成」，点击「启用」（或点击界面顶部的提示条「启用」）。这会把 `idm-nmhost.exe` 注册为本机 Native Messaging host，供 Firefox 扩展连接。
2. **安装 Firefox 扩展**：在 Firefox 中临时载入扩展目录 `extensions/firefox`（开发者工具方式），或安装经 Mozilla 签名后的 `.xpi`。扩展启用后，工具栏图标显示 `ON`。

接管已开启/关闭可通过点击工具栏图标切换；图标上的 `!` 表示最近一次交接失败（见常见问题）。

## 常用设置

在「选项」对话框中设置，保存后立即或重启生效：

- **下载目录**：新任务的默认保存位置。
- **每任务默认连接数（1–32）**：单个任务的默认分段/连接数。
- **继续下载时使用当前的默认连接数**：勾选后，暂停任务点「继续」时，改用当前的默认连接数；已下载分段保留，不重新切分。
- **每台服务器最大连接数（1–32）**：所有任务共享的每主机连接上限，保存后立即对正在下载的任务生效。
- **全局限速（KB/秒，0 表示不限速）**：所有任务的总速度上限。
- **开机自动启动（以托盘方式启动）**：开机后以托盘图标方式启动，主窗口默认隐藏。
- **下载完成时通知**：任务完成/失败时发送系统通知。

## 卸载

通过「设置 → 应用 → 安装的应用」卸载 idm-rs，或运行安装目录中的 `uninstall.exe`。卸载会删除程序文件、开始菜单快捷方式、浏览器 host 注册和开机启动项，但**不会删除数据目录** `%LOCALAPPDATA%\idm-rs`——下载记录、设置和日志会保留，以便重装后继续下载。浏览器集成清单 `idm_rs.json` 会在卸载时清理。

## 常见问题

**扩展图标显示 `!`（交接失败）**：把鼠标移到工具栏图标可看到原因提示：
- "未连接到 idm-rs"：本机程序未注册，请打开 idm-rs →选项→浏览器集成，点击启用。
- "idm-rs 无响应"：请确认 idm-rs 正在运行。
- "运行器未启动"：请执行 `idm-cli runner start`。
- "交接失败，已交给 Firefox 下载"：本次未接管成功，下载仍由 Firefox 正常进行，可稍后重试。

失败后约 30 秒内的新下载会直接交给 Firefox，不会反复尝试接管。

**下载没有被接管**：确认扩展为 `ON`、浏览器集成状态为"已启用"、idm-rs 正在运行；`blob:`、`data:`、`file:`、`about:` 等非 http/https 链接不会被接管。本扩展只支持 Firefox。

**界面顶部出现"后台运行器正在运行，界面为只读"横幅**：这是观察模式——另一个 idm-cli 运行器正占用任务队列。此时界面只能查看，不能新增/操作下载。执行 `idm-cli runner stop` 后重启桌面程序即可回到可操作模式。

## 开发者信息

- 技术栈：Rust（edition 2024，MSVC `x86_64-pc-windows-msvc`）+ Tokio；桌面端 Tauri 2 + Vue 3；状态持久化 rusqlite（内嵌 SQLite）。
- Crate 职责：`crates/idm-core` 为纯下载引擎；`crates/idm-cli` 为命令行验证工具；`crates/idm-ipc` 负责 Native Messaging host。
- 常用命令：

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
pwsh -File scripts/verify-workspace.ps1
pwsh -File scripts/package-extension.ps1   # 生成 dist/idm-rs-firefox-0.1.0.zip
```

更完整的功能与限制说明见 `docs/release-notes-0.1.0.md` 与各阶段进度文件 `docs/phase5b-progress.md`、`docs/phase5c-progress.md`。
