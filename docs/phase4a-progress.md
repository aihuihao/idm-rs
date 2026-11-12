# Phase 4a 进度（实时记录，上下文将满时据此续做）

基线 HEAD：`dd5bf76 docs: save phase4b prompt`（之前 `9f30e8c phase3`）。
预期未提交改动：`Cargo.lock`、`crates/idm-ipc/Cargo.toml`（serde/serde_json，已就绪）。
本阶段不写浏览器扩展（4b）。测试只用本地回环服务器 + 测试数据库，不碰真实注册表/外网。
所有构建 `--offline`。Cargo.lock 相对基线仅新增 `serde_json 1.0.154` 与 `zmij 1.0.23`。

## 任务分解与状态
- [ ] 一、依赖：迁移 serde/serde_json 到 workspace.dependencies，idm-ipc 用 workspace=true
- [ ] 二、DB 迁移 V4：task_headers + settings 表；Store 方法；enqueue_task 带 headers
- [ ] 三、请求头透传：DownloadConfig.extra_headers + 校验；probe/chunk/retry/resume 带头；build_options/resume 读库；日志不含 Cookie 值
- [ ] 四、常驻运行器：`queue run --watch`；runner.lock 单实例(退出码3)；`config set/get download-dir`
- [ ] 五、idm-nmhost 二进制：帧协议；stdout 仅帧；panic hook + nmhost.log；ping/enqueue；校验；拉起 runner
- [ ] 六、测试（crates/idm-ipc/tests/、crates/idm-core/tests/）：1..15 + 回归
- [ ] 七、ARCHITECTURE.md 第 12 节
- [ ] 八、docs/phase4a-report.md 自查表（每项测试名 + 10/10）
- [ ] 九、提交（docs crates scripts ARCHITECTURE.md Cargo.toml Cargo.lock），不 push

## 已完成
- 保存 `docs/prompts/phase4a.md`
- Part 2：store/mod.rs V4 迁移（task_headers/settings）+ 6 个新方法 + enqueue_task 带 headers（同一事务）。cargo check 通过
- Part 3：DownloadConfig.extra_headers + 手动 Debug（遮蔽 Cookie）+ validate_extra_headers + build_client 注入默认头（User-Agent 覆盖）；build_options/resume 从 DB 读头；download.rs 完成时 clear_task_headers。cargo check 通过
- 已读取 store/mod.rs、client.rs、queue/mod.rs、download.rs、main.rs 相关段

## 已完成
- 保存 `docs/prompts/phase4a.md`
- Part 2：store/mod.rs V4 迁移（task_headers/settings）+ 6 个新方法 + enqueue_task 带 headers
- Part 3：DownloadConfig.extra_headers + 手动 Debug（遮蔽 Cookie）+ validate_extra_headers + build_client 注入；build_options/resume 读库；download.rs 完成清头
- Part 1：serde/serde_json 迁移到 workspace.dependencies（idm-ipc 用 workspace=true）
- Part 4：queue run --watch + runner.lock 单实例(退出码3) + config set/get download-dir
- Part 5：idm-nmhost 二进制（帧协议/stdout纪律/panic hook/ping/enqueue/校验/拉起 runner）+ QueuedTask.file_name
- cargo check --workspace --offline --all-targets 全绿

## 已完成（测试部分）
- frame.rs 单元测试（item 1）：6 个，全绿
- idm-core/tests/phase4a.rs（7,8,9,10,14）：6 个，全绿
- idm-ipc/tests/nmhost.rs（2,3,4,4b,5,6,13）：7 个，全绿
- idm-cli/tests/phase4a.rs（11,12 + config）：3 个，全绿
- 修复：nmhost 改用共享 frame 模块；frame 截断返回 Eof；ReadFrame 派生 Debug；
  phase4a core 测试 helper 改 Arc<Store>；headers_cleared 用 run_queue 避免双任务抢同一文件；
  tracing-subscriber dev 依赖 + 闭包式 MakeWriter；CLI watch 测试用异步等待避免饿死测试服务器

## 已完成（收尾）
- 七、ARCHITECTURE.md 第 12 节（按原文）
- 八、docs/phase4a-report.md 自查表
- 九、初版提交 `20c3429`（22 项 10/10）

## 已完成（修正提交 phase4a-fix）
- clippy STEP 6 全绿：`verify-workspace.ps1` 16 步全 PASS，exit 0
  （core support HeaderSink 别名、phase4a 去 unwrap/vec/多余引用、main.rs 折叠 if、
   nmhost.rs 测试全改 `?`；未使用任何 `#[allow]`）
- host 安全修正：删手写 is_http_url 改 `Url::parse`（8 KiB / 无空白控制字符 / scheme / host_str）；
  文件名先取 basename 再 sanitize；`write_frame` 序列化失败返回 Err 且不发空帧；
  lib.rs 注释更新；panic 开关支持第 N 条消息
