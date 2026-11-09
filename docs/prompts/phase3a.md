# 任务：idm-rs Phase 3a —— 高阶网络控制层（全局动态限速、按 Host 连接限制、接管连接零重叠、代理支持）

沿用全部工作规则（见 `docs/prompts/` 下历次任务原文）。开始前先把本任务原文原样保存为 `docs/prompts/phase3a.md`，确认 `git status` 干净（基线为 `83d6d5c`）。
本阶段只做网络控制层与单/多任务共享控制原语，不做多任务排队调度器（留到 Phase 3b）。本阶段所有测试均通过本地回环服务器完成，不访问外网。
**提交前必须完成第七部分的自查表，任何一项测试缺失都不得提交。**

## 一、运行中可动态调整的双层限速器（`crates/idm-core/src/net/ratelimit.rs`）

1. **动态调整速率**：
   - `RateLimiter::new(bytes_per_sec: Option<u64>)`：`None` 或 `Some(0)` 表示不限速。
   - 新增 `pub fn set_rate(&self, bytes_per_sec: Option<u64>)` 与 `pub fn current_rate(&self) -> Option<u64>`，支持在下载进行中随时修改限速值，无需重启任务。
   - **关键边界**：
     - 从限速改为不限速（`None` 或 `0`），或大幅调高限速值时，必须立即唤醒所有正在 `acquire` 中等待令牌的任务（使用 `tokio::sync::Notify` 或 `watch`），在 100ms 内按新速率生效。
     - 从不限速或高限速调低到小限速（例如从 50 MiB/s 降到 1 MiB/s）时，必须立即将桶内当前剩余令牌数截断到新桶容量上限（1 秒配额，最小 256 KiB），防止积攒的旧令牌导致调低后瞬间突发大流量。
2. **双层限速组合（全局 + 单任务）**：
   - 新增 `BandwidthLimiter`（或等效封装），内部可同时持有 `task_limiter: Option<Arc<RateLimiter>>` 与 `global_limiter: Option<Arc<RateLimiter>>`。
   - 每次读取网络数据块后，若配置了单任务限速和/或全局限速，必须同时满足两者的配额（依次 `acquire`），且等待期间均响应 `CancelSignal`。
   - `DownloadOptions` 中保留单任务 `max_speed: Option<u64>`，并新增可选的 `global_limiter: Option<Arc<RateLimiter>>`，使多个并发 `download` / `resume` 调用可以共用同一个全局限速器实例。

## 二、接管模式统一延迟启动，消除瞬间 +1 连接重叠（`split.rs` & `download.rs`）

### 1. 背景与现状
在 Phase 2.2.1 中，接管模式（`Takeover`）的切分点已改为 `split = reserved`（不对齐），但只有 `reserved == start`（零进度）走了 `PendingChunk` 延迟分配；当 `reserved > start` 时仍立即在 `try_split` 内启动新连接，而被接管的旧连接要稍晚几毫秒才关闭，导致瞬间并发连接数会达到 `connections + 1`。

### 2. 修正要求
- 将**所有 `SplitMode::Takeover`**（无论 `reserved == start` 还是 `reserved > start`）统一改为 `PendingChunk` 延迟启动：
  1. 第 4 步：把共享 `end` 缩为 `split`（即 `reserved`），同时更新 `watch` 唤醒被接管分块。
  2. 第 5 步：执行数据库切分事务 `split_chunk`。
  3. 第 6 步：当 `split > start` 时调用 `pool.shrink_end`（若被接管分块已提前退出返回 `ChunkNotRegistered`，记 warn 跳过）；当 `split == start` 时跳过 `shrink_end`。
  4. 第 7 步：**一律不立即启动新连接**，而是记入主循环的 `PendingChunk` 列表（计入活动连接名额）。等被接管分块的任务以 `ChunkResult::Completed { db_index }` 退出（证明其旧连接已丢弃关闭、写入器已释放）后，再为新分块执行 `allocate_chunk` -> `register` -> 启动新连接（保留 5 秒超时保护）。
  5. 边界保护：被接管分块如果在最后一次在途写盘或重试前发现 `acked == shared.end()`，必须直接返回 `Completed`，不得因网络侧刚好断开而误走重试或报错路径。
