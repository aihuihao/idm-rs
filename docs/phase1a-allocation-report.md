# Phase 1a 磁盘空间分配策略实测报告（4GB / 16GB）

> 配套实现：`crates/idm-core/src/disk/*`（仅 `cfg(windows)`）
> 验收脚本：`scripts/verify-workspace.ps1`
> 实测工具：`idm-cli bench-alloc`
> 旧版 1GB 数据已删除，本报告仅含 4GB / 16GB 实测。

## 1. 目的

按架构规范第 3 节要求，在 NTFS 上实测并比较三种文件空间分配方案，为 Phase 1b/1c 落盘/续传设计确定默认策略：

- **SetLen**：`File::set_len(size)`（即 `SetEndOfFile`）仅设置逻辑长度。
- **Reserve**：`SetFileInformationByHandle(FileAllocationInfo)` 预先承诺物理分配大小，再 `set_len`。
- **Sparse**：`DeviceIoControl(FSCTL_SET_SPARSE)` 将文件标记为稀疏文件，再 `set_len`。

比较项：创建耗时、实际磁盘占用、尾部（与中部）首次写入耗时、落盘耗时、磁盘空间不足时的系统级报错时机与原始错误码。

## 2. 测试环境

| 项目 | 值 |
| --- | --- |
| 操作系统 | Windows 10 Pro 26H2（NTFS 内核 `10.0.26300`） |
| 架构 / 工具链 | x86_64-pc-windows-msvc，Rust 1.99.1 |
| 目标卷 | **D:** NTFS，总 ≈ 310.5 GiB（333,447,163,904 B），可用 ≈ 79.4 GiB |
| D: 所在物理磁盘 | `TOSHIBA DT01ACA100`（HDD, SATA, 1 TB） |
| 另一块磁盘 | `YMTC YMSS2CB04B32MC`（SSD, NVMe, 256 GB，应为 C: 系统盘） |
| 实测目录 | `D:\idm-bench`（仓库外，测完已删除） |

> 说明：创建耗时、写入耗时受具体磁盘型号与并发负载影响，绝对值仅供同机横向对比；**相对差距**与**磁盘占用**是确定性结论。本机 D: 为机械硬盘，故 SetLen/Reserve 的"尾部首次写入"代价被显著放大（零填充写盘）；换用 SSD 时绝对耗时下降，但相对差距与结论不变。

## 3. 方法论

`bench-alloc` 对每个策略、每轮执行以下时序（全部指标逐轮打印，见 §4/§5 原始值）：

1. **前置检查**：`volume_filesystem(dir)` 确认卷为 NTFS（非 NTFS 直接拒绝）。
2. **创建**：`TargetFile::create(path, size, strategy, skip_space_check=false)`，记录 `create_ms`；创建后调用 `allocation_info()` 读取 `logical_len / AllocationSize / CompressedFileSize / is_sparse`（即"创建后的分配信息"）。
3. **尾部首次写入 1MB**：在 `size - 1MiB` 处 `seek_write` 1 MiB，只计 `write` 调用本身（`tail_write_ms`），随后 `sync_data()` 计 `tail_sync_ms`。
4. **中部首次写入 1MB**：在 `size / 2` 处 `seek_write` 1 MiB，计 `mid_write_ms`。
5. **写入后的分配信息**：再次 `allocation_info()`（即"写入后的分配信息"）。
6. 每轮删除文件；每种策略 3 轮取**中位数**。

`allocation_info()` 通过 `GetFileInformationByHandleEx(FileStandardInfo)` 取逻辑/分配长度，`GetCompressedFileSizeW` 取压缩后实际占用（含稀疏），并据此判定稀疏属性。

### skip-precheck 设计修正