- 新增 3 个测试：`write_frame_failure_emits_no_bytes`、`host_url_validation_cases`、
  `host_stdout_single_ping_frame_then_panic`；`filename_is_sanitized` 改对具体值断言
- 25 个新测试各 10 次全 PASS（ACCEPT_EXIT=0）；全量回归 244 passed/0 failed（REG_EXIT=0）；
  崩溃测试 2 passed/0 failed（CRASH_EXIT=0）；verify-workspace.ps1 SCRIPT_EXIT=0
- 报告更正「验收脚本 exit code 0」的指向，补第 8 条不一致与并发单实例说明

## 已完成（第二次修正：test-hooks feature 隔离）
- 实测改造前 release 二进制含 `IDM_NMHOST_TEST_PANIC` 与 `injected panic for self-test`（True/True）
- `crates/idm-ipc/Cargo.toml` 加 `[features] default=[] / test-hooks=[]`；
  `idm-nmhost.rs` 的 `maybe_test_panic` 拆成两个 cfg 版本，main/run_loop 不再传开关参数
- 实测改造后 release 二进制 `CONTAINS_ENV_NAME=False`、`CONTAINS_PANIC_MSG=False`
- 两个 panic 测试改为 `#[cfg(feature = "test-hooks")]`；新增 `default_build_ignores_panic_env`
  （`#[cfg(not(...))]`）确认默认构建下环境变量无效
- 新增 `url_tests::validate_url_returns_for_three_inputs` 打印三个边界输入实际返回值；
  `host_url_validation_cases` 打印 nmhost.log 原文并断言不含 user/pass
- 验收脚本加 `Feature` 字段（panic 测试用 `--features test-hooks`）与 2 个新测试项，共 27 项
- 27 项各 10 次全 PASS（ACCEPT_EXIT=0）；verify-workspace.ps1 16/16 PASS（SCRIPT_EXIT=0）；
  两种 feature 下 clippy -D warnings 均 exit 0

## 待做
- [ ] 追加提交 `phase4a-fix: gate panic test hook behind cargo feature`
  （docs crates scripts ARCHITECTURE.md Cargo.toml Cargo.lock），不 push

## 关键代码位置（边做边补）
- 迁移数组：`crates/idm-core/src/store/mod.rs` L27 (`MIGRATIONS`), L30 (`SCHEMA_VERSION`)
- enqueue_task：`store/mod.rs` L585 (pub) / L1057 (sql)
- 完成清理钩子位置：待在 `crates/idm-core/src/queue/mod.rs` 完成处接 clear_task_headers
- 删除级联：task_headers REFERENCES tasks(id) ON DELETE CASCADE（已设计）

## 下一步
1. 实现 Part 2（迁移 V4 + Store 方法 + enqueue_task headers）
2. 读 net/headers.rs / net/filename.rs / net/client.rs / net/probe.rs / task/download.rs / task/resume.rs / queue/mod.rs / main.rs 相关段
3. 实现 Part 3 请求头透传
4. 实现 Part 4 运行器
5. 实现 Part 5 nmhost

## 4b 前置改动（已完成并提交 `4860907`）
- idm-nmhost 入队前拒绝含 userinfo 的 URL：`"url must not contain credentials"`，不入队
- 新增 `host_enqueue_rejects_url_credentials`、`url_tests::validate_url_rejects_all_userinfo_forms`
- `verify-workspace.ps1` 16 步全 PASS / exit 0 后开始 4b

## Phase 4b 已完成（待提交）
- `idm-cli nm install / uninstall / status`（`mod nm`，纯 reg.exe，只写 HKCU）
- `extensions/firefox/`：`manifest.json`（MV2、idm-rs@local、恰好 5 权限）、
  `background.js`（onCreated / ping / Cookie / enqueue / cancel+erase / 去重 / 徽标 / 超时）、`README.md`
- `crates/idm-cli/tests/phase4b.rs`：7 个测试，`RegGuard` 失败也清理注册表
- `scripts/phase4b-acceptance.ps1`：5 个纯 PowerShell 静态检查 + 7 个 Rust 测试，各 10 次
- `scripts/verify-workspace.ps1` STEP 10 保留原检查并新增 extensions/ 下禁止 package.json
- 12 项各 10/10 全 PASS（ACCEPT_EXIT=0）；verify-workspace.ps1 16/16 PASS（SCRIPT_EXIT=0）；
  全量回归 253 passed（REG_EXIT=0）；两个崩溃测试 2 passed（CRASH_EXIT=0）
- 报告 `docs/phase4b-report.md`（MDN 核对、清单/注册表实测内容、实现位置、9 条不一致）
6. 测试 + 报告 + 提交
