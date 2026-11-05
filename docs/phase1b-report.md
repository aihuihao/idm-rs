# Phase 1b 交付报告（idm-rs）

> 阶段范围：Phase 1b = 前置修正（A）+ Probe 与静态分块下载（B）。
> 验收脚本：`scripts/verify-workspace.ps1`（15 步，全部 PASS）；`cargo test --release -- --ignored`（1GB 压力测试，PASS）。
> 提交均**未 push**。

---

## 1. 相关提交（hash）

| hash | 说明 |
|------|------|
| `dea606a` | `phase1b-pre: pool id allocation, async sync, disk-full mapping, sparse detection`（A 部分） |
| `3b86889` | `phase1b: cancel signal, unbounded range, full test coverage`（B 部分主体） |
| `4de2f06` | `phase1b.1: timeouts, cooperative cancel, safe rename`（B 部分收尾：三层超时 / 协作式取消 / 安全重命名） |

> 说明：原文 B7 预期“两次提交”（`phase1b-pre` + `phase1b`）。实际因把“三层超时 / 协作式取消 / 安全重命名”单独拆成 `phase1b.1` 提交，故为 3 个提交。功能完整、测试全绿，拆分不影响交付。

---

## 2. 新增公开 API（签名 + 一句话说明）

### `net/client.rs`
- `pub struct DownloadConfig { pub connections: usize, pub min_chunk_size: u64, pub connect_timeout: Duration, pub read_timeout: Duration, pub user_agent: String, pub max_redirects: usize }` — 下载配置（连接数、最小分块、三层超时、UA、重定向上限）。
- `DownloadConfig::new() -> Self` — 默认配置（连接数 8、最小分块 1MiB、连接/读取超时各 30s）。
- `DownloadConfig::validate(&self) -> Result<()>` — 校验连接数 ∈ 1..=32。
- `DownloadConfig::check_scheme(url: &Url) -> Result<()>` — 仅允许 `http`/`https`，其余 scheme 拒绝。
- `DownloadConfig::request_timeout(&self) -> Duration` — 仅供 Probe 使用的总时长上限（连接 + 读取）。
- `DownloadConfig::build_client(&self) -> Result<reqwest::Client>` — 构建客户端：强制 `Accept-Encoding: identity`、限制重定向、施加三层超时、TLS 校验始终开启。

### `net/probe.rs`
- `pub struct ProbeResult { final_url, total_size: Option<u64>, supports_range: bool, etag: Option<(String, bool)>, last_modified: Option<String>, filename: String, content_type: Option<String> }` — 探测结果（弱 ETag 用 `bool` 标注）。
- `pub struct Probe<'a>` / `Probe::new(client, config) -> Self` / `Probe::probe(&self, url: &Url) -> Result<ProbeResult>` — 发 `Range: bytes=0-0` 取响应头、丢弃正文，判 Range 支持与总长。

### `net/filename.rs`
- `pub fn resolve_conflict(dir: &Path, desired: &str) -> PathBuf` — 目标目录同名时按 `name (1).ext` 顺序避让。
- `pub fn sanitize_windows_filename(name: &str) -> String` — Windows 文件名合法化（保留名、非法字符、末尾点/空格、200 字符上限）。
- `pub fn infer_filename(url: &Url, cd: Option<&str>) -> String` — 文件名推断（Content-Disposition > URL 路径 > `download`）。

### `net/headers.rs`
- `pub fn parse_content_range(value: &str) -> Result<(u64, u64, Option<u64>)>` — 解析 `Content-Range`（纯函数）。

### `task/download.rs`
- `pub async fn download(url: &str, pool: Arc<DiskWriterPool>, opts: &DownloadOptions, cancel: Option<&CancelSignal>) -> Result<(DownloadOutcome, watch::Receiver<ProgressSnapshot>)>` — 下载编排：Probe → 分块/顺序 → 完成/取消/暂停，返回结果与进度接收端。
- `pub struct DownloadOptions { config, strategy, dir, out_name, progress_tx }` — 下载选项。
- `pub enum DownloadOutcome { Completed, Cancelled, Paused(PauseReason) }` — 非失败结果也保留 `.idm-part`。
- `pub enum PauseReason { DiskFull }` — 暂停原因。