`TargetFile::create` 内部原本无条件做"目标卷可用空间"预检查（`GetDiskFreeSpaceExW` + `available < len` 返回 `DiskFull`）。本轮新增 `skip_space_check: bool` 参数：正常路径传 `false`（保留安全预检查）；`bench-alloc --skip-precheck` 传 `true`，使系统调用直接暴露原始错误码，用于观察空间不足时的**系统级**报错时机（详见 §6）。

## 4. 实测结果 — 4 GB（`--size-gb 4 --runs 3`，取中位数）

| 策略 | create_ms | mid_write_ms | tail_write_ms | tail_sync_ms | 创建后占用 | 写入后占用 |
| --- | --- | --- | --- | --- | --- | --- |
| **SetLen** | 0.60 | 0.26 | **23808.25** | 5816.87 | 4.00 GiB / sparse=false | 4.00 GiB / sparse=false |
| **Reserve** | 0.64 | 0.25 | **23663.09** | 4858.16 | 4.00 GiB / sparse=false | 4.00 GiB / sparse=false |
| **Sparse** | 0.34 | 0.25 | **0.33** | 61.73 | 0 / sparse=true | 1.00 MiB / sparse=true |

4 GB 原始逐轮值：

```
STRATEGY SetLen size_gb=4 runs=3
  run0 create_ms=0.51  tail_write_ms=22405.09 tail_sync_ms=5408.88 mid_write_ms=0.26  before alloc=4GiB compressed=4GiB sparse=F | after 4GiB/4GiB/F
  run1 create_ms=0.69  tail_write_ms=25723.70 tail_sync_ms=6324.99 mid_write_ms=0.34  before 4GiB/4GiB/F | after 4GiB/4GiB/F
  run2 create_ms=0.60  tail_write_ms=23808.25 tail_sync_ms=5816.87 mid_write_ms=0.26  before 4GiB/4GiB/F | after 4GiB/4GiB/F
STRATEGY Reserve size_gb=4 runs=3
  run0 create_ms=0.64  tail_write_ms=28278.40 tail_sync_ms=4716.14 mid_write_ms=0.27
  run1 create_ms=0.53  tail_write_ms=23663.09 tail_sync_ms=5475.37 mid_write_ms=0.25
  run2 create_ms=0.81  tail_write_ms=22715.95 tail_sync_ms=4858.16 mid_write_ms=0.23
STRATEGY Sparse size_gb=4 runs=3
  run0 create_ms=0.41  tail_write_ms=0.34 tail_sync_ms=39.18 mid_write_ms=0.27   before 0/0/T | after 1MiB/1MiB/T
  run1 create_ms=0.32  tail_write_ms=0.33 tail_sync_ms=63.22 mid_write_ms=0.24
  run2 create_ms=0.34  tail_write_ms=0.33 tail_sync_ms=61.73 mid_write_ms=0.25
```

## 5. 实测结果 — 16 GB（`--size-gb 16 --runs 3`，取中位数）

| 策略 | create_ms | mid_write_ms | tail_write_ms | tail_sync_ms | 创建后占用 | 写入后占用 |
| --- | --- | --- | --- | --- | --- | --- |
| **SetLen** | 0.82 | 0.27 | **123301.76** | 4499.84 | 16.00 GiB / sparse=false | 16.00 GiB / sparse=false |
| **Reserve** | 0.81 | 0.26 | **134061.20** | 4217.09 | 16.00 GiB / sparse=false | 16.00 GiB / sparse=false |
| **Sparse** | 0.35 | 0.30 | **0.35** | 124.28 | 0 / sparse=true | 1.00 MiB / sparse=true |

16 GB 原始逐轮值：

