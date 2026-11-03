# Phase 1a 磁盘空间分配策略实测报告

> 配套实现：`crates/idm-core/src/disk/*`（仅 `cfg(windows)`）
> 验收脚本：`scripts/verify-workspace.ps1`
> 实测工具：`idm-cli bench-alloc`

## 1. 目的

按架构规范第 3 节要求，在 NTFS 上实测并比较三种文件空间分配方案，为 Phase 3 落盘/续传设计确定默认策略：

- **SetLen**：`File::set_len(size)`（即 `SetEndOfFile`）仅设置逻辑长度。
- **Reserve**：`SetFileInformationByHandle(FileAllocationInfo)` 预先承诺物理分配大小。
- **Sparse**：`DeviceIoControl(FSCTL_SET_SPARSE)` 将文件标记为稀疏文件。

比较项（来自架构第 3 节）：创建耗时、实际磁盘占用、尾部首次写入耗时、磁盘空间不足时的报错时机、异常退出后的文件状态。

## 2. 测试环境

| 项目 | 值 |
| --- | --- |
| 操作系统 | Windows (x86_64-pc-windows-msvc) |
| Rust | 1.99.1 (2026-10-13) |
| 文件系统 | NTFS |
| 卷可用空间（实测时） | ≈ 79.4 GiB（见 §5） |
| 工具命令 | `cargo run -p idm-cli -- bench-alloc --dir <DIR> --size-gb 1 --runs 3` |

> 说明：创建耗时、写入耗时受具体磁盘型号与并发负载影响，绝对值仅供同机横向对比；**相对差距**与**磁盘占用**是确定性结论。

## 3. 方法论

`bench-alloc` 对每个策略执行：

1. **前置检查**：`volume_filesystem(dir)` 确认卷为 NTFS（非 NTFS 直接拒绝，避免无意义的跨平台比较）。
2. **创建**：`TargetFile::create(path, size, strategy)`，记录 `create_ms`。创建前/后各调用一次 `allocation_info()` 读取 `logical_len / AllocationSize / CompressedFileSize / is_sparse`。
3. **尾部写入**：向文件**最后一块**写入确定性数据，记录 `tail_write_ms`（反映“大文件尾部 Chunk 首次写入”在架构第 3 节中指出的潜在补零卡顿）。
4. **中部写入**：向文件**中部某块**写入确定性数据，记录 `mid_write_ms`（基准参考）。
5. **落盘**：`sync_data()`，记录 `tail_sync_ms`（FlushFileBuffers 刷盘耗时）。
6. `size_gb=1`、`runs=3`，取 3 次实测值。所有分配大小均通过 `allocation_info()` 实测读取，非推断。

`allocation_info()` 通过 `GetFileInformationByHandleEx(FileStandardInfo)` 取逻辑长度，`GetCompressedFileSizeW` 取压缩后实际占用（含稀疏），并以此判定稀疏属性。

## 4. 实测结果（1 GiB 文件，3 次平均）

| 策略 | create_ms | mid_write_ms | tail_write_ms | tail_sync_ms | 实际磁盘占用（after） | 稀疏 |
| --- | --- | --- | --- | --- | --- | --- |
| **SetLen** | ~0.59 | ~0.26 | **~5400** | **~1342** | 1073741824 B（1.00 GiB） | false |
| **Reserve** | ~4.44 | ~0.24 | **~5461** | **~1356** | 1073741824 B（1.00 GiB） | false |
| **Sparse** | ~0.84 | ~0.27 | **~0.47** | **~62** | 1048576 B（1.00 MiB） | true |

各策略 3 次原始数据（节选关键指标）：

- **SetLen**：tail_write 5517 / 5360 / 5336 ms；tail_sync 1313 / 1440 / 1272 ms；after `alloc=1073741824 compressed=1073741824 sparse=false`。
- **Reserve**：tail_write 5227 / 5685 / 5472 ms；tail_sync 1325 / 1473 / 1270 ms；after 同上。
- **Sparse**：tail_write 0.49 / 0.40 / 0.52 ms；tail_sync 42 / 40 / 106 ms；before `alloc=0 compressed=0 sparse=true`，after `alloc=1048576 compressed=1048576 sparse=true`。

### 4.1 关键观察

