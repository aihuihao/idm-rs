# Phase 2.1 报告：切分修正

- 任务原文：`docs/prompts/phase2.1.md`（开始时 `git status` 干净，基线提交 `b10c102`）。
- `ARCHITECTURE.md` 第 10 节的协议**步骤与顺序未改动**；本阶段只修正实现。
- 提交信息：`phase2.1: resume index fix and victim selection`（只暂存 `docs crates scripts Cargo.lock`，未 push、未打 tag）。

## 一、每项修改的位置

| 任务项 | 位置 | 修改内容 |
|---|---|---|
| 一、续传编号冲突 | `crates/idm-core/src/task/download.rs:305`、`:573` | 续传时从 `ctx.resume.plan.chunks`（`resume()` 从数据库**全部**分块记录构造，见 `resume.rs:146`）取最大编号；`next_db_index = max(全部) + 1`，新建下载仍取 `inits` 最大编号。 |
| 二、速度反映"卡住" | `crates/idm-core/src/task/split.rs:126`（`recent_speed`）、`:130`（`recent_speed_at`） | 分母改为 `当前时间 − 窗口起点时间`（不再用最后一个采样时间）；最后一个采样已超过 `SPEED_WINDOW`（4 秒）时直接返回 0。 |
| 三、选择被切分分块 | `crates/idm-core/src/task/split.rs:234`（`select_victim`）、`:295`（`try_split` 传 `min_split`）、`:48`/`:50`/`:114`/`:152`（`started` / `has_data` / `is_fresh`） | ① 先用 `compute_split_point` 过滤，只在能切分的分块里比较 ETA；② 刚创建（启动 < 4 秒且无写入）的分块用全体"正在下载且有速度数据"分块的平均速度估算，全都没有速度数据时按无穷大；③ 启动超过 4 秒且窗口内无数据的分块速度为 0、ETA 无穷大，优先被切分。`try_split` 第 2~7 步顺序未变，选择器只接收 `ChunkShared`，写器句柄仍留在 `slots`。 |
| 三、集成测试服务器 | `crates/idm-core/tests/support/mod.rs:56`（`StallConfig`）、`:135`、`:460`、`:484` | 起点为 0 的响应写出 `after_bytes` 后完全停住（保持连接、不发送、不关闭），直到放行信号置位或达到 `max`（6 s < 默认 30 s `read_timeout`）；切分产生的新请求（起点落在第 0 号分块内部）置位放行信号。 |
| 四、启动循环错误路径 | `crates/idm-core/src/task/download.rs:587`（`spawner.open` 失败）、`:1462`（`cooperative_drain`）、`:675`（主循环复用） | `open` 出错时先 `internal.cancel()`，再走与主循环相同的"等待最多 5 秒、超时才 `abort_all`"协作式排空，最后返回错误；把主循环里那段排空抽成共用函数，行为不变。 |

## 二、新增测试与单独连续运行 10 次的结果

| 任务项 | 测试名 | 位置 | 10 次连续运行 |
|---|---|---|---|
| 一 | `resume_when_largest_split_chunk_already_completed` | `crates/idm-core/tests/phase2.rs:594` | **10/10 通过** |
| 二 | `recent_speed_drops_to_zero_when_stalled` | `crates/idm-core/src/task/split.rs:537` | **10/10 通过** |
| 三① | `victim_selection_skips_unsplittable_chunks` | `crates/idm-core/src/task/split.rs:563` | **10/10 通过** |
| 三② | `victim_selection_estimates_fresh_chunk_with_average_speed` | `crates/idm-core/src/task/split.rs:596` | **10/10 通过** |
| 三③ | `victim_selection_prefers_stuck_chunk` | `crates/idm-core/src/task/split.rs:622` | **10/10 通过** |
| 三（集成） | `stalled_first_chunk_is_split_and_beats_no_split` | `crates/idm-core/tests/phase2.rs:714` | **10/10 通过** |
| 四 | `cooperative_drain_waits_for_started_tasks` | `crates/idm-core/src/task/download.rs:1511` | **10/10 通过** |

运行方式：单元测试 `cargo test -p idm-core --lib --locked <名字>`；集成测试
`cargo test -p idm-core --test phase2 --locked <名字>`。每次都用 `--locked`，逐个
单独连续运行 10 次，7 个测试全部 10/10，无一次失败。

