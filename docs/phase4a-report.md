# Phase 4a 提交前自查报告

任务原文：`docs/prompts/phase4a.md`。基线 HEAD：`dd5bf76`。
所有构建/测试均使用 `--offline`，未执行 `cargo update`。测试只用本地回环服务器与临时数据库，
未访问外网、未碰真实注册表。

本报告经两次修正提交更新：
- `phase4a-fix: clippy and host input validation`：修正 `verify-workspace.ps1` STEP 6 的
  clippy 错误、host 的 URL/文件名校验与 `write_frame` 行为，补做工作区验收脚本全量运行。
- `phase4a-fix: gate panic test hook behind cargo feature`：把 host 的 panic 测试开关
  `IDM_NMHOST_TEST_PANIC` 从无条件编译改为仅 `test-hooks` feature 启用时存在（默认/发布构建
  完全不含该开关），并补测默认构建下环境变量无效。文中的所有结果均为修正后重新实测的数据。

## 一、提交前自查表（第六部分编号）

每个新测试用 `scripts/phase4a-acceptance.ps1 -Section all -Repeat 10` 单独连续运行 10 次。
判定标准：单次 `cargo test --exact` 退出码 0 **且**输出含 `1 passed`；任一不满足即计失败。
第二次修正后重跑结果：`SUMMARY: all 27 tests reached 10/10 : PASS`，`ACCEPT_EXIT=0`
（日志 `target/accept-hooks.out`）。工作区验收脚本 `scripts/verify-workspace.ps1` 单独运行结果：
`SUMMARY: all 16 steps executed; final result: PASS`，`SCRIPT_EXIT=0`（日志 `target/vw3.out`）。
其中 `host_stdout_contains_only_frames` 与 `host_stdout_single_ping_frame_then_panic` 由验收脚本
以 `cargo test --features test-hooks` 运行；`default_build_ignores_panic_env` 以默认 features 运行。

| 第六部分编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|
| 1 | 帧读写：正常 / 长度 0 / 超长不分配缓冲 / 半截 EOF / 连续多条 / 写后读 / 序列化失败不发空帧 | `frame::tests::frame_roundtrip_normal`, `frame_zero_length`, `frame_over_limit`, `frame_truncated_then_eof`, `frame_multiple`, `frame_write_then_read`, `write_frame_failure_emits_no_bytes` | 7 项各 10/10 PASS |
| 2 | ping 往返（真实帧协议子进程） | `host_ping_roundtrip` | 10/10 PASS |
| 3 | enqueue 创建 Queued 任务（URL/目录/请求头正确） | `host_enqueue_creates_queued_task` | 10/10 PASS |
| 4 | 拒绝非法输入（scheme/host/超长 URL/超大消息/非法 JSON/未知 type/含换行头） | `host_rejects_bad_input`, `host_rejects_oversized_message` | 2 项各 10/10 PASS |
| 4（补充） | URL 严格校验：空白/`http://:80/`/畸形 IPv6/控制字符拒绝；内网主机名与 userinfo 允许且不入日志 | `host_url_validation_cases` | 10/10 PASS |
| 4（补充） | `validate_url` 对三个边界输入的实际返回值 | `url_tests::validate_url_returns_for_three_inputs` | 10/10 PASS |
| 4（补充） | `config set/get download-dir` | `config_set_get_download_dir` | 10/10 PASS |
| 5 | stdout 只写帧、panic 写入 nmhost.log | `host_stdout_contains_only_frames` | 10/10 PASS |
| 5（补充） | 先正常回复 ping 再 panic，stdout 恰好一个完整 ping 回复帧、无其它字节 | `host_stdout_single_ping_frame_then_panic` | 10/10 PASS |
| 5（补充） | 默认/发布构建下 `IDM_NMHOST_TEST_PANIC` 环境变量完全无效 | `default_build_ignores_panic_env` | 10/10 PASS |
| 6 | 文件名 sanitize（对 5 个具体结果值断言） | `filename_is_sanitized` | 10/10 PASS |
| 7 | 请求头到达服务器（含重试、续传） | `headers_reach_server_including_retry`, `headers_reach_server_on_resume` | 2 项各 10/10 PASS |
| 8 | 任务完成后 task_headers 清空 | `headers_cleared_on_completion` | 10/10 PASS |
| 9 | Cookie 值不进日志 / 错误信息 | `cookie_not_in_logs` | 10/10 PASS |
| 10 | core 拒绝非法请求头（白名单/换行/超长） | `invalid_headers_rejected_in_core` | 10/10 PASS |
| 11 | `--watch` 空闲后接纳新增任务并在时限内下载完成、逐字节正确 | `watch_runner_picks_up_new_tasks` | 10/10 PASS |
| 12 | 单实例：第二个退出码 3，第一个退出后第三个可启动 | `watch_runner_single_instance` | 10/10 PASS |
| 13 | 连续入队两个任务只拉起一个运行器 | `host_spawns_runner_once` | 10/10 PASS |
| 14 | 迁移 V4 升级与重复执行 | `migration_v4_creates_tables_and_is_idempotent` | 10/10 PASS |
| 15 | 回归：全部原有测试 + 两个崩溃测试 | `cargo test --workspace --offline`（全量）；`cargo test -p idm-cli --test crash_resume -- --ignored`（2 个崩溃测试）；`scripts/verify-workspace.ps1`（16 步） | 全量 244 passed / 0 failed（`REG_EXIT=0`）；崩溃 2 passed / 0 failed（`CRASH_EXIT=0`）；验收脚本 16/16 PASS（`SCRIPT_EXIT=0`） |

