# 任务：idm-rs Phase 1b —— 前置修正 + Probe 与静态分块下载

沿用 Phase 1a 的全部工作规则（见 docs/prompts/phase1a.md）。开始前先把本任务原文原样保存为 `docs/prompts/phase1b.md`，`git status` 必须干净。
本阶段不写任何 SQLite、断点续传、重试、限速、下载队列代码（分别属于 Phase 1c 和 Phase 3）。

---

## A. 前置修正（先做完 A，单独提交，再开始 B）

**A1 chunk_id 由线程池分配（数据安全问题，必须修）**
- 现状：`worker` 中 `writers.insert(chunk_id, writer)` 遇到重复 id 会静默覆盖，导致一个文件的数据被写进另一个文件。
- 修正：线程池内部用 `AtomicU64` 生成全局唯一的 pool id。`register` 不再使用 ChunkWriter 自带的 chunk_id 做路由键；`PoolChunk` 保存 pool id，以及原 chunk_id 和路径（日志用）。路由和 HashMap 的键都改用 pool id。
- `Register` 请求增加 oneshot 回执，确认注册成功后才返回 `PoolChunk`。
- 新增测试：两个不同文件各自注册 chunk_id 为 0 到 7 的分块，并发写入，两个文件都逐字节校验正确。

**A2 sync 与 shutdown 不得阻塞异步线程**
- `sync` 改为 `async fn sync(&self, target: &TargetFile)`，内部用 `spawn_blocking` 执行 `sync_data`。语义保持不变。
- `shutdown` 中等待线程结束的 `join` 放进 `spawn_blocking`。任何线程 panic 都要作为错误返回，不得吞掉。

**A3 其他修正**
- `DiskWriterPool::new`：参数非法（线程数或队列容量为 0）时返回错误，不得 `assert!`。
- 新增错误变体 `ChunkNotRegistered { pool_id, chunk_id, path }`，替换 `unregistered()` 中借用 IO 错误的写法。
- 确认 `ChunkWriter::write` 只在整块写入成功后才推进写入位置；写入失败时位置保持不变（这样重新写入同一偏移是安全的）。补一个用模拟写入器验证的测试。
- 写盘时如果系统返回错误码 112（ERROR_DISK_FULL）或 39（ERROR_HANDLE_DISK_FULL），要映射为单独的错误变体 `DiskFullDuringWrite { path, offset }`，并给 `Error` 增加 `is_disk_full()` 方法（创建前预检查得到的 DiskFull 也返回 true）。补映射的单元测试。

**A4 稀疏文件支持检测**
- win32.rs：`GetVolumeInformationW` 同时取出 `lpFileSystemFlags`，新增 `volume_supports_sparse(path) -> Result<bool>`（检查 `FILE_SUPPORTS_SPARSE_FILES = 0x40`）。
- `AllocationStrategy` 新增 `Auto`：卷支持稀疏文件时用 Sparse，否则用 SetLen，并输出 warn 日志（与 ARCHITECTURE.md 第 3 节的决定一致）。
- 删除 `get_compressed_file_size` 里那段残缺的 SAFETY 注释。

**A5 验收脚本**
- 删除"unsafe 块上方必须有 SAFETY 注释"这项文本检查，这一项改由 clippy 的 `undocumented_unsafe_blocks` 负责。保留"unsafe 只能出现在 win32.rs"的检查。
- 反向测试：在 `crates/idm-core/src/disk/pool.rs` 末尾加一个含 unsafe 块的函数，确认脚本报 FAIL（报告是哪一步拦下的），然后恢复。

**A 部分提交**：验收全部 PASS 后，只暂存 `docs crates scripts`，提交信息 `phase1b-pre: pool id allocation, async sync, disk-full mapping, sparse detection`。

---

## B. Probe 与静态分块下载

### B1 模块结构

```
crates/idm-core/src/
├── net/
│   ├── mod.rs
│   ├── client.rs      # reqwest Client 构建
│   ├── headers.rs     # Content-Range / Content-Disposition 解析（纯函数）
│   ├── filename.rs    # 文件名推断与 Windows 合法化（纯函数）
│   └── probe.rs
└── task/
    ├── mod.rs
    ├── state.rs       # 任务与 Chunk 状态机（只在内存中，1c 再持久化）
    ├── progress.rs    # 进度快照
    └── download.rs    # 下载编排
```