第一项测试的**反向验证**：把 `next_db_index` 临时改回"只看 `inits`"，该测试立即失败于
`Database { message: "UNIQUE constraint failed: chunks.task_id, chunks.chunk_index" }`，
确认它确实覆盖了所述的主键冲突；随后已恢复修复版代码。

## 三、第三部分集成测试的实测耗时

服务器：8 MiB 文件、2 连接；第 0 号分块写出 256 KiB 后完全停住，最长 6 s；其余分块正常。
`stalled_first_chunk_is_split_and_beats_no_split` 连续运行 10 次，每次都同时跑
「开启切分」与 `--no-split` 两种方式：

| 轮次 | 开启切分 | `--no-split` |
|---|---|---|
| 1 | 87.99 ms | 6.048 s |
| 2 | 91.80 ms | 6.046 s |
| 3 | 89.30 ms | 6.078 s |
| 4 | 86.13 ms | 6.052 s |
| 5 | 93.77 ms | 6.057 s |
| 6 | 91.68 ms | 6.043 s |
| 7 | 97.88 ms | 6.053 s |
| 8 | 90.37 ms | 6.055 s |
| 9 | 83.56 ms | 6.054 s |
| 10 | 97.81 ms | 6.045 s |

- 开启切分：83.6~97.9 ms（中位数约 91 ms），每轮切分 1 次，第 0 号分块区间被缩小，
  **远短于配置的停住时长 6 s**。
- `--no-split`：6.043~6.078 s（中位数约 6.053 s），必须等满整个停住时长。
- 两种方式的文件都经 `verify_file` 逐字节校验正确；切分/不切分的断言均通过。

## 四、测试总数与验收

- `cargo test --workspace --locked`：**174 passed，0 failed，4 ignored**
  （idm-cli 6 + phase1d 1、idm-core lib 67、disk_concurrency 5、phase1b 39、
  phase1c1 12、phase1c_pre 2、phase1c_resume 9、phase1c_store 11、phase1d 13、
  phase2 9；ignored 为 2 个崩溃测试 + 2 个原有 `#[ignore]`）。
- `scripts/verify-workspace.ps1`：**16 个步骤全部 PASS，exit code 0**
  （含 `cargo fmt --check`、`cargo check`、`cargo test --workspace`、`clippy -D warnings`、
  `cargo doc`、依赖静态检查，以及第 16 步两个 `#[ignore]` 崩溃测试各 20 轮）。
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过，0 警告。

## 五、与任务原文不一致之处（含实现解释）

1. **"下载完成时间远短于它停住的时长"的实现解释**：被停住的连接既不关闭也不重试，
   而协议第 3 步要求切分后两段都 ≥ 1 MiB，被切分方始终还欠 `[acked, split)` 的数据，
   因此若服务器停满配置的 6 s，任何下载都不可能早于 6 s 完成。为此测试服务器在
   **切分产生的新分块请求到达时提前放行**停住的连接：`--no-split` 没有这种请求，
   于是等满 6 s；开启切分时约 0.09 s 完成。报告中的"停住的时长"指服务器配置的
   最长停住时长 6 s（仍 < 30 s `read_timeout`，期间不关闭连接、不触发重试）。
   这是保持连接不中断的前提下，唯一能让切分显著缩短总耗时的方式。
2. **"平均速度"的口径**：按"本任务所有正在下载的分块"实现为
   `remaining > 0 且 recent_speed > 0` 的分块取算术平均；已完成的分块（remaining=0）
   和速度为 0 的卡住分块不计入。一个都没有时 `avg_speed = 0`，刚创建的分块按
   ETA 无穷大处理（任务原文：就按无穷大处理）。
3. **ARCHITECTURE 10.3 第 1 步**：规范文字"速度为 0 的视为无穷大"按任务第三部分
   第 2 条补充了"刚创建、无数据的分块改用平均速度估算"这一例外。
   **第 10 节的协议步骤文本、编号与顺序均未修改**，只在代码注释中说明这一解释细化。
4. **第四部分没有专门触发 `spawner.open` 失败的集成测试**：任务未要求该测试；
   提供了 `cooperative_drain_waits_for_started_tasks` 单元测试验证"等待已启动任务
   自行退出"的排空语义，错误路径本身由全量回归（含 `verify-workspace.ps1` 的
   16 个步骤与两个 20 轮崩溃测试）间接覆盖。
5. Phase 2 遗留的 10.3 第 6 步 `shrink_end` 对 `ChunkNotRegistered` 记 warn 后跳过
   的偏差（见 `docs/phase2-report.md`）保持不变，本阶段未改动。