## 二、Cargo.lock 相对基线新增的包

`git diff dd5bf76 -- Cargo.lock` 中 `+name =` 只有两处：

- `serde_json 1.0.154`
- `zmij 1.0.23`（serde_json 的传递依赖）

其余差异均为已有包（`idm-ipc`、`idm-core` 等）的依赖清单引用变化，没有新增其它包、没有版本变动。
`tracing-subscriber` 作为 `idm-core` 的 dev-dependency 加入，但该包本就已在锁文件中（由
`idm-cli` 依赖），因此不新增包。

## 三、每项实现的代码位置（文件:行号）

### 依赖
- `Cargo.toml:22-23`：`serde` / `serde_json` 提升到 workspace.dependencies
- `crates/idm-ipc/Cargo.toml:11-15,23-28`：`workspace = true`、`idm-core`、tokio features、
  `[features] default=[] / test-hooks=[]`、`[[bin]] idm-nmhost`

### 迁移 V4 与 Store（第二部分）
- `crates/idm-core/src/store/mod.rs:27` `MIGRATIONS` 追加 V4；`:30` SCHEMA_VERSION=4
- `crates/idm-core/src/store/mod.rs:88` `MIGRATION_V4`：`task_headers`（外键 `ON DELETE CASCADE`）与 `settings`
- `crates/idm-core/src/store/mod.rs:601` `db_path()`
- `crates/idm-core/src/store/mod.rs:640` `enqueue_task(t, Option<headers>)`；`:1170` 同一事务写入任务与请求头
- `crates/idm-core/src/store/mod.rs:650/657/663/669/675` `set/get/clear_task_headers`、`get/set_setting`
- `crates/idm-core/src/task/download.rs:944` 任务完成时 `clear_task_headers`

### 请求头透传（第三部分）
- `crates/idm-core/src/net/client.rs:86` `DownloadConfig.extra_headers`
- `crates/idm-core/src/net/client.rs:89` 手写 `Debug`（Cookie 值遮蔽为 `<cookie>`）
- `crates/idm-core/src/net/client.rs:149` `validate_extra_headers`（白名单、CR/LF、8 KiB、错误信息不含值）
- `crates/idm-core/src/net/client.rs:224` `build_client`：`:235` 默认头注入 Referer/Cookie，`:253` User-Agent 覆盖
- `crates/idm-core/src/queue/mod.rs:496` `build_options`；`:509` 从 DB 读 `get_task_headers`
- `crates/idm-cli/src/main.rs:765` `run_resume` 读取任务请求头注入配置

