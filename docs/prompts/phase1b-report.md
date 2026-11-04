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

1. **CLI 子命令命名**：原文 B5 规定 `idm-cli get <URL> ...`；实现使用 `idm-cli download <URL> ...`（更直观）。属命名偏差，功能等价。如需严格对齐原文，可改回 `get`。
2. **额外测试参数**：CLI 增加了 `--cancel-after <秒>`（原文未要求），仅用于验证协作式取消路径，不复写取消逻辑。
3. **提交数量**：原文预期 2 个提交，实际 3 个（见第 1 节说明）。
4. **分阶段子命令**：CLI 额外提供 `bench-alloc`（Phase 0 遗留，原文允许保留）与 `download`。
5. **真实网络 CLI 验收**：原文末尾明确“本阶段结束后，由我本人在真实网络上做 CLI 验收，不需要你做”。因此 2–4GB 真实 ISO 下载 + SHA256 校验由用户执行，本助手未做（详见第 8 节风险）。

---

## 8. 发现的风险或疑问

1. **真实网络出口被限速（重要）**：本次在沙箱内对清华镜像做了连通性核验——`HEAD` 返回 200、支持 `Range`、`Accept-Ranges: bytes`、无 `Content-Encoding`。但实测 egress 吞吐仅约 **0.5–1.8 MB/s**（curl 100MB 区间 3 分钟仅下完约 85MB；CLI 进度亦约 1.8 MB/s）。按此速率，4GB ISO 需 **40 分钟以上甚至数小时**，在交互式会话内不具可行性。**建议用户在正常网络环境下完成真实网络验收**。
2. **`download()` 返回的 `watch::Receiver` 需保持存活**：调用方若提前 drop 进度发布端，进度推送会停止（下载本身不受影响）。CLI 已正确持有。
3. **稀疏文件实际占用**：`AllocationStrategy::Auto` 在 NTFS 上选 Sparse，逻辑长度预分配为全长，但物理占用随写入增长；非 NTFS 卷回退 SetLen，空间预检查已覆盖。
4. **`Cargo.lock` 由 `cargo generate-lockfile` 按当前工具链（rustc 1.99.1）解析**；验收用 `--locked` 通过，锁文件与 `Cargo.toml` 一致。
5. **TLS**：`build_client` 不暴露任何关闭证书校验的开关，默认后端证书链/主机名校验始终开启（符合安全约束）。