### `task/state.rs`
- `pub enum TaskState { Pending, Probing, Downloading, Finalizing, Completed, Failed, Cancelled, Paused }` — 任务状态机，受控 `transition`。
- `pub enum ChunkState { Pending, Downloading, Done, Failed }` — 分块状态机，受控 `transition`。
- `pub struct InvalidStateTransition<S: Debug + PartialEq + Eq>` — 非法转换错误。

### `task/progress.rs`
- `pub struct ProgressSnapshot { total_size, downloaded, speed, chunks }` — 进度快照（经 `watch` 发布，200ms 节流）。
- `pub struct ProgressPublisher` — 进度发布器。

### `task/cancel.rs`
- `pub struct CancelSignal` — 基于 `watch` 的取消信号（状态广播，取消不漏）：`new()` / `cancel()` / `is_cancelled()` / `subscribe()` / `cancelled()`。

### `disk`
- `pub struct DiskWriterPool` / `PoolChunk` — 落盘线程池与注册句柄（pool id 路由，A1 修复）。
- `pub struct TargetFile` / `AllocationInfo` / `AllocationStrategy { SetLen, Reserve, Sparse, Auto }` / `SpaceCheck { Enforce, Skip }` — 目标文件与空间分配策略（Auto = 稀疏则 Sparse 否则 SetLen）。
- `pub fn disk_free_space(path) -> Result<u64>` / `volume_filesystem(path) -> Result<String>` / `pub fn volume_supports_sparse(path) -> Result<bool>` — 卷信息查询。
- `pub struct ChunkWriter` / `PositionalWrite` — 位置化写入。

### `disk/win32.rs`
- `pub(crate) fn move_file(from: &Path, to: &Path) -> Result<()>` — 安全重命名，封装 `MoveFileExW` 不带 `MOVEFILE_REPLACE_EXISTING`（不覆盖用户同名文件）。

### `error.rs`
- `pub enum Error`（见第 3 节）/ `pub type Result<T>` / `Error::is_disk_full() -> bool` / `pub(crate) fn io_error(...)`。

---

## 3. 新增错误变体清单

- `Io { operation, path, offset, source }` — 带上下文的 IO 错误。
- `WriteZero { path, offset }` — 写入返回 0 字节（短写退化）。
- `DiskFull { path, needed, available }` — 创建前磁盘空间不足（预检查）。
- `DiskFullDuringWrite { path, offset }` — 写盘过程磁盘满（系统码 112/39 映射），`is_disk_full()` 同样为 true。
- `RangeOverlap { start, end }` — 区间重叠。
- `OutOfRange { offset, len, end }` — 写越出分配区间。
- `InvalidRange { start, end, file_len }` — 非法区间。
- `LengthMismatch { path, expected, actual }` — 打开已有文件长度不符（供 1c）。
- `PoolClosed` — 落盘线程池已关闭。
- `ChunkNotRegistered { pool_id, chunk_id, path }` — 写入/注销未注册 chunk（A1 修复）。
- `InvalidPoolConfig { message }` — 线程池构造参数非法（A3 修复）。
- `Http { url, source }` — HTTP 请求失败。
- `InvalidArgument { message }` — 参数非法（连接数越界 / scheme 不支持）。
- `UnsupportedContentEncoding { encoding }` — 非 identity 的 Content-Encoding，绝不写盘。
- `RemoteChanged` — 带 If-Range 却返回 200（远端变化）。
- `RangeNotHonored` — 未带 If-Range 却返回 200。
- `InvalidContentRange { requested, actual }` — 206 但 Content-Range 不一致。
- `UnexpectedEof { chunk_id, expected, received }` — 分块提前结束。
- `UnboundedRangeConflict { chunk_id }` — 无界区间冲突。

