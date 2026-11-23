# idm-rs 0.1.0 版本说明

## 主要功能

- 桌面程序（Tauri）：任务列表、详情（分段状态、正在下载/排队/最大并发）、添加/暂停/继续/删除任务、清除已完成记录、打开下载文件所在目录。
- 下载引擎：多任务队列、分段下载、断点续传、动态拆分、每任务连接闸门、每主机连接上限、全局限速、速度平滑显示。
- 设置：下载目录、每任务默认连接数、继续时使用当前连接数、每台服务器最大连接数、全局限速、开机启动、完成通知、浏览器集成提示开关。
- 浏览器集成：Firefox Native Messaging host（`idm_rs`），扩展把 http/https 下载交给本机程序；按失败原因给出中文提示；启动时自动修复损坏的 host 路径，并刷新开机启动的程序路径。
- 命令行工具 `idm-cli`：下载、续传、列出、删除、入队、队列运行/移动、配置、runner、nm（install/uninstall/status）等子命令。
- 安装包：NSIS 当前用户模式安装，`idm-cli.exe` 与 `idm-nmhost.exe` 与桌面程序同目录；卸载时清理程序与注册表，保留数据目录 `%LOCALAPPDATA%\idm-rs`。

## 已知限制

- **仅支持 Firefox**：浏览器接管扩展只提供 Firefox 版本（Manifest V2），不支持 Chrome/Edge。
- **程序未做代码签名**：安装时 Windows 可能显示"未知发布者"提示，需用户手动确认继续。
- **`idm-cli enqueue --out` 未生效**：`enqueue` 解析了 `--out` 参数但未应用到入队任务的文件名；需要指定输出名时请使用 `download --out`。
- **`idm-cli queue` 没有 `pause` 子命令**：队列子命令只有 `run` 与 `move`；暂停任务请通过桌面程序或 `idm-cli` 的其他可用入口。
- **Native Messaging host 读取消息无超时**：`idm-nmhost` 的主循环直接阻塞读取标准输入，未设置读取超时；若浏览器端长时间不发消息，读取会一直等待（标准输入关闭时正常退出）。
- **Tauri 升级时需同步自定义安装模板**：当前使用自定义 NSIS 模板 `apps/idm-desktop/src-tauri/nsis/installer.nsi` 与卸载钩子 `nsis/hooks.nsh`（调整安装目录、清理 host 与开机启动项）。升级 Tauri 后需核对内嵌模板变化并同步这些自定义文件，否则安装目录/卸载行为可能回退。
