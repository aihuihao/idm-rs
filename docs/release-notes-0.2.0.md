# idm-rs 0.2.0 版本说明

本次主要特性：安装包自带经 Mozilla 签名的 Firefox 扩展，并支持在桌面程序中一键安装。程序版本号由 0.1.1 升至 0.2.0；Firefox 扩展未改动，版本仍为 0.1.0。

## 本次新增与修复

- **安装包自带签名扩展**：`build-release.ps1` 校验 `dist/idm-rs-firefox-0.1.0-signed.xpi` 的 SHA256（期望值单一来源为脚本顶部 `$ExtensionSha256`），随 NSIS 安装包安装为 `%LOCALAPPDATA%\Programs\idm-rs\idm-rs-firefox.xpi`（安装目录根），卸载时自动清理。
- **Firefox 检测**：只读检测 Firefox 安装路径（注册表 HKLM/HKCU 的 App Paths，回退 `%ProgramFiles%` / `%ProgramFiles(x86)%`），并解析 `%APPDATA%\Mozilla\Firefox\profiles.ini`（支持相对/绝对路径）判断扩展是否已安装到各配置文件夹。
- **一键安装扩展**：选项对话框「浏览器集成」区显示 Firefox 扩展状态；未安装或未检测到 Firefox 时可点击「安装 Firefox 扩展」，用 firefox.exe 打开随包分发的 xpi，由 Firefox 弹出安装确认窗口。开发构建从仓库 `dist/` 取签名 xpi（配置注入）。
- **首次运行提示**：浏览器集成已启用但扩展未安装时，顶部横幅提示「Firefox 扩展未安装」并提供「安装」按钮；「不再提示」沿用现有设置。
- **竞态核实**：确认运行器调度器在拉起任务前重新查询数据库（`WHERE state='Queued'`），排队的任务被 CLI 暂停后不会被启动；新增回归测试 `pause_queued_while_runner_running`。

## 已知限制

- **仅支持 Firefox**：浏览器接管扩展只提供 Firefox 版本（Manifest V2），不支持 Chrome/Edge。
- **程序未做代码签名**：安装时 Windows 可能显示"未知发布者"提示，需用户手动确认继续。
- **扩展安装需用户确认**：一键安装只负责用 Firefox 打开 xpi，仍需在 Firefox 弹出的窗口中手动点击「添加」；建议不勾选「允许在隐私窗口中运行」（隐私窗口下载会被记入 idm-rs 下载记录）。
- **Tauri 升级时需同步自定义安装模板**：详见 `docs/maintenance.md`。