- 更新 `ARCHITECTURE.md` 第 10.5 节：把“当 `reserved == start` 时新分块延后到被接管分块以 Completed 退出后再分配写入器并启动连接”扩展为“**所有接管模式（Takeover）下，新分块一律延后到被接管分块以 Completed 退出、关闭旧连接并释放写入器后，再分配写入器并启动新连接，确保任何时刻活动连接数严格不超过上限**”。

## 三、按服务器（Host）限制连接数（`crates/idm-core/src/net/host_limit.rs`）

1. **`HostKey` 归一化**：
   - 从请求 `Url` 提取 `(scheme_lowercase, host_lowercase, effective_port)`（例如 `http` 默认 80，`https` 默认 443）。
2. **`HostConnectionLimiter`**：
   - `HostConnectionLimiter::new(max_per_host: usize)`：默认每个 Host 最多 8 个并发连接（合法范围 1..=64，为 0 时返回配置错误）。
   - 内部按 `HostKey` 维护 `Arc<tokio::sync::Semaphore>`，对外提供 `pub async fn acquire(&self, url: &Url, cancel: &CancelSignal) -> Result<HostPermit>`。
   - **许可证生命周期（严格执行）**：
     - 分块每次发起 HTTP 请求（`req.send()`）之前，按目标 URL（重定向后的最终 URL）申请 `HostPermit`，等待期间监听 `CancelSignal`。
     - 在读取响应流期间持有 `HostPermit`。
     - **一旦单次请求结束、出错或被唤醒提前关闭连接，必须立即释放 `HostPermit`**。特别是进入重试退避等待（backoff sleep）之前，必须先 drop 掉 `HostPermit`，绝不允许在退避睡眠期间死占着服务器的连接名额；下一次重试发请求前再重新 `acquire`。
   - `DownloadConfig` 新增 `max_connections_per_host: usize`（默认 8）；`DownloadOptions` 新增可选的 `host_limiter: Option<Arc<HostConnectionLimiter>>`（未传入时任务内部按 `DownloadConfig` 默认创建一个）。
   - CLI `download` 与 `resume` 新增 `--max-host-connections <N>` 参数。

## 四、HTTP / HTTPS / SOCKS5 代理支持（`crates/idm-core/src/net/client.rs`）

1. **依赖特性**：
   - 在根目录 `Cargo.toml` 的 `reqwest` 依赖中增加 `"socks"` 特性：`reqwest = { version = "0.13", features = ["stream", "socks"] }`。
   - 注意：严禁执行 `cargo update`。仅允许因开启 `"socks"` 特性而由 Cargo 解析新增的必要子依赖，并在报告中列出 `Cargo.lock` 新增的包名与版本。
2. **代理配置类型**：
   - 在 `net/client.rs` 定义并公开 `ProxyConfig`：
     - `System`（默认，使用系统/环境变量代理）
     - `Disabled`（显式禁用代理，调用 `ClientBuilder::no_proxy()`）
     - `Custom(String)`（支持 `http://`、`https://`、`socks5://`、`socks5h://`，支持 URL 内嵌 `user:pass@host:port` 认证）
   - 构建 `Client` 时校验代理 URL：若格式非法或 scheme 不受支持，返回新增的强类型错误 `Error::InvalidProxy { url: String, reason: String }`。
3. **CLI 参数**：
   - `idm-cli download` 与 `idm-cli resume` 新增 `--proxy <URL>` 与 `--no-proxy`。同时传入这两个参数时打印错误用法并以退出码 2 退出。

## 五、测试要求（统一放在 `crates/idm-core/tests/phase3a.rs` 及对应单元测试）