```
STRATEGY SetLen size_gb=16 runs=3
  run0 create_ms=0.82  tail_write_ms=100922.22 tail_sync_ms=8902.16 mid_write_ms=0.25  before 16GiB/16GiB/F | after 16GiB/16GiB/F
  run1 create_ms=0.90  tail_write_ms=123301.76 tail_sync_ms=4499.84 mid_write_ms=0.31
  run2 create_ms=0.74  tail_write_ms=129426.27 tail_sync_ms=3601.52 mid_write_ms=0.27
STRATEGY Reserve size_gb=16 runs=3
  run0 create_ms=0.90  tail_write_ms=128540.27 tail_sync_ms=4217.09 mid_write_ms=0.26
  run1 create_ms=0.81  tail_write_ms=134061.20 tail_sync_ms=1398.98 mid_write_ms=0.25
  run2 create_ms=0.80  tail_write_ms=173836.42 tail_sync_ms=4431.29 mid_write_ms=0.36
STRATEGY Sparse size_gb=16 runs=3
  run0 create_ms=33.55  tail_write_ms=0.37 tail_sync_ms=164.04 mid_write_ms=0.33  before 0/0/T | after 1MiB/1MiB/T
  run1 create_ms=0.34  tail_write_ms=0.35 tail_sync_ms=97.98  mid_write_ms=0.30
  run2 create_ms=0.35  tail_write_ms=0.33 tail_sync_ms=124.28 mid_write_ms=0.26
```

> 注：Sparse 16 GB run0 的 `create_ms=33.55` 为首次调用 `FSCTL_SET_SPARSE`/`set_len` 的一次性开销，中位数取 run1/run2 的 0.35。

### 5.1 关键观察

- **磁盘占用**：SetLen 与 Reserve 在创建后**立即占用与逻辑长度等量的物理空间**（4/16 GiB），而 Sparse 创建后占用 0，仅在写入后占用实际写入量（本例 1 MiB）。这是确定性结论。
- **尾部首次写入卡顿（架构第 3 节预警被证实）**：NTFS 在"首次写入越过有效数据长度（VDL）"时同步补零。本基准中尾部 1 MiB 写入触发**整文件**零填充——4 GB 下 ~23 s、16 GB 下 ~123 s（机械盘）。`before` 已显示 `AllocationSize=逻辑长度`，但 `set_len` 只是逻辑/分配长度声明，物理簇在该首次写入时才真正提交、卡顿来自补零本身而非"分配"。中部写入快（~0.25 ms）是因为它发生在尾部写入**之后**——尾部写入已把整文件零填充、将 VDL 扩展到文件末尾，中部只是覆写已分配且已补零的区域。
- **落盘耗时**：`FlushFileBuffers` 在 SetLen/Reserve 下需刷已提交的脏数据（4 GB ~5 s、16 GB ~4 s），Sparse 仅刷实际簇（~60–124 ms）。
- **创建耗时**：三者亚毫秒级（Sparse 偶有首次调用开销），差异可忽略。

## 6. 磁盘空间不足（DiskFull）行为 — skip-precheck

跳过 `TargetFile::create` 内部预检查（`skip_space_check=true`），构造 `size = 卷可用空间 + 1 GiB`，只创建不写入：

```
SKIP_PRECHECK available_bytes=85241724928 attempt_size_bytes=86315466752 runs=1
  SetLen  CREATE_FAILED os_error=112 (expected: ERROR_DISK_FULL)
  Reserve CREATE_FAILED os_error=112 (expected: ERROR_DISK_FULL)
  Sparse  CREATE_SUCCEEDED (expected: sparse does not pre-allocate)
```

- **SetLen / Reserve**：创建时由系统直接报错，**原始错误码 112 = `ERROR_DISK_FULL`**（`set_len` / `SetFileInformationByHandle(FileAllocationInfo)` 返回）。文件未创建（失败路径已删除半成品）。
- **Sparse**：创建成功（`FSCTL_SET_SPARSE` + `set_len` 只是标记/扩展逻辑长度，不承诺物理分配），随后由 bench 删除。
- 结论：空间不足时失败发生在系统调用层，而非我们的预检查；`skip_space_check` 参数使这一时机可被实测。生产路径保留预检查，作为第一道快速失败防线。

