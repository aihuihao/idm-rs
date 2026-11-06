# Phase 2 报告：动态分块与工作窃取

任务原文：`docs/prompts/phase2.md`。实现说明见 `ARCHITECTURE.md` 第 10 节，过程记录见
`docs/phase2-progress.md`。

## 一、提交前自查表（第七部分）

| 第四部分编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|
| 1 | 切分点计算：64 KiB 对齐、两段都不小于 `min_split`、剩余太少放弃 | `split::tests::split_point_is_aligned_and_respects_min_size` | 10/10 通过 |
| 2 | 写入截断：end 被缩小后跨越 end 的数据只写到 end，分块以完成结束 | `split::tests::reserve_truncates_at_end` | 10/10 通过 |
| 3 | 10.3 第 4 步竞争：被切分方前进到 split，切分放弃且不丢数据 | `split::tests::split_commit_loses_the_race` | 10/10 通过 |
| 4 | 第 0 号分块很慢：切分总耗时远短于 `--no-split`，至少切分一次，文件正确 | `phase2::slow_chunk_split_beats_no_split` | 10/10 通过 |
| 5 | 并发压力：8 连接、64 MiB、随机速度、50 个种子 | `phase2::eight_connections_sixty_four_mib_fifty_seeds` | 完整运行 1 次通过（76 s） |
| 6 | 切分与重试同时发生 | `phase2::split_with_retries` | 10/10 通过 |
| 7 | 切分与取消同时发生，之后 resume 能完成 | `phase2::cancel_during_split_then_resume` | 10/10 通过 |
| 8 | 续传读取已切分出的分块记录并完成 | `phase2::resume_reads_split_chunk_records` | 10/10 通过 |
| 9 | 切分与限速：总速度 ≤ 限速值 120% | `phase2::rate_limit_holds_with_splits` | 10/10 通过 |
| 10 | 同时在下载的分块数 ≤ 配置连接数（服务器端统计峰值） | `phase2::server_side_concurrency_never_exceeds_connections` | 10/10 通过 |
| 11 | 崩溃测试：慢速分块 + 开启切分，切分发生后强杀，20 轮 | `crash_resume::crash_resume_with_split_twenty_rounds` | 完整运行 1 次通过（20 轮，splits 2~3/轮） |
| 12 | 原有全部测试 + 原有崩溃测试 | `cargo test --workspace` + `crash_resume::crash_resume_twenty_rounds` | 全量通过；原崩溃测试 20 轮通过 |

（第 1~3 项为 `crates/idm-core/src/task/split.rs` 的单元测试，运行方式
`cargo test -p idm-core --lib <名字>`；第 4~10 项为 `crates/idm-core/tests/phase2.rs` 的集成测试，
运行方式 `cargo test -p idm-core --test phase2 <名字>`；第 11、12 项为 `#[ignore]` 测试，
运行方式 `cargo test -p idm-cli --test crash_resume -- --ignored --test-threads=1`。）

## 二、切分协议各步骤在代码中的位置

### 10.1 分块的共享状态

- `ChunkShared` / `SharedInner`：`crates/idm-core/src/task/split.rs:33`（`new` 在 `:52`，
  `lock` 在 `:68`）。锁内只做整数运算与采样，全部 IO 与 `.await` 都在锁外。
- 速度滑动窗口（4 秒）：`SPEED_WINDOW` `split.rs:21`、`record` `split.rs:103`、
  `recent_speed` `split.rs:111`、`prune_samples` `split.rs:162`。

### 10.2 分块写入（被切分方）

| 步骤 | 位置 |
|---|---|
| 第 1 步：加锁读取 end、在锁内预留 | `split.rs:75`（`ChunkShared::reserve`），调用点 `download.rs:1367` |
| 第 2 步：可写长度为 0 → 丢弃剩余数据、主动关闭连接、以完成结束 | `download.rs:1368-1376`（丢弃 `resp` 即关闭连接，返回 `ChunkExit::Completed`） |
| 第 3 步：只写入可写长度，超出部分截掉 | `download.rs:1377-1383` |
| 第 4 步：写入失败 → reserved 退回 acked，按 1d 规则重试 | `download.rs:1384-1388` → `split.rs:83`（`rollback_reserved`） |
| acked 只在 `pool.write` 成功回执后推进（Release/`record`） | `download.rs:1389-1396` |