### 常驻运行器（第四部分）
- `crates/idm-cli/src/main.rs:1189` `run_queue_run`
- `crates/idm-cli/src/main.rs:1254` `--watch` 解析
- `crates/idm-cli/src/main.rs:1278-1294` `runner.lock` 独占（`share_mode(0)`）/ "already running" / 退出码 3
- `crates/idm-cli/src/main.rs:1323-1327` Ctrl+C 安全停止；`:1360-1374` watch 每秒 `adopt_queued`
- `crates/idm-cli/src/main.rs:1395` `run_config`；`:1408` 仅支持 `download-dir`（目录创建）

### Native Messaging host（第五部分）
- `crates/idm-ipc/src/lib.rs` 导出 `pub mod frame`，文档注释改为 Phase 4a 实际说明
- `crates/idm-ipc/src/frame.rs:8` `MAX_MESSAGE_LEN=64 KiB`；`:12` `ReadFrame`；`:26` `read_frame`（先校验长度再分配）；`:54` `write_frame`（泛型 `Serialize`，序列化失败返回 `Err` 且不写任何字节，绝不发空帧）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:54` `host_log`（1 MiB 覆写、绝不到 stdout）；`:77` panic hook
- `crates/idm-ipc/src/bin/idm-nmhost.rs:88-99` `maybe_test_panic` 的两个 cfg 版本：
  `#[cfg(feature = "test-hooks")]` 读 `IDM_NMHOST_TEST_PANIC` 并注入 panic；
  `#[cfg(not(feature = "test-hooks"))]` 为空操作，默认/发布构建不含开关字符串
- `crates/idm-ipc/src/bin/idm-nmhost.rs:116` `run_loop`（帧循环 / stdout 仅帧 / 超长关闭；panic 开关按第 N 条消息触发）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:159` `handle_message`（ping / enqueue / 未知 type / 非法 JSON）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:170` `handle_enqueue`（头白名单校验 + 丢弃非法头并记日志 + 入队）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:182` 文件名先取最后一个路径分隔符后的部分再 `sanitize_windows_filename`
- `crates/idm-ipc/src/bin/idm-nmhost.rs:245` `validate_url`（`idm_core::net::Url::parse`；长度 ≤ 8 KiB、无空白/控制字符、scheme 仅 http/https、`host_str()` 非空；错误不回显 URL，userinfo 不入日志）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:289` `spawn_runner_if_needed`（runner.lock 单实例、DETACHED_PROCESS|CREATE_NO_WINDOW、`runner:"not_started"`）

### clippy 修正（修正提交）
- `crates/idm-core/tests/support/mod.rs:73` `pub type HeaderSink = ...`（消除 `type_complexity`）；`:127` 字段改用别名
- `crates/idm-core/tests/phase4a.rs`：`&[...]` 替代 `&vec![...]`、`?` 替代 `unwrap`、去掉 `assert!` 参数多余引用、`install_log_capture` 返回 `TestResult`
- `crates/idm-cli/src/main.rs:1413-1417` 折叠嵌套 `if`（`&& let Err(e) = ...`）
- `crates/idm-ipc/tests/nmhost.rs`：所有辅助函数返回 `TestResult` 并用 `?`，无 `unwrap()/expect()`，全程不用 `#[allow]`

### 文档 / 脚本 / 测试
- `ARCHITECTURE.md:144-149` 第 12 节（按任务原文）
- `scripts/phase4a-acceptance.ps1` 逐测试 10× 验收（本阶段额外新增，见第六节第 8 条）
- `crates/idm-core/tests/support/mod.rs:73,127,329` `header_sink` 记录所有请求头
- `crates/idm-ipc/src/frame.rs`（单元测试含 `write_frame_failure_emits_no_bytes`）、`crates/idm-ipc/tests/nmhost.rs`、
  `crates/idm-core/tests/phase4a.rs`、`crates/idm-cli/tests/phase4a.rs`