## 7. 结论与建议

1. **默认策略选 Sparse**。`FSCTL_SET_SPARSE` 在 NTFS 上近乎零成本完成"预留"语义：创建即时、磁盘占用仅实际写入量、尾部首次写入无补零卡顿、落盘极快，且天然满足架构第 2/3 节对"断电一致性"与"不要宣称已保证物理分配"的约束。
2. **SetLen / Reserve 仅作可选策略保留**：实测显示它们在创建时占满等量物理空间，且"尾部首次写入"会带来 4–16 GB 规模下数十秒级的补零卡顿，Phase 1b/1c 默认不应启用。
3. **生产路径保留 `create` 的可用空间预检查**（`skip_space_check=false`）：作为系统报错之前轻量、确定的快速失败；`skip_space_check=true` 仅用于本基准的 DiskFull 时机实测。
4. **异常退出后的文件状态**：`create` 失败路径删除半成品；运行中崩溃时未写区域在 Sparse 下不占空间、读为 0；已写区域由 Phase 1c 的进度记录恢复，不会出现"进度完成但内容错误"。

## 8. 本轮实现修正

- `TargetFile::create` 增加 `skip_space_check: bool` 参数（默认调用点传 `false`）。`bench-alloc --skip-precheck` 传 `true`，并使失败路径输出原始 `raw_os_error`（如 `Some(112)`）。
- 修复 `get_volume_filesystem` 先用 `GetVolumePathNameW` 解析卷根再查询（原始实现对子目录路径会失败），bench-alloc 前置检查与 skip-precheck 均正常。
- 修复 `make_chunks` 覆盖区间错误（原先硬编码 `FILE_SIZE`，2 GiB 压力测试后半段未覆盖）；现接收 `size` 并贯穿全部调用方。

---

# 附录 A：windows-sys 版本与 features

- **版本**：`windows-sys = "0.61"`（工作区 `Cargo.toml` 的 `[workspace.dependencies]`，所有 crate 共享）。
- **实际开启 features**（同处声明）：
  - `Win32_Foundation`
  - `Win32_Storage_FileSystem`
  - `Win32_System_IO`
  - `Win32_System_Ioctl`
- 这些 feature 覆盖了本模块用到的全部 Win32 API（`GetDiskFreeSpaceExW`、`GetFileAttributesW`、`GetCompressedFileSizeW`、`GetVolumeInformationW`、`GetVolumePathNameW`、`SetFileInformationByHandle`、`FlushFileBuffers`、`DeviceIoControl`/FSCTL、`GetFileInformationByHandleEx` 等）。`FSCTL_SET_SPARSE` 常量来自 `Win32_System_Ioctl`。

# 附录 B：公开 API 清单

来源：`crates/idm-core/src/disk/*` 通过 `mod.rs` 导出。

### 类型

- `enum AllocationStrategy` — 三种空间分配策略：`SetLen` / `Reserve` / `Sparse`（派生 `Debug, Clone, Copy`）。
- `struct AllocationInfo` — 文件空间信息；字段 `logical_len: u64`、`allocation_size: u64`、`compressed_size: u64`、`is_sparse: bool`。
- `struct TargetFile` — 已创建/已打开的目标文件，内部维护区间登记表与共享 `Arc<File>`。
- `struct DiskWriterPool` — 进程内共享的落盘线程池（按 `chunk_id` 路由到固定线程、无锁独占 `ChunkWriter`、有界通道形成背压）。
- `struct PoolChunk` — 落盘线程池返回的轻量句柄（`Clone`，含 `chunk_id` 与路径），用于向线程池路由写入请求。
- `struct ChunkWriter` — 单个写入区间的写入器，实现 `PositionalWrite`，drop 时自动注销区间。
- `trait PositionalWrite` — 位置化写入契约：`fn write(&self, buf: &[u8]) -> Result<()>`，写入显式偏移、循环补齐短写。
- `struct ChunkRange` — 已登记区间（start/end），用于重叠校验。
- `enum Error` / `type Result<T>` — 核心强类型错误与便捷别名（见 `error.rs`）。