### 10.3 切分（窃取方）

`try_split` 入口：`split.rs:218`。

| 步骤 | 位置 |
|---|---|
| 第 1 步：选预计剩余时间最长的分块（速度 0 视为无穷大） | `split.rs:194`（`select_victim`），调用 `split.rs:227` |
| 第 2 步：锁内读 reserved/end，`split = reserved + (end-reserved)/2`，向下对齐 64 KiB | `split.rs:149`（`compute_split_point`），`SPLIT_ALIGN` 在 `split.rs:23` |
| 第 3 步：两段都 ≥ `min_split`，否则放弃 | `split.rs:154-158` |
| 第 4 步：再次确认 `reserved < split`，满足则 `end = split` | `split.rs:134`（`commit_split`），调用 `split.rs:239-246` |
| 第 5 步：单事务改 victim 的 end + 插入新分块（`committed = split`），提交后才发请求 | `split.rs:252-260` → `crates/idm-core/src/store/mod.rs:455`（`split_chunk`），SQL 在 `store/mod.rs:763`、`store/mod.rs:768` |
| 第 6 步：`pool.shrink_end` 把写器区间缩到 split | `split.rs:262-274` |
| 第 7 步：为新区间分配写器 → 注册进度 → 用新连接下载 | `split.rs:276-289` |
| 切分日志（victim / split / 新编号 / speed_bps / eta_secs） | `split.rs:291-299` |

### 10.4 不变式

- 写器不重叠：`pool.shrink_end`（第 6 步）先缩小旧写器，`allocate_chunk`（第 7 步）才分配新的；
  加上第 4 步在锁内把 end 缩到 split，旧分块不可能写过 split。
- 数据库区间覆盖：单事务保证「缩短 victim + 插入新分块」同时生效；
  测试 `phase2::*` 与 `crash_resume::crash_resume_with_split_twenty_rounds` 均按
  「按 start 排序后首尾相接、覆盖 `[0,total)`」校验。

## 三、实测数据

### 第 4 项：两种方式耗时（8 MiB，2 连接，第 0 号分块 2 MiB/s，其余不限速）

```
split=1.4628048s (1 次切分) no_split=2.6087504s
```

切分比不切分快约 44%：不切分时整个 4 MiB 慢速分块由单个连接下载，切分后一半区间
交给空闲连接全速下载。

### 第 10 项：服务器端最大并发

`max_active = 4`，等于配置的连接数 4（8 连接场景另由第 5 项覆盖），没有超出。

### 第 5 项

8 连接、64 MiB、50 个随机种子（种子可控）：50/50 文件逐字节正确，且每次结束时
分块区间互不重叠并正好覆盖 `[0, 64 MiB)`；单次耗时约 76 s。

### 第 11 项

32 MiB / 4 连接、慢速前缀 1 MiB（≈2 MiB/s），20 轮全部满足：强杀瞬间
`[start, committed)` 逐字节正确、分块记录满足 10.4、`resume` 后文件逐字节正确。
每轮实测切分 2~3 次，强杀时已落盘 17.3~20.3 MiB / 32 MiB。

## 四、测试总数与验收脚本

`cargo test --workspace`：**167 passed，0 failed，4 ignored**
（idm-cli 6、idm-core lib 62、disk_concurrency 5、phase1b 39、phase1c1 12、phase1c_pre 2、
phase1c_resume 9、phase1c_store 11、phase1d 13+1、phase2 7；ignored 为 2 个崩溃测试
+ 2 个原有 `#[ignore]`）。

`scripts/verify-workspace.ps1`：**16 个步骤全部 PASS**（fmt / check / test / clippy -D warnings /
CLI 自检 / 依赖与静态检查 / cargo doc / 单独跑崩溃测试）。

## 五、真实网络对比结果

见 `docs/phase2-network-acceptance.md`（第五部分验收，单独一次提交）。
