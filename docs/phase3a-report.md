# Phase 3a 报告：高阶网络控制层

- 任务原文：`docs/prompts/phase3a.md`；开始时 `git status` 干净，基线提交 `83d6d5c`。
- 全部测试使用 127.0.0.1 上的本地测试服务器 / 本地代理，未访问外网。
- 提交信息：`phase3a: dynamic global ratelimit, host limiter, and proxy support`
  （只暂存 `docs crates scripts ARCHITECTURE.md Cargo.toml Cargo.lock`，未 push、未打 tag）。

## 一、提交前自查表（第七部分）

| 第五部分编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|
| 1 | 动态调速：升高/改为不限速 100ms 内生效；降低时截断旧令牌无突发 | `dynamic_rate_increase_and_unlimited`、`dynamic_rate_decrease_clamps_burst`、`net::ratelimit::tests::dynamic_rate_change_clamps_tokens_and_wakes_waiters` | **10/10、10/10、10/10** |
| 2 | 全局 + 单任务双层限速，2 任务共享 2 MiB/s | `global_rate_limiter_shared_across_tasks`、`net::ratelimit::tests::bandwidth_limiter_applies_both_layers` | **10/10、10/10** |
| 3 | 接管零重叠：4 连接峰值严格 ≤ 4，旧连接先关闭 | `takeover_never_exceeds_connection_limit`、`task::split::tests::all_takeovers_use_deferred_allocation` | **10/10、10/10** |
| 4 | 跨任务按 Host 限制连接数，峰值严格 == 5 | `host_limiter_caps_connections_across_tasks`、`net::host_limit::tests::host_key_normalizes_scheme_host_and_port`、`net::host_limit::tests::zero_and_excessive_limits_are_rejected` | **10/10、10/10、10/10** |
| 5 | 重试退避前释放 Host 名额 | `host_permit_released_during_retry_backoff`、`net::host_limit::tests::host_permit_releases_on_drop_and_wait_is_cancellable` | **10/10、10/10** |
| 6 | HTTP / SOCKS5 代理端到端、非法代理 URL | `download_via_http_proxy`、`download_via_socks5_proxy`、`invalid_proxy_url_rejected`、`net::client::tests::invalid_proxy_urls_are_rejected`、`net::client::tests::supported_proxy_urls_build_a_client`、`net::client::tests::proxy_config_default_is_system`、`dl::tests::proxy_and_no_proxy_are_mutually_exclusive` | **7 个测试均 10/10** |
| 7 | 回归：原有全部测试、Phase 2 的 50 种子并发测试、两个 20 轮崩溃测试 | `eight_connections_sixty_four_mib_fifty_seeds`、`crash_resume_twenty_rounds`、`crash_resume_with_split_twenty_rounds` | **10/10、10/10、10/10**（`cargo test --workspace --locked` 与 `verify-workspace.ps1` 另见第五节） |

新增/改动过的其余单元测试，同样各 10/10：
`net::ratelimit::tests::capacity_is_one_second_quota_with_256kib_floor`、
`net::ratelimit::tests::acquire_waits_for_one_second_quota`、
`net::ratelimit::tests::concurrent_acquires_share_one_bucket`、
`net::host_limit::tests::host_permit_releases_on_drop_and_wait_is_cancellable`。

运行方式：单元测试 `cargo test -p idm-core --lib --locked <名字>` /
`cargo test -p idm-cli --bin idm-cli --locked <名字>`；集成测试
`cargo test -p idm-core --test phase3a --locked <名字>`；回归
`cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1`。
无一项测试名为空，无一项低于 10/10。

## 二、`Cargo.lock` 因开启 `reqwest` 的 `socks` 特性新增的依赖

- 根目录 `Cargo.toml`：`reqwest = { version = "0.13", features = ["stream", "socks"] }`。
- **未执行 `cargo update`**。`git diff -- Cargo.lock` 为空：新增包 **无**，已有依赖版本 **未变**（206 个 package 条目逐一保持不变）。
- 原因：本工作区解析到的 `reqwest 0.13.5` 把 `socks` 定义为空特性（`socks = []`），SOCKS5 支持由 reqwest / hyper-util 内部 `#[cfg(feature = "socks")]` 代码实现，不引入 `tokio-socks` 之类的新子依赖；`cargo tree -e features -i reqwest` 可见 `reqwest feature "socks"` 已启用，端到端 SOCKS5 测试也实际通过了本地 SOCKS5 代理。
- 因此报告中的"新增包名与版本"列表为：**空列表**。

## 三、三处关键逻辑的代码位置

### 1. 动态限速（`crates/idm-core/src/net/ratelimit.rs`）