1. **动态调速单元测试与集成测试**：
   - `dynamic_rate_increase_and_unlimited`：16 MiB 文件，初始限速 1 MiB/s 启动下载；1.5 秒后调用 `set_rate(None)` 切换为不限速，断言总耗时小于 3.5 秒（若未生效需 16 秒），文件逐字节正确。
   - `dynamic_rate_decrease_clamps_burst`：初始限速 50 MiB/s 运行 500ms 后调用 `set_rate(Some(1 MiB/s))`，随后统计接下来 2 秒内的实际传输字节数，断言不超过 2.5 MiB（证明旧桶容量已被截断，没有突发泄漏）。
2. **全局 + 单任务双层限速测试**：
   - `global_rate_limiter_shared_across_tasks`：同时启动 2 个下载任务（各 4 个连接），共用一个 `2 MiB/s` 的全局 `RateLimiter`，每个任务下载 6 MiB（合计 12 MiB），断言总耗时在 5.0 到 7.2 秒之间（合计平均速度不超过 2.4 MiB/s），两个文件逐字节正确。
3. **接管零重叠并发峰值测试**：
   - `takeover_never_exceeds_connection_limit`：4 个连接，第 0 号分块发送 100,000 字节（非对齐，`reserved > start`）后完全停住。服务器端在不依赖 `peek` 事后剔除的前提下，精确统计新分块请求到达时的活动连接数，断言全程并发峰值严格 `<= 4`，且旧连接在新请求到达前已被客户端关闭。
4. **跨任务按 Host 限制连接数测试**：
   - `host_limiter_caps_connections_across_tasks`：3 个下载任务同时从同一个本地测试服务器下载，每个任务设 `connections = 4`（共请求 12 个连接），共用一个 `max_per_host = 5` 的 `HostConnectionLimiter`。服务器端统计整个过程中的最大并发活动请求数，断言峰值严格 `== 5`（绝不超过 5），3 个文件全部逐字节正确。
5. **重试退避期间释放 Host 名额测试**：
   - `host_permit_released_during_retry_backoff`：`max_per_host = 1`，退避基数设为 5 秒。第 0 号分块首次请求被服务器断开并进入 5 秒退避等待；此时第 1 号分块必须能立即拿到唯一的 Host 名额并在 1 秒内完成下载，无需等待第 0 号分块的 5 秒退避结束。
6. **HTTP 代理与 SOCKS5 代理端到端测试**：
   - 在 `tests/support/` 中用 `tokio::net::TcpListener` 编写轻量级的本地测试用 HTTP 转发代理与 SOCKS5（RFC 1928 `CONNECT`）转发代理，并记录经过代理的请求数。
   - `download_via_http_proxy` 与 `download_via_socks5_proxy`：8 个连接分块下载，断言所有 Probe 与分块 Range 请求均实际经过代理服务器转发（代理计数 > 0），最终文件逐字节正确。
   - `invalid_proxy_url_rejected`：传入非法代理 URL（如 `ftp://127.0.0.1:1080` 或乱码），断言返回 `Error::InvalidProxy`。
7. **回归测试**：原有全部单元测试、集成测试、Phase 2 的 50 种子并发测试及两个 20 轮崩溃测试全部通过。

## 六、提交

- 新增的每个测试单独连续运行 10 次，全部通过。
- 运行 `scripts/verify-workspace.ps1`，16 步全部 PASS，exit code 0。
- 只暂存 `docs crates scripts ARCHITECTURE.md Cargo.toml Cargo.lock`，提交信息：`phase3a: dynamic global ratelimit, host limiter, and proxy support`，不得 push，不打 tag。

## 七、提交前自查表（必须完成）

提交前在报告中列出下表，任何一项测试名为空或未满 10/10 通过均不得提交：

| 第五部分编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|---|

## 八、报告

- 第七部分的自查表
- `Cargo.lock` 因开启 `reqwest` 的 `socks` 特性新增的依赖列表（确认已有依赖版本未变）
- 动态限速、接管延迟启动、Host 许可证在重试退避前释放这三处关键逻辑的代码位置（文件:行号）
- 第 1、2、3、4、5、6 项测试的实测关键指标（耗时、并发峰值、代理转发请求数）
- 测试总数与验收脚本输出
- 与本文不一致之处（没有就写“无”）
