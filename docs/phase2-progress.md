# Phase 2 进度（动态分块与工作窃取）

任务原文：`docs/prompts/phase2.md`。本文件记录阶段进度，供上下文超限后继续。

更新时间：2026-11-06（本对话第 1 次现状盘点）

## 一、当前工作区状态

`git status`：11 个已修改文件 + 3 个未跟踪文件，均未提交。

```
modified:   ARCHITECTURE.md
modified:   crates/idm-cli/src/main.rs
modified:   crates/idm-core/src/net/client.rs
modified:   crates/idm-core/src/store/mod.rs
modified:   crates/idm-core/src/task/download.rs        (+712)
modified:   crates/idm-core/src/task/progress.rs
modified:   crates/idm-core/src/task/resume.rs
modified:   crates/idm-core/tests/phase1c1.rs
modified:   crates/idm-core/tests/phase1c_resume.rs
modified:   crates/idm-core/tests/phase1c_store.rs
modified:   crates/idm-core/tests/support/mod.rs
untracked:  crates/idm-core/tests/phase2.rs
untracked:  crates/idm-core/tests/support/speed.rs
untracked:  docs/prompts/phase2.md
```

## 二、已完成的项（对照任务第二 ~ 四部分）

### 第二部分：ARCHITECTURE.md
- 第 10 节「动态切分协议」已按原文追加，文字逐条未改。

### 第三部分：实现要求
- 3.1 每分块速度：`ChunkShared` 内 4 秒滑动窗口采样（`SPEED_WINDOW`、`prune_samples`）。
- 3.2 配置：`DownloadConfig::min_split_size`（默认 1 MiB）、`dynamic_split`（默认 true）；
  CLI `download` / `resume` 均支持 `--no-split`。
- 3.3 来源列与切分次数：`ChunkProgress.db_index` / `parent`、`ProgressSnapshot.split_count`；
  CLI 汇总表新增 `source=initial|split_from_N`，`RESULT ... splits=N`。
- 3.4 续传：`ResumePlan.chunks` 改为 `ResumeChunk { index, start, end, committed, parent }`，
  `run_download` 读取**全部**分块记录，`committed < end` 的继续下载（起点 = committed），续传中仍可切分。
- 3.5 切分日志：`info!(task_id, victim, split, new_chunk, speed_bps, eta_secs, "chunk split")`。
- 数据库：schema V2（`chunks.parent_index`）+ `Store::split_chunk` 单事务（改 end + 插入新分块）。

### 第四部分：测试
- 第 1 项（切分点计算）：`crates/idm-core/src/task/split.rs` 的
  `tests::split_point_is_aligned_and_respects_min_size` —— **已写，通过**。
- 第 2 项（写入截断）：`split::tests::reserve_truncates_at_end` —— **已写，通过**。
- 第 3 项（第 4 步竞争）：`split::tests::split_commit_loses_the_race` —— **已写，通过**。
- 第 4~10 项：`crates/idm-core/tests/phase2.rs` 7 个测试，**全部通过**
  （`slow_chunk_split_beats_no_split`、`eight_connections_sixty_four_mib_fifty_seeds`、
  `split_with_retries`、`cancel_during_split_then_resume`、`resume_reads_split_chunk_records`、
  `rate_limit_holds_with_splits`、`server_side_concurrency_never_exceeds_connections`）。
- 第 11 项：`crates/idm-cli/tests/crash_resume.rs::crash_resume_with_split_twenty_rounds`
  （`#[ignore]`，20 轮）—— **已完成并通过**。
- 第 12 项：`crates/idm-cli/tests/crash_resume.rs::crash_resume_twenty_rounds`（原测试，
  20 轮）—— 通过。
- 第 11 项（崩溃测试扩展）：**已完成**。`crates/idm-cli/tests/crash_resume.rs` 新增
  `crash_resume_with_split_twenty_rounds`（`#[ignore]`）：
  - 测试服务器新增 `Spec { size, slow_prefix, slow_delay }`：起点 < 1 MiB 的请求按
    4 ms/8 KiB（≈2 MiB/s）发送，其余全速；文件 32 MiB / 4 连接 → 初始分块 8 MiB。
  - 慢分块（第 0 号）被连切两次以上（实测每轮 2~3 次切分），等数据库出现 ≥ 2 条
    切分记录且有落盘进度后 `Child::kill`。
  - 强杀后校验：`[start, committed)` 逐字节正确；分块记录满足 10.4（不重叠、覆盖
    `[0, size)`、`start ≤ committed ≤ end`、至少一条 `parent_index` 非空）；`resume 1`
    后最终文件逐字节正确。
  - 实测 20 轮全部通过（每轮 verified 17.3~20.3 MiB / 32 MiB，splits 2~3）。
- 第 12 项：原有测试全部通过；原崩溃测试 `crash_resume_twenty_rounds` 20 轮通过
  （每轮 verified 640 KiB~2.27 MiB / 4 MiB）。

## 三、当前编译与测试状态

- `cargo check --workspace --all-targets`：**通过**，0 警告。
- `cargo clippy --workspace --all-targets -- -D warnings`：**通过**。
- `cargo test --workspace`：**全部通过**
  - idm-core lib 62、disk_concurrency 5(+1 ignored)、phase1b 39(+1)、phase1c1 12、
    phase1c_pre 2、phase1c_resume 9、phase1c_store 11、phase1d 13、**phase2 7**、
    idm-cli 6 + phase1d 1；crash_resume 仍是 1 个 `#[ignore]`（第 11 项待扩展）。
