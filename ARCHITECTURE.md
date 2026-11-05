# idm-rs 架构规范

本文件是项目最高级技术约束。任何阶段的实现若与本文件冲突，必须先停止并提出，不得为实现便利擅自偏离。

## 1. Workspace 物理隔离
- idm-core：纯 Rust 引擎。网络、Probe、分块调度、工作窃取、磁盘 I/O、SQLite 状态持久化全部在此。不得依赖任何 GUI、浏览器或 IPC crate。
- idm-cli：验证工具。Phase 1-3 通过它执行真实下载、续传与压力测试，只调用 idm-core 公开 API。
- idm-ipc：Phase 4 启用，负责本地通信与浏览器 Native Messaging。它依赖 idm-core，禁止反向依赖。
- idm-desktop：Phase 5 才创建（Tauri 2 + Vue 3），只作为 idm-core 的展示与控制层。
- 依赖方向：cli / ipc / desktop → core。core 不知道任何上层存在。

## 2. Windows 文件并发写入
- 禁止 Arc<Mutex<tokio::fs::File>> + seek + write 的串行方案。
- 使用 std::os::windows::fs::FileExt::seek_write，所有写入显式指定偏移量；多个 Chunk 共享 Arc<std::fs::File>。
- seek_write 会改变文件游标：同一个共享句柄上只允许使用 seek_write / seek_read 这类显式偏移操作，禁止混用依赖当前游标的 seek / read / write。需要顺序读写时，另开独立的文件句柄。
- seek_write 可能短写：必须按返回值推进偏移，循环写入直到缓冲区全部提交；写入 0 字节视为错误。
- 阻塞磁盘写入不得在 Tokio 工作线程上执行，使用 spawn_blocking 或专用落盘线程。
- 网络层到落盘层之间用有界通道传递缓冲区，形成背压，保证内存占用有上限。
- 不同 Chunk 的写入区间不得重叠。区间合法性由调度器保证，并在落盘层做断言校验。
- Windows 文件写入封装为 idm-core 内独立的磁盘 I/O 模块（cfg(windows)），网络层与调度器不得直接操作文件。
- 该模块如需 unsafe（调用 Win32 API），只允许在该模块内局部 allow，并在每一处写明 SAFETY 注释。
- Phase 1 必须编写真实的多线程并发写入测试：多个线程同时写入不同区间，完成后逐字节校验文件内容。

## 3. 文件空间分配
- 区分三个概念：逻辑文件长度（File Size）、物理分配大小（Allocation Size）、有效数据长度（Valid Data Length）。
- File::set_len 只设置逻辑长度。不得在未实测的情况下，宣称它已保证物理空间分配，或已避免后续的补零开销。
- 已知风险：NTFS 在超出有效数据长度的偏移处写入时会同步补零，大文件尾部 Chunk 首次写入可能卡顿。
- Phase 1 必须在 NTFS 上实测并比较以下方案：set_len、SetFileInformationByHandle(FileAllocationInfo)、稀疏文件。比较项包括：创建耗时、实际磁盘占用、尾部首次写入耗时、磁盘空间不足时的报错时机、异常退出后的文件状态。根据结果再定方案。
- 禁止使用需要特殊权限的 SetFileValidData。

### 决定（依据 Phase 1a 实测，详见 docs/phase1a-allocation-report.md）
- 目标卷支持稀疏文件时（GetVolumeInformationW 返回的标志含 FILE_SUPPORTS_SPARSE_FILES），默认使用 Sparse 策略。
- 不支持时退回 SetLen，并输出 warn 日志，说明首次在远端偏移写入时会触发补零。
- 依据：D 盘（机械硬盘，NTFS）16GB 文件，SetLen / Reserve 在尾部首次写入 1MB 的耗时中位数约 123s / 134s，Sparse 约 0.35ms。
- 创建前的空间预检查保留，对所有策略生效。
- 已知代价 1：稀疏文件按需分配空间，下载中途仍可能磁盘满。写入遇到 ERROR_DISK_FULL(112) 时，任务必须进入可恢复的暂停状态，不得判定失败、不得丢弃已下载数据（Phase 1b/1c 实现）。
- 已知代价 2：多个 Chunk 并发写入会使文件碎片化，机械硬盘上更明显，后续阶段评估影响。

## 4. 断点续传一致性（最重要）
- 写入顺序：数据写入 → 按策略刷盘（flush/sync_data）→ 再更新 SQLite 中的 Chunk 进度。
- 数据库中记录的已完成偏移量，任何时刻都不得超过已确认落盘的数据。宁可少记、重下一段，也不能多记。
- 续传前必须校验远端未变化（ETag / Last-Modified / Content-Length）；如有变化，拒绝续传并返回明确错误。
- 进程崩溃或断电后重启，必须能根据数据库恢复到一致状态，不能出现"进度显示完成、文件内容错误"的情况。