| 逻辑 | 位置 |
|---|---|
| `RateLimiter::new(Option<u64>)` / `None`、`Some(0)` 表示不限速 | `:59` |
| `current_rate()` | `:74` |
| `set_rate()`：按旧速率补齐 → 更换 rate/capacity → **把令牌截断到新桶容量上限** → `watch` 代际号 +1 唤醒等待者 | `:87`（截断在 `:100`） |
| `acquire()`：`sleep(wait)` 与 `generation.changed()` 并列，速率一变立即重算 | `:138`（`select!` 在 `:167`） |
| `BandwidthLimiter`（单任务 + 全局两层依次 `acquire`） | `:179`、`:201` |
| 调用点：`download.rs` 组装两层限速器 | `crates/idm-core/src/task/download.rs:511`~`:522` |
| 调用点：读块后 `select!` 中监听取消 | `download.rs:1549`~`:1556` |

### 2. 接管统一延迟启动（`crates/idm-core/src/task/split.rs` / `download.rs`）

| 逻辑 | 位置 |
|---|---|
| `needs_deferred_allocation(mode)`：所有 `Takeover` 一律延迟 | `split.rs:266` |
| 第 6 步：`split > start` 才 `shrink_end`；`split == start` 跳过 | `split.rs:509` |
| 第 7 步：一律 `pending.push(PendingChunk)`（含 5 秒 deadline） | `split.rs:522`~`:531` |
| 主循环：`Completed { db_index }` 到达后才分配写器/启动新连接 | `download.rs:666`、`:727`（`process_pending_chunks` 在 `:1680`） |
| 超时保护（5 秒未结束 → 任务失败） | `download.rs:1695`~`:1704` |
| 边界保护：`acked == shared.end()` 时直接 Completed（在途写盘 / 重试前 / 网络错误 / 响应体提前结束） | `download.rs:1247`、`:1518`、`:1598`、`:1610` |
| ARCHITECTURE 10.5 修订 | `ARCHITECTURE.md:131` |

### 3. Host 许可证在重试退避前释放（`crates/idm-core/src/net/host_limit.rs` / `download.rs`）

| 逻辑 | 位置 |
|---|---|
| `HostKey` 归一化 `(scheme, host, effective_port)` | `host_limit.rs:30` |
| `HostConnectionLimiter::new`（1..=64，0 报配置错误） | `host_limit.rs:64` |
| `acquire()`：按 Host 取 `Semaphore`，`select!` 监听 `CancelSignal` | `host_limit.rs:94` |
| `HostPermit`（RAII，drop 即还名额） | `host_limit.rs:109` |
| 每次 `req.send()` 前申请，函数返回即 drop（退避在 `run_chunk_inner` 中、本函数已返回） | `download.rs:1377`~`:1386` |
| 未注入时按 `DownloadConfig::max_connections_per_host` 创建 | `download.rs:516`~`:522` |

### 4. 代理（补充）

`crates/idm-core/src/net/client.rs:13`（`ProxyConfig`）、`:25`（URL 校验 → `Error::InvalidProxy`）、`:185`~`:188`（`no_proxy()` + `Proxy::all`）；`Error::InvalidProxy` 在 `crates/idm-core/src/error.rs:91`；CLI 参数在 `crates/idm-cli/src/main.rs:402`~`:430`（download）与 `:826`~`:856`（resume）。测试代理在 `crates/idm-core/tests/support/proxy.rs`，精确连接计数服务器在 `crates/idm-core/tests/support/conn_probe.rs`。

## 四、第 1 ~ 6 项实测关键指标

（以下为最终代码上 `cargo test -p idm-core --test phase3a --locked -- --nocapture --test-threads=1` 一次完整运行的打印值；10 次单独运行的全部结果见第一节。）

| 项 | 关键指标 |
|---|---|
| 1 升速/不限速 | 16 MiB，初始 1 MiB/s，1.5 s 后 `set_rate(None)`：总耗时 **1.617 s**（未生效需 ~16 s，断言 < 3.5 s），文件逐字节正确 |
| 1 降速截断 | 64 MiB，初始 50 MiB/s，500 ms 后 `set_rate(Some(1 MiB/s))`：稳定后 2 秒窗口服务器实际写出 **2,031,616 字节（1.94 MiB）** ≤ 2.5 MiB；10 次单独运行均为该值；`current_rate() = 1 MiB/s`、`capacity() = 1 MiB` |
| 2 双层/共享全局 | 2 任务 × 6 MiB、各 4 连接，共享 2 MiB/s：总耗时 **6.074 s**（断言 5.0~7.2 s），合计 12,582,912 字节，两个文件逐字节正确 |
| 3 接管零重叠 | 4 连接、100000 字节非对齐停住 20 s：`peak_active = 4`（严格 ≤ 4），旧连接在 **50 ms** 时被客户端关闭，新请求在关闭之后到达，接管 1 次，下载 **82 ms** 完成，文件逐字节正确 |
| 4 跨任务 Host 上限 | 3 任务 × 4 连接（12 个请求）共享 `max_per_host = 5`：服务器全程并发峰值 **5**（严格 == 5），3 个文件逐字节正确 |
| 5 退避释放名额 | `max_per_host = 1`、退避基数 5 s：第 1 号分块 **0.035 s** 完成（要求 < 1 s），重试 0 次，文件逐字节正确；整体下载在第 0 号分块 5 s 退避后重试完成 |
| 6 代理 | 8 连接分块下载：HTTP 代理转发 **10** 个请求、SOCKS5 代理转发 **9** 个请求（= 1 Probe + 8 个 Range 请求，均 ≥ 9）；耗时 158.7 ms / 136.9 ms；两个文件逐字节正确。`invalid_proxy_url_rejected`：3/3 个非法 URL 均返回 `Error::InvalidProxy` |