- 第 4 项实测：split ≈ 0.6 s（1 次切分），no_split ≈ 1.2 s（见测试内 println）。
- 第 10 项实测：服务器端最大并发 = 4（= 配置连接数，断言峰值 ≤ 4 通过）。

## 四、上一个对话停在哪里（从 diff 判断）

- 代码主体（切分协议、DB 事务、进度、CLI、测试服务器能力）已经写完，能编译；
  `phase2.rs` 的第 4、5、6、9 项已通过（其中第 5 项 50 个种子 64 MiB 通过）。
- 停在「修 phase2.rs 里 3 个失败的集成测试」这一步，第 11 项崩溃测试扩展尚未动笔，
  验收脚本与自查表未做，未提交。

### 本次对话已修复的 3 个失败测试（原因/修法）

- `cancel_during_split_then_resume`（`tests/phase2.rs:124`）：`assert_chunks_partition`
  按 `chunk_index` 顺序校验空间连续性，而切分出的分块编号在空间上夹在中间。
  → 改为按 `start` 排序后校验（10.4 只要求不重叠且覆盖 `[0, total)`）。
- `resume_reads_split_chunk_records`（`tests/phase2.rs:413`）：原来只有 `start==0`
  的请求慢，切出的分块全速，取消前就已下完（`committed == end`），续传时被过滤。
  → 新增 `speed::slow_in_prefix(8 MiB, 4 MiB/s)`，让切出的分块在取消时仍未完成。
- `server_side_concurrency_never_exceeds_connections`（`tests/phase2.rs:545`）：
  `srv.shutdown()` 后服务器任务在下一个调度点才执行 `ActiveGuard::drop`，断言读太早。
  → shutdown 后短暂轮询等待计数归零（最多 1 s）。

## 五、与协议不一致、需要记录的地方

1. 10.3 第 6 步 `pool.shrink_end` 失败时：实现没有直接让任务失败，而是
   `ChunkNotRegistered` 记 warn 后跳过（其余错误仍然失败）。
   原因：第 4 步 commit_split 之后到第 6 步之间有 `.await`（数据库事务），
   被切分方可能在这期间写完 `[reserved, split)` 并 `release` 写器，
   此时 shrink 必然返回 `ChunkNotRegistered`。由于共享 end 已在第 4 步缩到 split，
   写器不可能写过 split，跳过缩小是安全的；若按字面「必定成功，否则失败」处理，
   任务会随机失败。**待用户确认是否接受该偏差。**
2. 10.2 第 2 步「主动关闭连接」：实现为丢弃 `resp`（reqwest 在 drop 时关闭连接），
   未显式调用关闭 API。

## 六、下一步（待办）

1. ~~把 `download.rs` 中切分相关代码搬到 `crates/idm-core/src/task/split.rs`~~ **已完成**：
   `ChunkShared` / `SharedInner` / `compute_split_point` / `prune_samples` / `ChunkSlot` /
   `select_victim`（新抽出的 10.3 第 1 步）/ `try_split` / `SPEED_WINDOW` / `SPLIT_ALIGN`
   以及三个单元测试都搬到 `task/split.rs`；`download.rs` 只保留 `ChunkSpawner` 等编排代码，
   通过 `AckedList` 类型别名与 `pub(super)` 成员与 split.rs 对接。逻辑未改。
2. ~~修 3 个失败的集成测试~~ **已完成**（见第四节）。
3. 第 11 项：扩展崩溃测试为「慢速分块 + 开启切分」场景（20 轮）。**进行中**
4. **已完成**：`scripts/verify-workspace.ps1` 16 步全 PASS；自查表（每个测试单独 10 次、
   第 5/11 项各完整跑 1 次）写在 `docs/phase2-report.md`；提交 `phase2: dynamic splitting and work stealing`。
5. **已完成**：第五部分真实网络对比验收写在 `docs/phase2-network-acceptance.md`，
   提交 `phase2: real network acceptance`。A（`--no-split`）349.66 s / 2267136 B/s；
   B（开启切分）755.21 s / 1049680 B/s；两次 SHA256 都与官方一致、并发峰值 8、无残留。
   B 未发生切分：分块数 = 连接数，链路公平分摊带宽，首个分块完成时其余剩余 ≈1.7 MB
   < 2 × `min_split`，协议第 3 步正确放弃。下载文件已删除。

## 七、测试记录（本次对话实跑）

- 单元 1~3、集成 4/6/7/8/9/10：各自单独连续运行 **10/10 通过**。
- 第 5 项：8 连接 64 MiB × 50 种子，1 次通过，76 s。
- 第 11 项：`crash_resume_with_split_twenty_rounds` 20 轮通过（每轮 splits 2~3，
  强杀时已落盘 17.3~20.3 MiB / 32 MiB）。
- 第 12 项：`cargo test --workspace` 167 passed / 0 failed / 4 ignored；
  原崩溃测试 `crash_resume_twenty_rounds` 20 轮通过。
- 第 4 项耗时：`split=1.46s (1 次切分) no_split=2.61s`。
- 第 10 项：服务器端并发峰值 4 = 连接数。