不新增依赖。百分号解码手写；URL 用 `reqwest::Url`；读取响应体用 `Response::chunk().await` 循环（不引入 futures）。idm-cli 的 tokio 增加 `signal` 特性；idm-core 的 dev-dependencies 中 tokio 增加 `net` 特性（测试服务器要用）。

### B2 HTTP 客户端（client.rs）

- `DownloadConfig`：连接数（默认 8，范围 1 到 32）、最小分块大小（默认 1 MiB）、连接超时、读取超时、User-Agent、最大重定向次数（默认 10）
- 只允许 http 和 https 两种 scheme，其他一律拒绝
- 每个请求显式发送 `Accept-Encoding: identity`
- 响应带有 `identity` 以外的 `Content-Encoding` 时，返回错误 `UnsupportedContentEncoding`，绝不能把压缩后的字节当作文件内容写盘
- 代理设置留到 Phase 3，本阶段不做

### B3 Probe（probe.rs）

- 主方法：发送带 `Range: bytes=0-0` 的 GET 请求，拿到响应头后立即丢弃响应体，不下载正文
  - 返回 206 且 `Content-Range: bytes 0-0/总长` → 支持 Range，总长已知
  - 返回 206 但总长为 `*` → 支持 Range，但总长未知，按"不支持分块"处理
  - 返回 200 → 不支持 Range；有 Content-Length 就记录总长
- 不能只看 `Accept-Ranges` 响应头就判断是否支持 Range
- `ProbeResult` 包含：重定向后的最终 URL、总长（Option）、是否支持 Range、ETag（同时标注是否为弱 ETag `W/`）、Last-Modified、推断出的文件名、Content-Type
- 文件名推断顺序：`Content-Disposition` 中的 `filename*=UTF-8''...`（RFC 5987）> `filename=` > URL 路径最后一段（百分号解码后）> `download`
- Windows 文件名合法化：替换 `<>:"/\|?*` 和控制字符；处理保留名（CON、PRN、AUX、NUL、COM1 到 COM9、LPT1 到 LPT9，含带扩展名的形式）；去掉末尾的点和空格；总长度不超过 200 个字符，并保留扩展名
- 目标目录下已有同名文件时，按 `name (1).ext`、`name (2).ext` 的顺序取第一个不存在的名字

### B4 下载编排（download.rs）

**下载模式的选择**
- 总长已知且支持 Range → 静态分块。分块数取 `min(连接数, 向上取整(总长 / 最小分块大小))`，各分块大小尽量均匀
- 其他情况 → 单连接顺序下载。总长未知时无法预分配空间，用单独的顺序写入路径，同样不得在 Tokio 线程上做阻塞写入。这类任务标记为"不可续传"（供 1c 使用）

**分块请求**
- 每个分块发送 `Range: bytes=start-end`
- 有强 ETag 时加 `If-Range: <ETag>`；没有 ETag 但有 Last-Modified 时加 `If-Range: <Last-Modified>`；只有弱 ETag 时不加 If-Range，但要校验每个分块响应中的 ETag 与 Probe 时一致
- 响应必须是 206，且 Content-Range 与请求的区间完全一致。否则按以下规则报错：
  - 带 If-Range 却返回 200 → `RemoteChanged`
  - 不带 If-Range 却返回 200 → `RangeNotHonored`
  - 返回 206 但 Content-Range 不一致 → `InvalidContentRange`
- 收到的字节数必须正好等于分块长度。提前结束返回 `UnexpectedEof { chunk_id, expected, received }`；多出来的数据也要报错，不得写入
- 数据通过 `DiskWriterPool` 写入，从网络收到的 `Bytes` 直接交给线程池，不做拷贝

**并发与取消**
- 每个分块一个 tokio 任务，用 `JoinSet` 管理。任何一个分块失败时，取消其余所有分块（`abort_all`），整个任务以第一个错误作为失败原因
- 支持外部取消（例如 CLI 收到 Ctrl+C）：停止所有分块，等已提交的写入完成后执行 sync，结果为 `Cancelled`

**完成与文件命名**
- 下载过程中，文件名为 `<最终文件名>.idm-part`
- 所有分块完成后，依次执行：release 每个分块，确认写入位置都等于各自的区间终点 → sync → 释放所有文件句柄 → 重命名为最终文件名
- 失败、取消或磁盘满时，保留 `.idm-part` 文件，不自动删除（1c 续传要用）
- 写盘遇到 `is_disk_full()` 的错误时，任务结果为 `Paused { reason: DiskFull }`，与 `Failed` 区分开