---

## 4. 状态机转换表

### 任务状态 `TaskState`
| from \ to | Probing | Downloading | Finalizing | Completed | Failed | Cancelled | Paused |
|-----------|---------|-------------|------------|-----------|--------|-----------|--------|
| Pending   | ✓       | ✓           |            |           |        |           |        |
| Probing   |         | ✓           |            |           | ✓      | ✓         |        |
| Downloading |       |             | ✓          |           | ✓      | ✓         | ✓      |
| Finalizing |        |             |            | ✓         | ✓      |           |        |
| Failed    |         |             |            |           |        |           |        | → `Pending` 可重置 |
| Cancelled |         |             |            |           |        |           |        | → `Pending` 可重置 |
| Paused    |         |             |            |           |        |           |        | → `Pending` 可重置 |

### 分块状态 `ChunkState`
| from \ to | Downloading | Done | Failed |
|-----------|-------------|------|--------|
| Pending   | ✓           |      |        |
| Downloading |          | ✓    | ✓      |

非法转换返回 `InvalidStateTransition { from, to }`。

---

## 5. 测试数量与结果、压力测试耗时

- 集成测试：`crates/idm-core/tests/phase1b.rs` **33** 个；`crates/idm-core/tests/disk_concurrency.rs` **5** 个。
- 模块单元测试：`error.rs`(4)、`state.rs`(3)、`cancel.rs`(3) 等随 `cargo test --workspace` 一并运行。
- `cargo test --workspace --locked`：**全部通过**（含上述集成 + 单元；`--ignored` 的 1GB 压力测试不计入）。
- `#[ignore]` 压力测试 `stress_1gb_16_connections`（本地手写 HTTP 服务器、确定性内容、逐字节校验）：
  - `size = 1073741824`（1 GiB），`connections = 16`
  - `download_elapsed = 1.08s`，`verify_elapsed = 1.31s`，`total_elapsed = 2.39s`
  - 结果：`1 passed; 0 failed`。

---

## 6. 验收脚本输出（`scripts/verify-workspace.ps1`）

15 步全部 PASS：

```
[STEP 1]  rustc/cargo 存在且 host = x86_64-pc-windows-msvc      PASS
[STEP 2]  Cargo.lock 存在                                      PASS
[STEP 3]  cargo fmt --all -- --check                           PASS
[STEP 4]  cargo check --workspace --all-targets --locked       PASS
[STEP 5]  cargo test --workspace --locked                      PASS
[STEP 6]  cargo clippy --workspace --all-targets --locked -D warnings  PASS
[STEP 7]  cargo run -p idm-cli（core_version 字段校验）        PASS
[STEP 8]  cargo tree 无禁止依赖（tauri/wry/sqlx/...）          PASS
[STEP 9]  Cargo.toml 静态检查无禁止直接依赖                   PASS
[STEP 10] 无 apps/ src-tauri/ package.json                    PASS
[STEP 11] .rs 中无 TODO/FIXME/todo!()/unimplemented!()        PASS
[STEP 12] unsafe 仅出现在 win32.rs                             PASS
[STEP 13] #[allow] 仅 win32.rs 的 #![allow(unsafe_code)]      PASS
[STEP 14] 重复依赖（仅信息）                                   PASS
[STEP 15] cargo doc --workspace --no-deps -D warnings          PASS
SUMMARY: all 15 steps executed; final result: PASS
```

---

## 7. 与原文不一致之处