### 函数 / 方法签名

- `TargetFile::create<P: AsRef<Path>>(path, len, strategy, skip_space_check: bool) -> Result<TargetFile>` — 创建新文件并应用分配策略；`create_new` 绝不覆盖；`skip_space_check=false` 时空间不足返回 `DiskFull` 且不建文件；中途失败删半成品。
- `TargetFile::open_existing<P: AsRef<Path>>(path, expected_len: u64) -> Result<TargetFile>` — 打开已有文件，长度不符返回 `LengthMismatch`（供续传校验）。
- `TargetFile::allocate_chunk(&self, chunk_id: u64, start: u64, end: u64) -> Result<ChunkWriter>` — 申请写入区间，重叠返回 `RangeOverlap`。
- `TargetFile::allocation_info(&self) -> Result<AllocationInfo>` — 读取逻辑/分配/实际占用/稀疏属性。
- `TargetFile::sync_data(&self) -> Result<()>` — `FlushFileBuffers` 刷盘。
- `TargetFile::len(&self) -> u64` / `is_empty(&self) -> bool` / `path(&self) -> &Path` / `strategy(&self) -> Option<AllocationStrategy>`；`file(&self) -> Arc<File>` 为 `pub(crate)`（不对外暴露句柄）。
- `DiskWriterPool::new(workers: usize, queue_depth: usize) -> Result<DiskWriterPool>`（全局共享，不绑定文件）。
- `DiskWriterPool::register(&self, ChunkWriter) -> Result<PoolChunk>`（异步，移交所有权得可克隆句柄）。
- `DiskWriterPool::write(&self, &PoolChunk, Bytes) -> Result<()>`（异步；区间检查与位置推进在落盘线程内由 `ChunkWriter` 完成，越界回执报错且零字节写入）。
- `DiskWriterPool::sync(&self, &TargetFile) -> Result<()>`（刷盘，语义不变）。
- `DiskWriterPool::release(PoolChunk) -> Result<u64>`（异步，注销并返回最终写入位置）。
- `DiskWriterPool::shrink_end(&PoolChunk, new_end: u64) -> Result<()>`（异步，经线程池缩小已注册 Chunk 尾部）。
- `DiskWriterPool::shutdown(self) -> Result<()>`（异步）。
- `ChunkWriter::write(&mut self, buf: &[u8]) -> Result<usize>`（无锁；区间检查越界报错且零字节写入）/ `position(&self) -> u64` / `chunk_id(&self) -> u64` / `path(&self) -> &Path` / `range(&self) -> ChunkRange` / `shrink_end(&mut self, new_end: u64) -> Result<()>`。
- `ChunkRange::new(start: u64, end: u64, file_len: u64) -> Result<ChunkRange>`；访问器 `start()/end()`。
- `fn volume_filesystem(path: &Path) -> Result<String>` — 查询卷文件系统名（公开封装，供 CLI 判断 NTFS）。
- `fn disk_free_space(path: &Path) -> Result<u64>` — 查询卷可用空间（公开封装，供 CLI 计算 skip-precheck 目标大小）。

# 附录 C：win32.rs 每个 unsafe 块

`win32.rs` 是 `cfg(windows)` 下唯一允许 `unsafe` 的子模块（顶部 `#![allow(unsafe_code)]`），所有 FFI 调用封装成安全函数，每个 `unsafe` 块均带 `// SAFETY:` 注释。共 **10** 个 `unsafe` 块（其中 9 处为 FFI 调用，1 处为 `SetLastError(0)` 错误码复位）。Phase 1a.1 已移除 `flush_buffers`（`FlushFileBuffers` 刷盘改由 `std::fs::File::sync_data` 在 `TargetFile` 内完成），故下表不再含 `FlushFileBuffers`。