**状态机（state.rs）**
- 任务状态：Pending → Probing → Downloading → Finalizing → Completed；以及 Failed、Cancelled、Paused。
- 分块状态：Pending → Downloading → Done / Failed。
- 所有状态转换都经过受控的转换函数，非法转换返回 `InvalidStateTransition { from, to }` 错误。补齐合法与非法转换的单元测试。

**进度（progress.rs）**
- 用 `tokio::sync::watch` 发布 `ProgressSnapshot`：总长、已下载字节数、每个分块的（区间起点、当前位置、区间终点、状态）、最近一段时间的平均速度
- 发布频率不超过每 200ms 一次。没有人读取进度时，下载照常进行，不得阻塞

**日志**：分块开始、结束、失败时用 info 级别，字段包括 task_id、chunk_id、start、end、received；每次收到数据只用 trace 级别。

### B5 CLI

```
idm-cli get <URL> [--dir <目录>] [--connections <N>] [--out <文件名>]
```

- 先执行 Probe 并打印结果，再开始下载
- 下载过程中，每秒刷新一行进度：已下载 / 总长、百分比、速度、活动分块数
- 结束时打印：结果状态、最终路径、大小、耗时、平均速度、分块数、实际使用的分配策略
- 按 Ctrl+C 取消下载，保留 `.idm-part` 文件，退出码非 0
- 不带参数运行时，保持 Phase 0 的行为；`bench-alloc` 保持不变

### B6 测试

**本地测试服务器**：在 `crates/idm-core/tests/support/` 中，用 `tokio::net::TcpListener` 手写一个最小的 HTTP/1.1 服务器（不引入 hyper 等任何依赖）。服务器返回的内容按偏移量确定性生成（每个字节的值由它的偏移量计算得出），所以可以模拟 GB 级文件而不占内存。服务器需要支持通过配置模拟以下情况：

- 正常支持 Range（206）、完全不支持 Range（总是返回 200）
- 总长未知（不发送 Content-Length，直接关闭连接表示结束）
- 对 Range 请求返回 200；返回与请求不一致的 Content-Range
- 发送到第 N 个字节时断开连接
- 慢速发送
- 中途更换 ETag（之后的请求按 If-Range 返回 200）
- 带 `Content-Encoding: gzip` 的响应头
- 302 重定向
- 带 Content-Disposition（包括 `filename*=UTF-8''` 编码的中文文件名）

**必须覆盖的测试**
1. headers.rs、filename.rs 的纯函数单元测试：各种 Content-Range 写法、Content-Disposition 写法、保留名、非法字符、超长文件名、同名文件编号
2. Probe：上面每种服务器行为下的识别结果都正确
3. 下载：8 个分块逐字节正确；小于最小分块大小的文件只用 1 个分块；不支持 Range 时走单连接；总长未知时走顺序写入
4. 异常：对 Range 请求返回 200、Content-Range 不一致、中途断开、ETag 变化、gzip，每种情况都返回对应的错误类型，保留 `.idm-part` 文件，程序不 panic，其余分块被取消
5. 完成后 `.idm-part` 被重命名为最终文件名（重命名成功也就证明了文件句柄已全部释放）
6. 取消：取消后得到 `Cancelled`，`.idm-part` 保留
7. 进度：最终快照中已下载字节数等于总长；全程不读取进度时，下载也能正常完成
8. **20 个下载任务同时进行、共用一个线程池**，每个任务 8 个分块，全部逐字节正确（这个测试专门验证 A1 的修复）
9. `#[ignore]` 压力测试：本地服务器提供 1GB 文件，16 个连接下载，逐字节校验。用 `cargo test --release -p idm-core -- --ignored` 运行，记录耗时

### B7 验收与提交

1. 验收脚本全部 PASS，exit code 0
2. 运行 `--ignored` 压力测试
3. 只暂存 `docs crates scripts`，提交信息 `phase1b: probe and static chunked download`，不得 push
4. 交付报告：
   - 两次提交的 hash
   - 新增的公开 API（签名，每项附一句话说明）
   - 新增的错误变体清单
   - 状态机的状态转换表
   - 测试数量与结果、压力测试耗时
   - 验收脚本输出
   - 与本文不一致之处（没有就写"无"）
   - 发现的风险或疑问

**本阶段结束后，由我本人在真实网络上做 CLI 验收，不需要你做。**