1. **CLI 子命令命名**：实现使用 `idm-cli download <URL> ...`。较新且具权威性的 `phase1b1.md`（§七）明确以 `download` 为准（“`idm-cli download` 新增可选参数 `--cancel-after`”），且本次任务再次确认“子命令名保持 `download`，不要改名”。故以 `download` 为正式命名，不视为偏差（原 `phase1b.md` 的 `get` 已被新文件取代）。
2. **额外测试参数**：CLI 增加了 `--cancel-after <秒>`（phase1b1.md §七要求），仅用于验证协作式取消路径，不复写取消逻辑。
3. **提交数量**：原文预期 2 个提交，实际 3 个（见第 1 节说明）。
4. **子命令**：`download` 为主下载子命令（phase1b1.md 指定）；`bench-alloc` 为 Phase 0 遗留、原文允许保留。
5. **真实网络 CLI 验收**：`phase1b.md` 末尾明确“本阶段结束后，由我本人在真实网络上做 CLI 验收，不需要你做”；本次任务进一步改为“由我编写脚本、由用户在本机运行”。因此 2–1.5GB 真实 ISO 下载 + SHA256 校验由用户执行，本助手未做（详见第 8 节风险与第 10 节网络验收脚本）。

---

## 8. 发现的风险或疑问

1. **真实网络出口被限速（重要）**：本次在沙箱内对清华镜像做了连通性核验——`HEAD` 返回 200、支持 `Range`、`Accept-Ranges: bytes`、无 `Content-Encoding`。但实测 egress 吞吐仅约 **0.5–1.8 MB/s**（curl 100MB 区间 3 分钟仅下完约 85MB；CLI 进度亦约 1.8 MB/s）。按此速率，4GB ISO 需 **40 分钟以上甚至数小时**，在交互式会话内不具可行性。**建议用户在正常网络环境下完成真实网络验收**。
2. **`download()` 返回的 `watch::Receiver` 需保持存活**：调用方若提前 drop 进度发布端，进度推送会停止（下载本身不受影响）。CLI 已正确持有。
3. **稀疏文件实际占用**：`AllocationStrategy::Auto` 在 NTFS 上选 Sparse，逻辑长度预分配为全长，但物理占用随写入增长；非 NTFS 卷回退 SetLen，空间预检查已覆盖。
4. **`Cargo.lock` 由 `cargo generate-lockfile` 按当前工具链（rustc 1.99.1）解析**；验收用 `--locked` 通过，锁文件与 `Cargo.toml` 一致。
5. **TLS**：`build_client` 不暴露任何关闭证书校验的开关，默认后端证书链/主机名校验始终开启（符合安全约束）。

---

## 9. Phase 1b.1 补交内容（phase1b1.md 第一部分）

### 9.1 `request_timeout()` 修改前后与三层超时默认值

- **修改前**：`DownloadConfig::request_timeout()` 返回 `connect_timeout + read_timeout`（= 30s + 30s = **60s**），并用于**所有**请求的 `request.timeout()`（含分块 / 顺序下载请求）。慢速网络下，大分块因“读取全部响应体”触达该总时长上限而失败。
- **修改后（三层超时）**，默认值均来自 `DownloadConfig::default()`：
  - `connect_timeout` = **30s**：建立 TCP/TLS 连接的最长耗时（Client 级 `.connect_timeout()`）。
  - `read_timeout` = **30s**：两次收到响应体字节之间的**最长空闲间隔**（Client 级 `.read_timeout()`），不针对单个请求设总时长上限。
  - `request_timeout()` = `connect_timeout + read_timeout` = **60s**：**仅 Probe 使用**的总时长上限（Probe 不读取正文，可设上限）。

### 9.2 一到七各项修正：对应文件与测试名