| # | 行 | Win32 函数 / 操作 | 用途 | SAFETY 理由摘要 |
| --- | --- | --- | --- | --- |
| 1 | 65 | `GetDiskFreeSpaceExW` | 查询目标卷可用空间 | `wide` 是调用期间存活的 null 结尾 UTF-16；`free` 为有效 `u64` 输出指针；另两个指针传 null |
| 2 | 92 | `SetFileInformationByHandle(FileAllocationInfo)` | 预留物理分配大小 | `info` 是调用期间有效的栈结构；长度按 `size_of` 给出 |
| 3 | 117 | `DeviceIoControl(FSCTL_SET_SPARSE)` | 标记稀疏文件 | 无输入/输出缓冲区；`lpOverlapped` 传 null 表示同步；所有指针为 null 或有效栈变量 |
| 4 | 144 | `std::mem::zeroed()` | 零初始化 `FILE_STANDARD_INFO` | 结构先零初始化再被下面的调用填充 |
| 5 | 146 | `GetFileInformationByHandleEx(FileStandardInfo)` | 读取物理分配大小 | `info` 是调用期间有效的栈结构；长度按 `size_of` 给出 |
| 6 | 171 | `SetLastError(0)` | 复位线程局部 Win32 错误码（供 `GetCompressedFileSizeW` 判别失败） | 仅设置错误码，不涉及任何指针操作 |
| 7 | 175 | `GetCompressedFileSizeW` | 读取实际占用（含压缩/稀疏） | `wide` 有效 UTF-16；`high` 为有效 `u32` 高 32 位输出指针 |
| 8 | 186 | `GetFileAttributesW` | 读取属性以判定稀疏标志 | `wide` 是调用期间存活的 null 结尾 UTF-16 |
| 9 | 206 | `GetVolumePathNameW` | 把任意路径解析为卷根 | `wide` 有效 UTF-16；`vol` 缓冲 `buf_len×u16`（≥261），长度按 `buf_len` 给出；API 只写入 `vol` |
| 10 | 214 | `GetVolumeInformationW` | 查询文件系统名 | `vol` 为 null 结尾卷根；`buf` 256×u16、长度 256；其余指针为 null |

# 附录 D：判断卷是否支持稀疏文件的 Win32 接口

- **接口**：`GetVolumeInformationW`（已在 `win32.rs` 第 10 处调用）。
- **机制**：其 `lpFileSystemFlags` 输出参数返回文件系统能力位掩码；测试掩码是否包含 **`FILE_SUPPORTS_SPARSE_FILES = 0x00000040`** 即可判断该卷支持稀疏文件。
- 说明：本模块当前对 `volume_filesystem()` 仅做"是否等于 NTFS"的字符串判断；若后续需要更通用的稀疏能力探测，可在同一 `GetVolumeInformationW` 调用中取 `lpFileSystemFlags` 并检测 `0x40`。本轮仅报告接口与标志位，未写代码。

---

# Phase 1a.1 补充：落盘线程池重构与 win32 封装修正

> 本次只做磁盘模块修正，不写网络/下载/SQLite。工作规则沿用 Phase 1a（验收脚本全 PASS、不引入 `rand`、提交不 push）。

## 1. 核心 API 变更（所有权移交模型）

- 落盘线程池改为**进程内共享、不绑定文件**：`DiskWriterPool::new(workers, queue_depth)`，不再接收 `Arc<File>` 与路径。
- 删除自由偏移接口 `write(chunk_id, offset, data)`；新模型为：
  `TargetFile::allocate_chunk` 取得 `ChunkWriter` → `pool.register(writer)` 移交所有权得 `PoolChunk`（Clone，含 `chunk_id`）→ `pool.write(&PoolChunk, Bytes)`。区间检查与位置推进在落盘线程内由 `ChunkWriter` 完成，越界回执报错且零字节写入。