## 5. 统一运行时与依赖
- 异步：仅 Tokio。HTTP：仅 Reqwest（开启 stream，保持默认 TLS）。SQLite：仅 rusqlite（bundled）。错误：thiserror。日志：tracing。
- HTTP 客户端必须禁用自动解压，请求中显式发送 Accept-Encoding: identity，确保 Range 偏移与磁盘字节一一对应。
- rusqlite 是同步 API：由单独的数据库线程（或 spawn_blocking）持有连接，开启 WAL 模式；写操作串行化到该线程，禁止在 Tokio 工作线程上直接执行查询。
- 数据库 schema 用 PRAGMA user_version 管理版本，从第一个版本起就必须提供迁移路径。
- 核心库不得初始化全局 tracing subscriber，由可执行程序负责。
- 依赖升级必须单独提交并说明原因，禁止在功能开发中顺手执行 cargo update。

## 6. 错误与可观测性
- idm-core 对外提供强类型错误枚举（thiserror），至少覆盖：网络错误、HTTP 状态码错误、服务器不支持 Range、远端文件已变化、文件系统错误、磁盘写入错误（含短写）、磁盘空间不足、数据库错误、非法状态转换、Chunk 调度错误、断点恢复错误。
- 核心库禁止 println!/eprintln!，禁止对可恢复错误使用 unwrap/expect/panic。
- 关键日志使用 tracing span/字段，能关联：task_id、chunk_id、start_byte、current_offset、end_byte、状态转换（from → to）、错误上下文。线程 ID 由 subscriber 输出；Tokio task id 在稳定 API 可用时记录。

## 7. 并发与状态
- 下载任务与 Chunk 使用显式状态机（enum + 受控转换函数），禁止用多个布尔值组合表达状态。
- 禁止用全局 Mutex 锁住整个引擎。锁粒度限定在单个任务或调度器内部，持锁期间禁止 .await 和磁盘 I/O。
- 引擎通过事件通道对外报告进度。UI 刷新频率与引擎执行频率解耦，引擎不因 UI 慢而阻塞。
- 动态分块与工作窃取（Phase 2）的数据结构从 Phase 1 起就要预留：Chunk 区间可拆分，不得在类型设计里写死"固定 N 段"。

## 8. 测试与阶段门禁
- 每个核心功能必须有单元测试，重要并发逻辑必须有压力测试。
- 下载相关测试优先使用测试内启动的本地 HTTP 服务（可模拟 Range、断连、慢速、ETag 变化），不依赖公网。
- 每个阶段交付前必须通过 scripts/verify-workspace.ps1，经人工验收并完成 Git 提交后，才能进入下一阶段。

## 9. 断点续传检查点协议（Phase 1c 确立）

### 9.1 进度的三个层次
- received：从网络收到的字节位置（只用于显示进度）
- acked：落盘线程池已返回写入成功回执的位置（数据已交给操作系统，但可能还在系统缓存里）
- committed：已写入数据库的位置。它是续传的唯一依据。

### 9.2 检查点协议（每个任务独立执行，顺序不得改变）
1. 读取每个分块当前的 acked 位置，记为快照 S
2. 等待 pool.sync(target) 完成
3. sync 成功后，在一个数据库事务中把 S 写入各分块的 committed。只允许增大：committed = MAX(committed, S)
- 先取快照、再 sync：S 中的每个字节都在 sync 开始前收到了回执，sync 返回后必然已落盘。反过来先 sync 再取快照，会把 sync 之后才写入的数据也记成已落盘，这是错误的。
- sync 失败时，本次检查点放弃，不写数据库。如果失败原因是磁盘已满，任务暂停。
- 检查点触发时机：定时触发（默认每 2 秒）；以及任务暂停、取消、失败、完成之前，各触发一次最终检查点。

### 9.3 数据库可靠性取舍
- WAL 模式，synchronous = NORMAL，busy_timeout = 5000ms
- 理由：断电时，WAL 模式下最多丢失最后几个事务，数据库仍然完整一致。丢失的只是较新的 committed 记录，结果是 committed 比实际落盘的数据少，续传时重新下载这一小段即可，不会损坏文件。所以不需要 FULL 模式那样的写盘代价。

### 9.4 完成阶段的顺序
最终检查点 → 数据库状态改为 Finalizing → 释放所有分块和文件句柄 → 不覆盖地重命名 → 数据库记录最终文件名，状态改为 Completed。
程序启动时的恢复规则：
- 状态为 Downloading 的任务，改为 Paused（原因：Interrupted）
- 状态为 Finalizing 的任务：.idm-part 存在 → 保持 Finalizing，续传时直接完成重命名；.idm-part 不存在但最终文件存在且大小正确 → 改为 Completed；其他情况 → Failed，并记录原因

### 9.5 多进程所有权
- 一个任务的 .idm-part 同一时间只能被一个进程以可写方式打开：TargetFile 使用 FILE_SHARE_READ 共享模式。
- 启动恢复只把"无人占用"的 Downloading 任务改为 Paused(Interrupted)。.idm-part 被占用（错误码 32）说明另一个进程正在下载，跳过这个任务。
- resume 遇到被占用的 .idm-part 时返回 TaskInUse，数据库状态不变。
- 所有文件句柄都必须在改名之前释放；由于共享模式不允许删除，只要还有句柄没释放，改名就会失败。
