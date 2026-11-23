# Phase 5c 进度记录

- 步骤 0：git status 干净（HEAD decd4008），任务原文原样保存为 docs/prompts/phase5c-1.md，本进度文件已新建。
- 步骤 1.1（测试清单）：全库仅 1 处调用 acquire_with_shrink —— queued_chunk_released_by_takeover_wakes_immediately（crates/idm-core/src/task/download.rs:1995），验证「排队等 Host 名额的分块被接管到 acked 时 100ms 内以 Completed 退出、且不占名额」。其行为在新流程 acquire_chunk_permits 中完整存在，可改造。
- 步骤 1.2（改造）：测试改用 limiter.new_task_gate(8)（每主机上限 1 → 闸门有效许可 1，分块只在主机级排队）+ 产品实际流程 acquire_chunk_permits，断言语义不变（Ok(None)、100ms、不占名额）。已删除 acquire_with_shrink 及 #[cfg(test)]，cargo check --all-targets EXIT=0，crates 目录搜索该符号 0 命中；恢复后测试 PASS。
- 步骤 1.3（变异验证）：移除 acquire_chunk_permits 第二级（主机级）的 wake_on_shrink 唤醒分支 → 测试 FAILED「被接管到 acked 的排队分块必须丢弃等待、以 Completed 退出: Err(Elapsed(())), 0.13s」；恢复后 PASS，无残留。
- 步骤 2.1（profile）：根 Cargo.toml 已有 [profile.release] 含 lto=thin、codegen-units=1、debug=line-tables-only；仅补 strip=true，未改 panic 策略。版本号 workspace/tauri.conf.json/package.json 均 0.1.0，已一致，仅报告。
- 步骤 2.2（exe 大小）：修改前 desktop 17416192 / cli 9602048 / nmhost 2790912 字节；加 strip 后 desktop 17414656（-1536）/ cli 9602048（不变）/ nmhost 2790912（不变）。MSVC release 已剥离大部分调试符号，strip 增量极小，如实报告。
- 步骤 3.1（打包配置）：tauri.conf.json 设 active=true、targets=[nsis]、installMode=currentUser、languages=[SimpChinese,English]；externalBin=[binaries/idm-cli, binaries/idm-nmhost]（选中原因：sidecar 与主 exe 同目录，满足 nm install 绝对路径要求；resources 会落到 _up_ 子目录不满足）。首次打包下载 NSIS 3.13 + nsis-tauri-utils 0.5.3（允许的联网范围）。
- 步骤 3.2（安装目录冲突）：依据生成 .nsi 第 540 行 currentUser 默认 `$LOCALAPPDATA\${PRODUCTNAME}` = `%LOCALAPPDATA%\idm-rs`，与数据目录（default_db_path=%LOCALAPPDATA%\idm-rs\idm.db）相同。NsisConfig schema 无 installDir 项，改用自定义 template（从 CLI 内嵌模板提取），把该行改为 `$LOCALAPPDATA\Programs\${PRODUCTNAME}`；重打包后生成 .nsi 第 544 行已生效。
- 步骤 3.3（卸载钩子）：新增 nsis/hooks.nsh 定义 NSIS_HOOK_PREUNINSTALL：调用 $INSTDIR\idm-cli.exe nm uninstall（默认 idm_rs）、删 HKCU Run 的 idm-rs-desktop/idm-rs-runner、不删数据目录，三点均已注释。生成 .nsi 第 32 行 !include hooks.nsh、第 802-803 行 PREUNINSTALL 插入点生效。
- 步骤 3.4（运行中提示）：确认 Tauri 默认模板已含 CheckIfAppIsRunning（.nsi 683/802 行，基于 RestartManager，utils.nsh 弹 appRunning/appRunningOkKill 提示），安装与卸载均提示关闭应用，无需额外实现。
- 步骤 3.5（构建脚本）：新增 scripts/build-release.ps1，按序构建 release idm-cli+nmhost、前端、stage externalBin、tauri nsis 打包，每步失败非零退出，末尾打印安装包路径与大小；externalBin 的 *.exe 加入 .gitignore。
- 步骤 3.6（打包验证）：生成安装包 target/release/bundle/nsis/idm-rs_0.1.0_x64-setup.exe（8140601 字节）；.nsi 确认语言 496/498 行 SimpChinese+English、INSTALLMODE currentUser（41 行）、RequestExecutionLevel user（106 行）、externalBin File/Delete 695-696/815-816 行、安装目录 544 行、钩子 include 32 行。
- 步骤 4（验证）：改造测试单独连续 10 次 PASS=10 FAIL=0；cargo fmt（应用后 --check PASS）、cargo clippy -D warnings、npm run build、verify-workspace.ps1（全 24 步 PASS）、phase4b-e2e.ps1（ALL ASSERTIONS PASSED，残留 0）全 PASS；scripts/build-release.ps1 完整跑通，安装包 8086700 字节（7.71 MiB），末尾正确打印路径与大小。规则审查：改造测试初引入的 1 处 new_task_gate(...).unwrap() 已改为 match+panic，新增行无 unwrap/expect/allow/unsafe。安装包与 externalBin exe 经 git check-ignore 确认均在忽略范围。
