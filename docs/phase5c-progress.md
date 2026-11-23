# Phase 5c 进度

## Phase 5c-2：首次运行自动配置

- 步骤 0：确认 git status 干净（HEAD=2eba0a87 phase5c-1），任务原文已原样保存 docs/prompts/phase5c-2.md。
- 步骤 1（nm 上移）：新建 crates/idm-core/src/nm_host.rs，迁移 CLI nm 的注册表/manifest 读写；核心无打印（core 顶层 warn print_stdout），返回 Status 数据；CLI nm 模块保留参数解析/路径计算/打印/退出码，改为调用 nm_host::{install,uninstall,status_in}。lib.rs 注册模块。core nm 单测 2/2 PASS；phase4b 的 7 个 nm 测试 7/7 PASS（含 JSON 5 字段、幂等、非法输入、缺 exe、卸载状态），证明 CLI 行为与输出不变。修复一处 ids JSON 少引号导致的 2 个失败后通过。
- 步骤 2（桌面集成状态）：service.rs 新增 BrowserIntegration(NotInstalled/Ok/Broken)、IntegrationConfig(host_name/manifest_dir/program_dir/current_exe 可注入)、open_with_config；三方法 browser_integration_status/enable/disable 观察模式允许；Settings 加 browser_integration_dismissed 并贯通 read/set_settings。Broken 判定：manifest host 路径不存在或与当前程序目录 nmhost 不同。
- 步骤 3（启动自动处理）：service.rs 新增 is_dev_build 纯函数、same_path/autostart_exe_path 辅助、auto_fix_browser_integration(Broken 且非 dev → enable)、refresh_autostart_path(Run 值路径非当前程序且非 dev → 更新并保留 --minimized)；失败返回消息供 main 记日志不阻止启动。main.rs setup 接线待做。
- 步骤 5（新测试）：新增 6 个测试（is_dev_build_variants、integration_status_variants、enable_fixes_broken、enable_without_nmhost_fails、autostart_path_refresh、autostart_path_refresh_absent_noop），随机 host 名 + 临时目录 + Drop 守卫清理注册表/manifest，不用默认 host/Run 名。发现并修复真实 bug：nm_host::extract_host_path 未反转义 JSON \\，导致 Windows 路径判定 Broken；加 json_unescape 后 6/6 PASS。
- 步骤 5.6（变异验证）：(A) Broken 判断忽略文件存在 → integration_status_variants、enable_fixes_broken 均 FAIL（left Ok/right Broken），恢复；(B) is_dev_build 恒为假 → autostart_path_refresh 开发构建分支 FAIL「开发构建不应刷新路径」，恢复。恢复后搜索 MUT-A/B=0 命中。
- 步骤 3 接线：main.rs setup 阶段 spawn 异步任务调用 auto_fix_browser_integration + refresh_autostart_path，结果通过 startup_auto_log 写数据目录 desktop-startup.log（失败静默、不阻止启动）；新增 3 个 tauri command（browser_integration_status/enable/disable）并注册 invoke_handler；cargo check --all-targets EXIT=0。
- 步骤 4（界面）：App.vue 顶部提示条（NotInstalled 且未 dismissed 时显示「浏览器集成未启用，Firefox 下载不会被接管」+ 启用/不再提示）；选项对话框新增浏览器集成区域（状态文字、启用/修复或移除按钮、灰色小字）；新增 loadBrowserStatus/onEnableBrowser/onDisableBrowser/onDismissBrowserBanner，onMounted 与 openOptions 刷新状态；Settings 接口贯通 browser_integration_dismissed；无 v-html/innerHTML/eval；npm run build EXIT=0。
- 步骤 5.7（循环）：6 个新测试各连续 10 次全 PASS（PASS=10 FAIL=0）；修复新增测试中 3 处 unwrap → ok_or_else（规则 3）；原有 phase4b 7/7、桌面全部 33/33 各 1 次 PASS。
- 步骤 6（验证）：cargo fmt（--check PASS）、cargo clippy -D warnings（修复 startup_auto_log 的 &PathBuf→&Path 后 PASS）、npm run build PASS、verify-workspace.ps1 全 24 步 PASS（修复 nm_host 文档注释尖括号导致的 rustdoc invalid-html-tags）、phase4b-e2e.ps1 PASS（残留进程 0）、scripts/build-release.ps1 重新生成安装包 idm-rs_0.1.0_x64-setup.exe 8120574 字节（7.74 MiB）。规则扫描：新增 Rust 与测试代码无 unwrap/expect/allow/unsafe，前端无 v-html/innerHTML/eval。note: verify-workspace 首次调用因参数遗漏触发审批超时，修正审批参数后重跑同一命令通过，未绕过任何护栏。
- 步骤 6（提交）：暂存 docs apps crates scripts，提交信息 phase5c-2: first-run browser integration and autostart path。
