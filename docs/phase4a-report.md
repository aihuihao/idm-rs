# Phase 4a 提交前自查报告

任务原文：`docs/prompts/phase4a.md`。基线 HEAD：`dd5bf76`。
所有构建/测试均使用 `--offline`，未执行 `cargo update`。测试只用本地回环服务器与临时数据库，
未访问外网、未碰真实注册表。

## 一、提交前自查表（第六部分编号）

每个新测试用 `scripts/phase4a-acceptance.ps1 -Section all -Repeat 10` 单独连续运行 10 次。
判定标准：单次 `cargo test --exact` 退出码 0 **且**输出含 `1 passed`；任一不满足即计失败。
结果：`SUMMARY: all 22 tests reached 10/10 : PASS`（日志 `target/accept3.out`，stderr 为空）。

| 第六部分编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|
| 1 | 帧读写：正常 / 长度 0 / 超长不分配缓冲 / 半截 EOF / 连续多条 / 写后读 | `frame::tests::frame_roundtrip_normal`, `frame_zero_length`, `frame_over_limit`, `frame_truncated_then_eof`, `frame_multiple`, `frame_write_then_read` | 6 项各 10/10 PASS |
| 2 | ping 往返（真实帧协议子进程） | `host_ping_roundtrip` | 10/10 PASS |
| 3 | enqueue 创建 Queued 任务（URL/目录/请求头正确） | `host_enqueue_creates_queued_task` | 10/10 PASS |
| 4 | 拒绝非法输入（scheme/host/超长 URL/超大消息/非法 JSON/未知 type/含换行头） | `host_rejects_bad_input`, `host_rejects_oversized_message` | 2 项各 10/10 PASS |
| 4（补充） | `config set/get download-dir` | `config_set_get_download_dir` | 10/10 PASS |
| 5 | stdout 只写帧、panic 写入 nmhost.log | `host_stdout_contains_only_frames` | 10/10 PASS |
| 6 | 文件名 sanitize（`..\..\evil.exe` 等 4 例） | `filename_is_sanitized` | 10/10 PASS |
| 7 | 请求头到达服务器（含重试、续传） | `headers_reach_server_including_retry`, `headers_reach_server_on_resume` | 2 项各 10/10 PASS |
| 8 | 任务完成后 task_headers 清空 | `headers_cleared_on_completion` | 10/10 PASS |
| 9 | Cookie 值不进日志 / 错误信息 | `cookie_not_in_logs` | 10/10 PASS |
| 10 | core 拒绝非法请求头（白名单/换行/超长） | `invalid_headers_rejected_in_core` | 10/10 PASS |
| 11 | `--watch` 空闲后接纳新增任务并在时限内下载完成、逐字节正确 | `watch_runner_picks_up_new_tasks` | 10/10 PASS |
| 12 | 单实例：第二个退出码 3，第一个退出后第三个可启动 | `watch_runner_single_instance` | 10/10 PASS |
| 13 | 连续入队两个任务只拉起一个运行器 | `host_spawns_runner_once` | 10/10 PASS |
| 14 | 迁移 V4 升级与重复执行 | `migration_v4_creates_tables_and_is_idempotent` | 10/10 PASS |
| 15 | 回归：全部原有测试 + 两个崩溃测试 | `cargo test --workspace --offline`（全量）；`cargo test -p idm-cli --test crash_resume -- --ignored`（2 个崩溃测试） | 全量 241 passed / 0 failed；崩溃 2 passed / 0 failed |

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
- `crates/idm-ipc/Cargo.toml:11-15,17-19`：`workspace = true`、`idm-core`、tokio features、`[[bin]] idm-nmhost`

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
- `crates/idm-ipc/src/lib.rs` 导出 `pub mod frame`
- `crates/idm-ipc/src/frame.rs:8` `MAX_MESSAGE_LEN=64 KiB`；`:12` `ReadFrame`；`:26` `read_frame`（先校验长度再分配）；`:52` `write_frame`
- `crates/idm-ipc/src/bin/idm-nmhost.rs:53` `host_log`（1 MiB 覆写、绝不到 stdout）；`:76` panic hook
- `crates/idm-ipc/src/bin/idm-nmhost.rs:109` `run_loop`（帧循环 / stdout 仅帧 / 超长关闭）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:152` `handle_message`（ping / enqueue / 未知 type / 非法 JSON）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:163` `handle_enqueue`（头白名单校验 + 丢弃非法头并记日志 + 入队）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:231` `is_http_url`（scheme/host/8 KiB）
- `crates/idm-ipc/src/bin/idm-nmhost.rs:279` `spawn_runner_if_needed`（runner.lock 单实例、DETACHED_PROCESS|CREATE_NO_WINDOW、`runner:"not_started"`）

### 文档 / 脚本 / 测试
- `ARCHITECTURE.md:144-149` 第 12 节（按任务原文）
- `scripts/phase4a-acceptance.ps1` 逐测试 10× 验收
- `crates/idm-core/tests/support/mod.rs:124,326` `header_sink` 记录所有请求头
- `crates/idm-ipc/src/frame.rs`（单元测试）、`crates/idm-ipc/tests/nmhost.rs`、
  `crates/idm-core/tests/phase4a.rs`、`crates/idm-cli/tests/phase4a.rs`

## 四、跨域重定向时 Cookie 的实际行为

按任务第三部分第 5 条，未做额外处理，实际观察：`build_client` 把 `Cookie` 注入 reqwest 的
**默认请求头**（`HeaderMap`）。reqwest/hyper 在跨域重定向时会自动剥离 `Cookie`（以及
`Authorization` 等敏感头）以防泄露给第三方域；同域重定向则保留。本阶段测试服务器的
`/start → /target` 重定向为同域，因此 Cookie 保留、三个头均到达；测试未构造跨域场景。
结论：跨域 Cookie 由 reqwest 自动剥离，无需在应用侧补处理。

## 五、测试总数

- 本阶段新增测试：**22 个**（frame 单元 6 + idm-ipc 集成 7 + idm-core 集成 6 + idm-cli 集成 3）
- 每个新测试单独连续运行 10 次：**220 次运行，全部 PASS**
- 全量回归 `cargo test --workspace --offline`：**241 passed / 0 failed**（含上述 22 个）
- 两个崩溃测试（`crash_resume.rs -- --ignored`）：**2 passed / 0 failed**
- 验收脚本 `scripts/phase4a-acceptance.ps1`：exit code 0，`SUMMARY: ... PASS`

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