- **磁盘占用**：SetLen 与 Reserve 在创建后**立即占用与逻辑长度等量的物理空间**（1 GiB），而 Sparse 创建后占用 0，仅在写入后占用实际写入量（本例 1 MiB）——即 1 GiB 文件仅落盘约 1 MiB。
- **尾部写入卡顿**：架构第 3 节预警的“NTFS 超出有效数据长度偏移处写入会同步补零”在 SetLen/Reserve 下确实存在——`tail_write` 高达 ~5.4 s（≈190 MiB/s 的整文件有效写入代价），而 Sparse 因不预分配、按实际写入分配簇，`tail_write` 仅 ~0.5 ms。
- **落盘耗时**：`FlushFileBuffers` 在 SetLen/Reserve 下需刷 ~1 GiB 脏数据（~1.35 s），Sparse 仅刷实际簇（~62 ms）。
- **创建耗时**：三者同一量级（亚毫秒到数毫秒），差异可忽略；SetLen 偶发波动与 NTFS 元数据有关。

## 5. 磁盘空间不足（DiskFull）行为

使用 `--skip-precheck` 变体：跳过 NTFS 前置检查，改为用 `disk_free_space(dir)` 取卷可用空间 `A`，然后尝试创建大小为 `A + 1 GiB` 的文件，验证三种策略都在**创建阶段**即失败、且**不残留文件**。

实测：

```
SKIP_PRECHECK available_bytes=85259239424 attempt_size_bytes=86332981248 runs=1
  SetLen  CREATE_FAILED insufficient disk space ... need 86332981248 bytes, available 85259239424 bytes
  Reserve CREATE_FAILED insufficient disk space ... need 86332981248 bytes, available 85259239424 bytes
  Sparse  CREATE_FAILED insufficient disk space ... need 86332981248 bytes, available 85259239424 bytes
```

- 三策略均返回 `Error::DiskFull`，**不会**创建出半截文件（创建失败原子回滚）。
- 报错时机正确：在 `TargetFile::create` 内部（`set_len` / `SetFileInformationByHandle` / `FSCTL_SET_SPARSE` 之后的空间落盘或扩展阶段）即被捕获，符合架构第 3 节“磁盘空间不足时的报错时机”要求。

## 6. 异常退出后的文件状态

- `TargetFile::create` 在分配/扩展失败时返回错误并删除已创建的文件句柄（见 `target.rs`：失败路径 `drop` 文件且无残留），因此“创建即失败”不会产生半截文件。
- 运行中进程崩溃：由于写入采用 `seek_write` 显式偏移 + 先写数据后由上层记录进度（架构第 4 节），已落盘区域为有效数据，未写区域保持稀疏/零；重启后由 SQLite 进度恢复，不会出现“进度完成但内容错误”。
- Sparse 在崩溃后未写区域不占空间、读为 0，与 `byte_at` 校验逻辑一致。

## 7. 结论与建议

1. **默认策略选 Sparse**。`DeviceIoControl(FSCTL_SET_SPARSE)` 在 NTFS 上几乎零成本完成空间“预留”语义：创建即时、磁盘占用仅实际写入量、尾部首次写入无补零卡顿、落盘极快，且天然满足架构第 2/3 节对“断电一致性”与“不要宣称已保证物理分配”的约束。
2. **SetLen / Reserve 仅在明确需要“保证物理分配”语义时作为可选策略保留**，但实测显示它们会在创建时占满等量物理空间并引入尾部写入/落盘代价，Phase 3 默认不应启用。
3. **校验前置检查必须基于卷根**：`volume_filesystem()` 已修正为先用 `GetVolumePathNameW` 解析卷根再查询（原始实现对子目录路径会失败），这是 bench-alloc 与未来任何按卷判断逻辑的前提。
4. **DiskFull 已在创建阶段正确捕获**：三种策略均原子失败、无残留文件，可作为 Phase 3 “空间不足优雅降级”的基础。

## 8. 实测中修复的两个实现缺陷（与报告相关）

- **`make_chunks` 覆盖区间错误**：原实现硬编码 `FILE_SIZE`（64 MiB）生成 Chunk 边界，导致 2 GiB 压力测试（`#[ignore]`）中后半段文件未被覆盖、校验失败。已改为接收 `size` 参数并贯穿 `run_std_concurrency` / `run_pool_concurrency` / `stress_three_strategies`，2 GiB × 3 策略 × 3 种子压力测试现已通过（release 下 32.39 s）。
- **`get_volume_filesystem` 卷根解析错误**：原实现直接把子目录路径传给 `GetVolumeInformationW`（要求卷根），相对路径下返回 `ERROR_INVALID_NAME(123)`。已改用 `GetVolumePathNameW` 解析卷根后再查询，bench-alloc 前置检查与 skip-precheck 均正常。