## 四、跨域重定向时 Cookie 的实际行为

按任务第三部分第 5 条，未做额外处理，实际观察：`build_client` 把 `Cookie` 注入 reqwest 的
**默认请求头**（`HeaderMap`）。reqwest/hyper 在跨域重定向时会自动剥离 `Cookie`（以及
`Authorization` 等敏感头）以防泄露给第三方域；同域重定向则保留。本阶段测试服务器的
`/start → /target` 重定向为同域，因此 Cookie 保留、三个头均到达；测试未构造跨域场景。
结论：跨域 Cookie 由 reqwest 自动剥离，无需在应用侧补处理。

## 五、测试总数

- 本阶段新增测试：**27 个**（frame 单元 7 + idm-nmhost bin 单元 1 + idm-ipc 集成 10 + idm-core 集成 6 + idm-cli 集成 3）
- 每个新测试单独连续运行 10 次：**270 次运行，全部 PASS**
- 全量回归 `cargo test --workspace --offline`：**244 passed / 0 failed**（默认 features，`REG_EXIT=0`）
- 两个崩溃测试（`crash_resume.rs -- --ignored`）：**2 passed / 0 failed**，`CRASH_EXIT=0`
- **`scripts/phase4a-acceptance.ps1`（本阶段额外新增的 10× 脚本）**：`ACCEPT_EXIT=0`，
  `SUMMARY: all 27 tests reached 10/10 : PASS`
- **`scripts/verify-workspace.ps1`（仓库原有的工作区验收脚本，任务第九部分所指）**：
  `SCRIPT_EXIT=0`，`SUMMARY: all 16 steps executed; final result: PASS`（16 步全 PASS，
  含 STEP 6 `cargo clippy --workspace --all-targets --locked -- -D warnings` 与
  STEP 16 两个 `--ignored` 崩溃测试）。修正前该脚本在 STEP 6 失败（`exit=101`，脚本退出码 1），
  修正后已全绿。

**更正说明**：此前报告第五节写的「验收脚本 exit code 0」指的是本阶段自建的
`scripts/phase4a-acceptance.ps1`，并**不是** `scripts/verify-workspace.ps1`；当时后者是失败的。
现按脚本全名分别列出，不再混用「验收脚本」一词。

## 六、与任务原文不一致之处（如实说明）

1. **`cargo fmt --all` 后的格式调整**：为通过格式检查，对新增代码执行了 `cargo fmt`，行号以格式化后为准。
2. **第 4 项「含换行的请求头」的处理方式**：任务第五部分第 4 条明确「不合法的请求头直接丢弃该头
   （不影响入队），并在日志中记录」，而第六部分第 4 项把含换行头与「返回错误且数据库里没有新任务」
   并列。二者存在张力。实现按第五部分：**整条 enqueue 仍成功**，`\r\n` 的 Cookie 被丢弃且不入库、记日志，
   host 不崩溃且能继续处理下一条。测试 `host_rejects_bad_input` 按此断言（任务仍入队、坏头不入库）。
3. **第 15 项回归不以「单个测试 10 次」形式执行**：全量回归与两个崩溃测试属于既有套件，按任务
   「回归：全部原有测试、两个崩溃测试」各执行 1 遍（全绿）；10× 要求针对本阶段新增测试。
4. **第 13 项用伪 `idm-cli` 而非统计真实进程**：测试用 `rustc` 现场编译一个持有 `runner.lock` 的伪
   `idm-cli.exe`，通过「启动日志行数 == 1」确定性地验证只拉起一次，比按进程名全局计数更可靠。
5. **host 集成测试把二进制复制到干净目录运行**：`host_enqueue_*` 等测试若直接从 `target/debug`
   启动，同目录存在真实 `idm-cli.exe`，入队会拉起常驻 runner 并残留进程/继承 stdout 句柄。
   因此测试复制 host 到临时干净目录，走 `runner:"not_started"` 分支；「拉起运行器」由第 13 项单独验证。