| 项 | 修正 | 文件 | 测试名（均在 `crates/idm-core/tests/phase1b.rs`，除非另注） |
|----|------|------|------|
| 一 超时 | 三层超时；分块 / 顺序请求不设总时长 | `src/net/client.rs` | `timeout_slow_but_steady_succeeds`、`timeout_freeze_returns_error_and_retains_part` |
| 二 协作式取消 | 内部 `CancelSignal` + 协作式排空（≤5s）+ `purge` 兜底 | `src/task/download.rs`、`src/disk/pool.rs` | `cancel_before_probe`、`cancel_during_probe`、`cancel_after_probe_before_create`、`cancel_during_download`（均含 `assert_no_residual_handle`：rename 成功 + `open_existing` 重申请区间成功） |
| 三 改名不覆盖 | `win32::move_file`（MoveFileExW，无 `REPLACE_EXISTING`）；仅最终文件已存在才 `resolve_conflict` | `src/disk/win32.rs`、`src/task/download.rs`、`src/net/filename.rs` | `rename_does_not_overwrite_existing` |
| 四 不创建空 .idm-part | 删除 `ensure_part_file`；仅 `TargetFile` 创建成功后才存在 `.idm-part` | `src/task/download.rs` | `download_disk_full_at_creation_leaves_no_file`；另 `cancel_before_probe` / `cancel_during_probe` / `cancel_after_probe_before_create`（建文件前取消 → 无残留文件） |
| 五 冲突考虑 .idm-part | `resolve_conflict` 的 `occupied` 闭包：`name.ext` 或 `name.ext.idm-part` 任一存在即占 | `src/net/filename.rs` | `two_concurrent_same_url_different_names` |
| 六 弱 ETag + LM | 弱 ETag 但有 Last-Modified → 以 LM 作 If-Range，并逐分块校验响应弱 ETag | `src/task/download.rs` | `error_weak_etag_with_last_modified_still_detected`（另 `probe_weak_etag_flag`） |
| 七 CLI 测试参数 | `idm-cli download --cancel-after <秒>` | `crates/idm-cli/src/main.rs` | 见 9.4（CLI 二进制，无独立单元测；底层取消逻辑由 `cancel_*` 覆盖） |

> 非 Completed 结束时调用 `purge` 兜底（项二.3）由上述 `cancel_*` 与所有 `error_*`（如 `error_range_returned_200`、`error_connection_broken`、`error_etag_changed`、`error_bad_content_range`、`error_weak_etag_changed_on_206`、`error_gzip`）间接覆盖：它们都断言 `.idm-part` 被保留、且 rename 后 `open_existing` 可重申请区间。

### 9.3 win32.rs 新增 unsafe 块（MoveFileExW）及 SAFETY 理由

函数 `pub(crate) fn move_file(from, to)` 封装 `MoveFileExW`，标志为 `MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH`（**不带** `MOVEFILE_REPLACE_EXISTING`）。unsafe 调用位于 `crates/idm-core/src/disk/win32.rs`：

```rust
// SAFETY: `from_w` 与 `to_w` 都是 null 结尾的有效 UTF-16 字符串，且在本调用期间存活；
// 两个 `PCWSTR` 仅被同步读取；`dwFlags` 为编译期常量，无输出参数。
let ret = unsafe {
    MoveFileExW(
        from_w.as_ptr() as PCWSTR,
        to_w.as_ptr() as PCWSTR,
        MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH,
    )
};
```

SAFETY 理由：
- `from_w` / `to_w` 由 `to_wide(path)` 生成，末尾追加了 `0`（null 结尾），且都是局部 `Vec<u16>`，在 `unsafe` 块执行期间保持存活，不会被移动或释放。
- 两个 `PCWSTR` 仅作为参数被 Win32 同步读取，调用返回后不再被引用；无别名写回。
- `dwFlags` 为编译期常量，无输出参数、无指针输出。
- 目标已存在时 Win32 返回 `FALSE`，`last_os_error()` = `ERROR_ALREADY_EXISTS`，调用方据此转为 `Error::Io`，从而**不覆盖**用户已有同名文件。

### 9.4 `--cancel-after` 是否已实现、是否与 Ctrl+C 同一 CancelSignal

