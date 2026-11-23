# 任务：idm-rs Phase 5c-2 —— 首次运行自动配置（浏览器集成、开机启动路径）

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5c-2.md
2. 不联网，不新增依赖
3. 不得使用 #[allow]、unsafe、unwrap()、expect()；前端不得使用 v-html、innerHTML、eval
4. 测试中写注册表或 manifest 一律使用随机测试名与临时目录，Drop 守卫清理；绝不使用默认 host 名 idm_rs、默认值名 idm-rs-desktop / idm-rs-runner，绝不写入用户的默认数据目录
5. 不得结束任何不是你启动的进程；不得在本机执行安装包
6. 任何安全护栏拦截或审批超时，立即停下报告，不得绕过
7. 遇到不明确之处、或原有测试失败，停下报告，不得修改原有测试的断言
8. 每完成一步在 docs/phase5c-progress.md 追加一行

## 第 1 步：把浏览器集成注册逻辑移到 idm-core
1. 把 idm-cli 中 nm install / uninstall / status 的注册表与 manifest 读写逻辑原样移到 idm-core 新模块 nm_host（与 5b-1 移动 autostart 的做法一致），CLI 改为调用它，输出与行为完全不变
2. 公开接口至少包括：install(host_name, host_exe_path, extension_ids, manifest_dir)、uninstall(host_name, manifest_dir)、status(host_name) -> 包含"是否注册、manifest 路径、manifest 中的 host 路径、该路径的文件是否存在"
3. phase4b 中原有的 nm 相关测试必须继续通过

## 第 2 步：桌面程序的集成状态
1. DesktopService 新增：
   - `browser_integration_status() -> BrowserIntegration`，取值：NotInstalled、Ok、Broken（已注册但 manifest 中的 host 路径文件不存在，或与当前程序目录下的 idm-nmhost.exe 不同）
   - `enable_browser_integration()`：用当前程序目录下的 idm-nmhost.exe 注册（host 名 idm_rs，扩展 ID idm-rs@local，manifest 放在数据目录，与 CLI 一致）；如果 idm-nmhost.exe 不存在，返回 InvalidInput("找不到 idm-nmhost.exe")
   - `disable_browser_integration()`
2. 观察模式下这三个都允许（只涉及注册表与 manifest，不涉及下载）
3. 为便于测试，host 名、manifest 目录、"当前程序目录"都可以通过内部配置注入（与 autostart 的做法一致）

## 第 3 步：启动时的自动处理
1. **浏览器集成**：
   - 状态为 Broken 且当前程序不是开发构建：自动调用 enable 修正，写一行日志
   - 状态为 NotInstalled：不自动注册；在界面顶部显示一次提示条"浏览器集成未启用，Firefox 下载不会被接管"，带"启用"和"不再提示"两个按钮；"不再提示"存入设置 browser-integration-dismissed
2. **开机启动路径**：Run 中存在 idm-rs-desktop，但其中的程序路径不是当前程序，且当前程序不是开发构建：更新为当前程序路径（保留 --minimized）。不存在时不做任何事
3. **开发构建的判断**：当前程序路径中包含 `\target\debug\` 或 `\target\release\`（不区分大小写）即视为开发构建。写成纯函数并单元测试
4. 以上自动处理失败时只写日志，不阻止程序启动

## 第 4 步：界面
1. 选项对话框新增"浏览器集成"区域：显示当前状态（未启用 / 已启用 / 需要修复），按钮"启用"或"修复"、"移除"；下方灰色小字"启用后，Firefox 安装 idm-rs 扩展即可自动接管下载"
2. 顶部提示条（第 3 步第 1 条），点"启用"成功后消失并刷新状态

## 第 5 步：测试
1. is_dev_build 纯函数：开发路径、安装路径、大小写混合、路径中仅包含 target 字样但不是 target\debug 的情况
2. integration_status_variants：用测试 host 名与临时目录，分别构造 NotInstalled、Ok、Broken（manifest 指向不存在的文件）三种情况
3. enable_fixes_broken：Broken 时调用 enable 后变为 Ok，manifest 中路径为注入的程序目录下的 idm-nmhost.exe
4. enable_without_nmhost_fails：注入的程序目录下没有 idm-nmhost.exe，返回 InvalidInput，且不写注册表
5. autostart_path_refresh：用测试值名，预置指向旧路径的值；以"非开发构建"运行刷新逻辑后路径更新且保留 --minimized；以"开发构建"运行时不变；值不存在时不创建
6. 变异验证：(a) 让 Broken 判断忽略文件是否存在，测试 2、3 应失败；(b) 让开发构建判断恒为假，测试 5 的开发构建分支应失败。贴出输出后恢复
7. 新测试各 10 次；原有 phase4b、桌面全部测试各 1 次

## 第 6 步：验证与提交
1. cargo fmt、cargo clippy -D warnings、npm run build、verify-workspace.ps1、phase4b-e2e.ps1 全部 PASS
2. 运行 scripts/build-release.ps1 重新生成安装包
3. 暂存 docs apps crates scripts，提交信息 phase5c-2: first-run browser integration and autostart path，不 push、不 amend

## 报告
1. 第 1 步移动后的接口签名；CLI 行为不变的依据
2. 各测试结果与变异验证输出
3. 安装包路径与大小、提交 hash
4. 完整的安装验收清单（写成用户能照做的步骤），覆盖：安装位置、首次启动提示条、启用浏览器集成、Firefox 真实接管一次小文件下载、开机启动开关、重启电脑后从托盘启动、卸载后注册表清理与数据保留、重新安装后下载记录仍在
5. 与本文不一致之处