6. **`tracing-subscriber` 作为 `idm-core` 的 dev-dependency**：`cookie_not_in_logs` 需要捕获 tracing
   事件，故在 `crates/idm-core/Cargo.toml` 加了 dev 依赖（该包本已在锁文件中，不新增包）。
7. **第 1 项帧编解码为库内单元测试**：放在 `crates/idm-ipc/src/frame.rs` 的 `#[cfg(test)]` 模块，
   而非 `tests/` 目录，以便复用共享 codec 且不导出内部类型。
8. **`scripts/phase4a-acceptance.ps1` 是额外新增的脚本**：任务原文第九部分只要求「每个新测试单独
   连续运行 10 次；验收脚本全部 PASS，exit code 0」，并未要求新建脚本。这里的「验收脚本」指的是
   仓库原有的 `scripts/verify-workspace.ps1`。为把「每个新测试各跑 10 次」自动化，额外新增了
   `scripts/phase4a-acceptance.ps1`，它是本阶段多出来的文件，不属于原文交付清单。
   **更正此前第五节的表述**：修正前报告里写的「验收脚本 exit code 0」指的是我自建的
   `scripts/phase4a-acceptance.ps1`，**不是** `scripts/verify-workspace.ps1`；当时后者在 STEP 6
   clippy 失败（`FAIL (exit=101)`，脚本退出码 1）。修正后两个脚本的结果分别如实记录于第五节，
   不再混用「验收脚本」一词。
9. **`test-hooks` feature 与 `IDM_NMHOST_TEST_PANIC` 开关隔离是任务原文未要求的额外改动**：
   原文第五部分第 2 条只要求「panic hook 把 panic 写到日志」，第六部分第 5 项要求用「仅在测试
   构建里存在」的开关验证 stdout 纯净。初版实现用运行时环境变量无条件编译该开关，导致**发布
   二进制也包含它**（实测 `cargo build --release` 产物中 `CONTAINS_ENV_NAME=True`）。改为
   `crates/idm-ipc/Cargo.toml` 定义默认关闭的 `test-hooks` feature 后，只有以
   `--features test-hooks` 编译的测试构建才含该开关；默认与 release 构建实测
   `CONTAINS_ENV_NAME=False`、`CONTAINS_PANIC_MSG=False`。代价是两个 panic 测试必须带 feature
   运行，工作区默认 `cargo test --workspace` 不执行它们（由 `default_build_ignores_panic_env`
   覆盖默认构建行为），因此 `verify-workspace.ps1` 的 STEP 5 不再覆盖这两个用例。

## 七、两个 host 同时入队时的运行器单实例行为

`idm-nmhost::spawn_runner_if_needed` 的锁检查是「独占打开 `runner.lock` → 能打开则**立即关闭**
→ 启动同目录 `idm-cli.exe queue run --watch`」。因此它只是避免同一 host 进程重复拉起，
**不是**跨进程的原子互斥：当两个 host 进程几乎同时入队时，可能都能短暂打开锁并各自启动一个
`idm-cli`。此时第二个 CLI 在 `run_queue_run` 启动时用 `share_mode(0)` 独占打开同一个
`runner.lock` 会失败，打印 `already running` 并以**退出码 3** 退出；第一个 CLI 持有锁直到进程
退出并承担常驻调度。**结果上仍然只有一个运行器进程存在**。

测试 `host_spawns_runner_once` 验证的是单个 host 连续两次入队只会拉起一次（伪 CLI 的启动日志
恰好 1 行）；跨进程并发的兜底由 CLI 侧的 `runner.lock` 与退出码 3 保证，由
`watch_runner_single_instance` 覆盖。这是「尽力避免重复拉起 + CLI 侧强单实例」的两层设计，
而非依赖 host 侧的原子锁。