- **已实现**：`crates/idm-cli/src/main.rs` 的 `dl::run` 解析 `--cancel-after <秒>`（非负整数，单位秒）。
- **同一 CancelSignal**：`--cancel-after` 与 Ctrl+C 走**完全相同**的取消路径——二者都作用在**同一个** `Arc<CancelSignal>`（`signal`）上：
  - Ctrl+C：`tokio::spawn` 监听 `tokio::signal::ctrl_c()`，触发时调用 `signal_task.cancel()`。
  - `--cancel-after`：`tokio::spawn` 一个 `sleep(secs)` 定时器，到点调用 `signal_timer.cancel()`（即同一个 `signal` 的克隆）。
  - 两者都仅调用 `CancelSignal::cancel()`，不另写取消逻辑；下载函数内部对“取消”的判定、退出码、输出完全一致。
- 因此 `--cancel-after 15` 触发的就是与手动 Ctrl+C 等价的中断，用于自动化测试取消行为。

### 9.5 当前测试总数

- `crates/idm-core/tests/phase1b.rs`：**36** 个（含 1 个 `#[ignore]` 1GB 压力测试 `stress_1gb_16_connections`）。
- `crates/idm-core/tests/disk_concurrency.rs`：**6** 个。
- 各模块单元测试：`state.rs`(3)、`cancel.rs`(3)、`lib.rs`(4)、`error.rs`(4)、`headers.rs`(9)、`filename.rs`(8)、`range.rs`(7)、`target.rs`(5)、`win32.rs`(1)、`write.rs`(4)。
- **合计：90** 个测试函数（默认运行 89 个；1 个 `#[ignore]` 压力测试需 `--ignored` 运行）。
- `cargo test --workspace --locked` 全绿；`cargo test --release -p idm-core --test phase1b -- --ignored` 压力测试 **2.39s** 通过。

---

## 10. 本轮收尾：Cargo.lock 确认、网络验收脚本、正式测试文件

### 10.1 Cargo.lock 未被 `generate-lockfile` 改动后提交

- `git status`：干净（本轮开始前），`git diff HEAD~4 -- Cargo.lock --stat` 显示相对 `HEAD~4` 有一处**新增**：
  - 新增包 `signal-hook-registry v1.4.8`（依赖 `errno`、`libc`），并在 `tokio` 的依赖列表中新增 `signal-hook-registry`。
- **这不是版本漂移**：没有删除、升级或降级任何已有包，只是新增一个传递依赖。来源是 Phase 1b.1 为 CLI 的 Ctrl+C 监听启用了 tokio 的 `signal` 特性（`idm-cli` 的 `tokio` 增加 `signal`），`signal-hook-registry` 是该特性在 Windows/Unix 上的传递依赖。
- 该条目在 `4de2f06`（`phase1b.1: timeouts, cooperative cancel, safe rename`）中随代码一同提交，属于预期改动，未做任何处理。
- 本轮工作树中 `Cargo.lock` 无未提交修改（`git status` 仅显示 docs/scripts 的改动）。

### 10.2 报告位置调整

- 按本轮要求，`docs/prompts/phase1b-report.md` 已移动为 `docs/phase1b-report.md`；`docs/prompts/` 只保留任务原文（`phase1a.md`、`phase1b.md`、`phase1b1.md`）。

### 10.3 `scripts/network-acceptance.ps1`

