# 任务：idm-rs Phase 5c-1 —— 清理旧函数、生成安装包

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5c-1.md；新建进度文件 docs/phase5c-progress.md
2. 允许联网的范围仅限：Tauri 打包工具首次下载 NSIS 等打包组件。不得 cargo update，不得新增 crate 或 npm 包。下载失败时停下报告，不得换镜像或其他来源绕过
3. 不得使用 #[allow]、unsafe、unwrap()、expect()
4. 不得结束任何不是你启动的进程；不得访问或修改用户的默认数据库和数据目录
5. **不得在本机执行安装包、不得写注册表**。安装验收由用户手动完成
6. 任何安全护栏拦截或审批超时，立即停下报告，不得绕过
7. 遇到不明确之处、或原有测试失败，停下报告，不得修改原有测试的断言
8. 每完成一步在进度文件追加一行

## 第 1 步：清理只剩测试在用的旧函数
5b-4d 中 acquire_with_shrink 被新流程取代，产品代码不再调用，只剩测试调用并加了 #[cfg(test)]。这些测试在测一段不再运行的代码。
1. 列出所有调用 acquire_with_shrink 的测试（名称、文件、行号）及各自验证的行为
2. 把每个测试改为通过产品实际使用的许可获取流程来验证同样的行为（断言语义不变）
3. 删除 acquire_with_shrink 及其 #[cfg(test)] 标记
4. 每个改造后的测试做一次变异验证（破坏它要保护的行为，应失败），贴出输出并恢复
5. 如果某个测试的行为在新流程中已不存在或无法表达，停下报告

## 第 2 步：发布构建配置
1. 根 Cargo.toml 增加 [profile.release]：lto = "thin"、codegen-units = 1、strip = true。不改 panic 策略
2. 构建 release 版的 idm-desktop、idm-cli、idm-nmhost，报告修改前后三个 exe 的大小
3. 版本号统一为 0.1.0（workspace、tauri.conf.json、package.json），如已一致则只报告

## 第 3 步：安装包
1. tauri.conf.json 打包目标只用 nsis；安装模式为当前用户（不需要管理员）；安装界面语言包含简体中文和英文
2. idm-cli.exe 与 idm-nmhost.exe 必须随安装包一起安装，并且安装后与 idm-desktop.exe 位于同一目录（浏览器接管配置需要 idm-nmhost.exe 的绝对路径）。使用 Tauri 的 resources 或 externalBin 机制，选择哪种由你判断，在报告中说明理由与安装后的实际相对路径
3. 新增脚本 scripts/build-release.ps1：按顺序构建 release 版 idm-cli 与 idm-nmhost、前端、桌面程序并生成安装包，任一步失败即以非零退出。脚本结束时打印安装包路径与大小
4. **安装目录与数据目录必须不同**：
   - 查明 Tauri NSIS 当前用户模式的默认安装目录（以实际生成的安装脚本或官方文档为依据，贴出依据）
   - 数据目录为 idm-core 的 default_db_path 所在目录
   - 如果两者相同或一个包含另一个，必须调整安装目录（例如 %LOCALAPPDATA%\Programs\idm-rs），方法可以是 Tauri 的配置项或 NSIS 安装钩子。做不到就停下报告
5. 卸载时的清理（使用 Tauri 的 NSIS 安装钩子，卸载前执行）：
   - 调用安装目录中的 idm-cli.exe nm uninstall（默认 host 名 idm_rs）
   - 删除 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 中的 idm-rs-desktop 与 idm-rs-runner 两个值（不存在时忽略）
   - **不得删除数据目录**（下载记录、设置、日志都保留）
   - 在钩子文件中写注释说明以上三点
6. 卸载前如果 idm-desktop.exe 正在运行，安装程序应提示用户关闭（确认 Tauri 默认模板是否已有此行为，报告结论）

## 第 4 步：验证（不执行安装）
1. 第 1 步改造的测试各单独连续运行 10 次
2. cargo fmt、cargo clippy -D warnings、npm run build、verify-workspace.ps1、phase4b-e2e.ps1 全部 PASS
3. 运行 scripts/build-release.ps1，成功生成安装包
4. 静态检查安装包相关产物：贴出生成的 NSIS 安装脚本（.nsi）中与安装目录、资源文件、卸载钩子相关的片段，证明第 3 步第 2、4、5 条均已生效
5. 暂存 docs apps crates scripts Cargo.toml Cargo.lock，提交信息 phase5c-1: cleanup, release profile, nsis installer，不 push、不 amend。安装包本身不提交（确认它在 .gitignore 覆盖范围内）

## 报告
1. 第 1 步测试清单、改造方式、变异验证输出
2. 三个 exe 修改前后大小；版本号情况
3. resources 与 externalBin 的选择理由及安装后路径
4. 默认安装目录的依据、数据目录位置、是否调整及方法
5. 卸载钩子内容；运行中提示的结论
6. 安装包路径与大小；验证结果；提交 hash
7. 给用户的安装验收步骤（安装、首次启动、卸载、确认数据保留与注册表清理）
8. 与本文不一致之处