## 五、测试总数与验收脚本输出

- `cargo test --workspace --locked`：**200 passed，0 failed，4 ignored**
  （idm-cli 8 + phase1d 1、idm-core lib 79、disk_concurrency 5、phase1b 39、
  phase1c1 12、phase1c_pre 2、phase1c_resume 9、phase1c_store 11、phase1d 13、
  phase2 12、phase3a 9；ignored 为 2 个崩溃测试 + 2 个原有 `#[ignore]`）。
- 两个崩溃测试各 20 轮：`cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1`
  → 2 passed（约 88 s）；另单独连续运行 10 次全部通过。
- Phase 2 的 50 种子并发测试单独连续运行 10 次全部通过。
- `scripts/verify-workspace.ps1`：**16 个步骤全部 PASS，exit code 0**
  （`[STEP 1]` rustc/host → `[STEP 3]` fmt → `[STEP 4]` check → `[STEP 5]` test →
  `[STEP 6]` clippy -D warnings → `[STEP 7]` CLI → `[STEP 8..13]` 静态检查 →
  `[STEP 15]` cargo doc → `[STEP 16]` 崩溃测试）。
- `cargo fmt --all -- --check`：通过；`cargo clippy --workspace --all-targets --locked -- -D warnings`：0 警告。

## 六、与本文不一致之处

1. **`set_rate` 额外清掉预扣欠债**：任务只要求"把桶内剩余令牌截断到新桶容量上限"。实现为
   `tokens = tokens.min(new_capacity).max(0.0)`：正向令牌按字面截断到新容量（10.5 节要求的
   1 秒配额、最小 256 KiB），同时把旧速率下"预扣 + 欠债"模型留下的负令牌清零。
   否则从 50 MiB/s 降到 1 MiB/s 时，约 1 MiB 的欠债要按新速率再还 1 秒，造成额外停顿，
   不符合"速率变更立即生效"。单元测试断言了截断后的令牌恰为新容量 1 MiB。
2. **`dynamic_rate_decrease_clamps_burst` 的测量方式**：任务写"随后统计接下来 2 秒内的
   实际传输字节数"。实现用测试服务器新增的 `bytes_sent`（实际写出的正文字节数，受 TCP
   背压约束）而非进度快照的落盘字节数，并在 `set_rate` 后先稳定 **300 ms** 再开始 2 秒
   窗口——50 MiB/s 阶段会有若干 MiB 滞留在内核/TCP 缓冲区里，若在变更瞬间开始计时，
   测到的是"旧数据排空"而不是新速率下的传输量。该测试使用 1 个连接、64 MiB 文件
   （任务未限定连接数与文件大小），窗口结束后再 `set_rate(None)` 让下载尽快收尾。
3. **动态调速通过 `DownloadOptions::global_limiter` 注入句柄**：任务只新增了
   `global_limiter`，未要求暴露任务内部由 `max_speed` 创建的 `RateLimiter` 句柄；
   两个动态调速集成测试把可调速的限速器作为 `global_limiter` 传入（同样受
   `BandwidthLimiter` 双层配额约束）。`max_speed` 字段按原文保留不变。
4. **`HostConnectionLimiter::new` 返回 `Result<Self>`**：任务写"为 0 时返回配置错误"，
   因此构造函数以强类型 `Result` 返回；`Default` 直接构造 8 的合法默认值。
5. **新增 `Error::Cancelled` 变体**：`HostConnectionLimiter::acquire` 在等待名额时被取消
   信号打断时返回它，`attempt_chunk_once` 把它映射成 `ChunkExit::Cancelled`。
6. **`ProxyConfig::Custom` 先调用 `ClientBuilder::no_proxy()` 再挂自定义代理**：确保
   环境变量里的系统代理不会与自定义代理叠加，使回环代理测试在任何环境下都确定。
7. **测试辅助设施**：新增 `tests/support/proxy.rs`（HTTP 转发 + SOCKS5 RFC1928 CONNECT）、
   `tests/support/conn_probe.rs`（accept 时 +1 / 检测到关闭时 -1 的精确连接计数服务器，
   不使用 `peek` 事后剔除）、`ServerConfig::bytes_sent`。均为测试代码，不改变生产逻辑。
8. 任务要求"全部原有测试通过"：为满足新增的 `DownloadOptions` 两个字段，所有既有
   `DownloadOptions` 结构体字面量（12 处）补上了 `global_limiter: None` /
   `host_limiter: None`，未改动任何既有断言或行为。
