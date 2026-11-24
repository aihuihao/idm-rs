# idm-rs 0.1.1 版本说明

本次为遗留问题修复版本，程序版本号由 0.1.0 升至 0.1.1；Firefox 扩展未改动，版本仍为 0.1.0。

## 本次修复

- **`idm-cli enqueue --out` 生效**：`--out NAME` 指定的文件名写入入队任务的 `file_name`，下载完成后使用该名称；路径含分隔符时只取最后一段，先经 `sanitize_windows_filename` 处理，结果为空则报错并以退出码 2 退出。例：`--out "..\\evil.bin"` 只生成 `evil.bin`。
- **`idm-cli queue pause <id>` / `queue resume <id>`**：排队中（Queued）任务可暂停为 `Paused(User)`（输出 `paused <id>`）；下载中（Downloading/Finalizing/Probing）任务不修改，提示改用桌面程序或 `idm-cli runner stop`，退出码 3；其他状态输出原因并以退出码 2 退出。`resume` 可将 `Paused`/`Failed` 任务改回 `Queued`（输出 `resumed <id>`），其他状态退出码 2。
- **Native Messaging host 读取消息超时**：读到 4 字节长度前缀后，若消息体 5 秒内未读完，host 写一行 `nmhost.log` 并以非 0 退出；等待下一条消息的空闲阶段不设超时，Firefox 关闭标准输入时自然结束。
- **验收脚本断言修正**：`phase4a-acceptance.ps1` 中 `validate_url` 两项指向迁移后的 `idm-core` 实际位置；移除已不存在的 `host_spawns_runner_once`（host 不再启动运行器）。
- **减小开发构建体积**：workspace `profile.dev` 改用 `debug = "line-tables-only"`，依赖包关闭 debug 信息；`idm-core` 集成测试程序 `phase3d` 由 13,841,920 字节降至 12,853,248 字节（约 -7.1%）。
- **维护说明**：新增 `docs/maintenance.md`，记录自定义 NSIS 模板来源与修改、Tauri 升级同步步骤、发布检查清单。

## 已知限制

- **仅支持 Firefox**：浏览器接管扩展只提供 Firefox 版本（Manifest V2），不支持 Chrome/Edge。
- **程序未做代码签名**：安装时 Windows 可能显示"未知发布者"提示，需用户手动确认继续。
- **Tauri 升级时需同步自定义安装模板**：当前使用自定义 NSIS 模板 `apps/idm-desktop/src-tauri/nsis/installer.nsi` 与卸载钩子 `nsis/hooks.nsh`（调整安装目录、清理 host 与开机启动项）。升级 Tauri 后需核对内嵌模板变化并同步这些自定义文件，否则安装目录/卸载行为可能回退。详见 `docs/maintenance.md`。