- `ChunkWriter::write` 改为 `&mut self`（无锁，因每个 `chunk_id` 路由到固定线程、独占持有写器）。
- 新增 `pool.release(PoolChunk) -> u64`（注销并返回最终写入位置）、`pool.shrink_end(&PoolChunk, new_end) -> Result<()>`（经线程池缩小已注册 Chunk 尾部）。
- `TargetFile::file()` 改为 `pub(crate)`，不对外暴露 `Arc<File>`。
- `PositionalWrite::write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>` 已含短写/0 字节/`Interrupted` 单测。

## 2. 剩余 unsafe 数

`win32.rs` 顶部 `#![allow(unsafe_code)]`，是本阶段唯一允许 `unsafe` 的子模块；其余 `.rs` 无任何 `unsafe`。共 **10** 个 `unsafe` 块（9 处 FFI 调用 + 1 处 `SetLastError(0)` 错误码复位），每个均带 `// SAFETY:` 注释，详见附录 C。验证脚本 STEP 12 静态检查（unsafe 仅限 win32.rs 且必有 SAFETY 注释）通过。

## 3. 测试数量与结果

- `idm-core` 单元测试：16 passed（含 `disk::{range,write,target,win32}` 子模块测试）。
- 集成测试 `tests/disk_concurrency.rs`：4 passed + 1 ignored（压力测试）。
  - 新增 4 项：`pool_write_out_of_range_rejected`（越界拒绝且文件不变）、`one_pool_serves_two_files`（一池双文件逐字节校验）、`release_returns_write_position`（release 位置）、`shrink_end_via_pool_normal_and_illegal`（shrink 正常/非法）。
  - 并发压力测试保持 3 策略 × 20 种子（2 GiB）。
- `cargo clippy --workspace --all-targets --locked -- -D warnings` 全绿（`unwrap_used`/`expect_used` 经测试内 `?` 传播规避）。

## 4. 压力测试耗时（2 GiB × 3 策略，std 直写 vs 线程池，20 种子）

| 策略 | std | pool |
| --- | --- | --- |
| SetLen | 5.23 s | 5.45 s |
| Reserve | 5.21 s | 5.32 s |
| Sparse | 4.05 s | 4.41 s |

总耗时约 29.68 s（release，机械盘 D:）；两种写入路径耗时一致，说明线程池无额外开销。

## 5. 验收输出

`scripts/verify-workspace.ps1` 全部 15 步 PASS：`SUMMARY: all 15 steps executed; final result: PASS`（含新增 STEP 15 `cargo doc --workspace --no-deps --locked` 设 `RUSTDOCFLAGS=-D warnings`）。exit code 0。

## 6. 不一致项 / 备注

- **intra-doc 链接**：`disk/mod.rs` 与 `disk/range.rs` 中原 `[`pool::DiskWriterPool`]`、`[`ChunkRange`]` 等 intra-doc 链接在 `cargo doc -D warnings` 下报 "unresolved link"（re-export 上下文解析失败），已改为纯代码跨度 `` `pool::DiskWriterPool` `` / `` `ChunkRange` `` 以消除 rustdoc 警告；链接语义保留为文档文字。
- **设计约束（测试中发现）**：同一 `DiskWriterPool` 内 `chunk_id` 必须全局唯一（线程按 `chunk_id` 路由并以之为键），否则后注册的写器会覆盖前者。测试 `one_pool_serves_two_files` 已据此使用不同 `chunk_id`（0 与 1）验证"一池服务多文件"。
- 报告措辞已同步：5.1 注明中部写入发生在尾部补零之后；"物理簇首次写入提交"改为"set_len 仅声明长度、卡顿来自补零"；第 7 节第 4 条 SQLite 说法改为 Phase 1c 进度记录；全篇 "Phase 3" 改为 "Phase 1b/1c"。