- 由用户在本机手动运行，自动完成 `phase1b1.md` 第二部分的 A 到 F 项；纯 ASCII、兼容 PowerShell 5.1。
- 参数：`-Url`（必填）、`-Sha256`（必填）、`-Dir`（默认 `D:\dl-test`）、`-Exe`（默认 `.\target\release\idm-cli.exe`）、`-KeepFiles`（开关）。
- 每次 CLI 调用用 `Start-Process -Wait -PassThru`，stdout/stderr 分别重定向到 `$Dir\logs\` 下按步骤命名的日志，读取 `ExitCode`，`Stopwatch` 计时；逐项 PASS/FAIL，单项 FAIL 继续执行，最后汇总，有任何 FAIL 则 `exit 1`。
- 开始前：exe 不存在则提示 `cargo build --release -p idm-cli` 并退出；`-Dir` 已存在且有文件时列出内容并退出。
- 输出：控制台汇总表 + 单连接/8 连接速度对比；完整结果写入 `docs/phase1b-network-acceptance.md`。未指定 `-KeepFiles` 时删除下载文件、保留 `logs`。

### 10.4 脚本自测（验证脚本本身）

真实网络验收由用户在本机执行；本轮仅验证脚本 A 到 F 的逻辑。由于 `--cancel-after 15` 要求下载在 15 秒时仍在进行，用**本地回环限速 HTTP 服务器**提供一份有官方 SHA256 的真实小文件：

- 自测文件：`node-v22.11.0-headers.tar.gz`（8,780,269 字节，约 8.4 MiB），取自清华 `nodejs-release/v22.11.0/`，官方校验文件 `SHASUMS256.txt` 给出 SHA256 `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5`；下载后 `Get-FileHash` 与官方值一致。
- 回环服务器全局限速约 350 KB/s，使每次下载约 22–24 秒，从而 D 项在 15 秒时确实取消了一个进行中的下载。
- 自测期间发现并修复了脚本的两个真实 bug（已修复后重跑全绿）：
  1. `Rename-Item` 在 PS 5.1 没有 `-Destination` 参数，应为 `-NewName`。
  2. `Start-Process -PassThru` 配合重定向输出时，PS 5.1 下须先设 `EnableRaisingEvents = $true`，否则 `WaitForExit()` 后 `ExitCode` 为 `$null`（E 项退出码读不到）。
- 最终结果（第三次完整运行，退出码 0）：

| 项 | 内容 | 结果 |
|----|------|------|
| A | `--connections 1`，22.84s，0.37 MB/s | PASS |
| B | `--connections 8`，23.99s，0.35 MB/s，生成 `name (1).ext` | PASS |
| C | A、B 两文件 SHA256 均等于官方值 | PASS |
| D | `--cancel-after 15`，退出码 1，`.idm-part` 保留，`Rename-Item` + `Remove-Item` 均成功 | PASS |
| E | 两个并发进程各 `--connections 4`，退出码均 0，新增两个不同名字文件且哈希正确 | PASS |
| F | 无 `.idm-part`、无 0 字节文件 | PASS |

- 完整原始输出见 `docs/phase1b-network-acceptance.md`（该文件当前保存的是回环自测结果，**用户在本机运行真实验收时会被脚本覆盖**）。自测的 A/B 速度比 0.95x 由全局限速决定，不代表真实多连接加速比。

### 10.5 正式测试文件与可直接复制运行的命令

- 镜像站：`mirrors.tuna.tsinghua.edu.cn`（清华 TUNA）。
- ISO：`debian-13.7.0-amd64-netinst.iso`，**792,723,456 字节（约 756 MiB）**，在要求的 500MB–1.5GB 范围内。
- 官方校验文件（同目录，已下载核对）：`https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/SHA256SUMS`，对应行：
  `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355  debian-13.7.0-amd64-netinst.iso`
- `HEAD` 确认：200、`Content-Length: 792723456`、`Accept-Ranges: bytes`、`Content-Type: application/octet-stream`，支持 Range 且总长已知。
- 在仓库根目录执行（先确保 `D:\dl-test` 不存在或为空）：

```
powershell -ExecutionPolicy Bypass -File .\scripts\network-acceptance.ps1 -Url "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso" -Sha256 "a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355"
```

- 若某机器单连接 60 秒内下完（导致 D 项无法在 15 秒时取消），按 `phase1b1.md` 第二部分要求换更慢的镜像站或更大文件；脚本的 D 项固定使用 `--cancel-after 15`。

### 10.6 与本文不一致之处

- 无。（子命令名保持 `download`；真实网络验收按本轮约定改由用户在本机运行脚本完成。）
