# Phase 2.2.1 报告：接管的两处补全

- 任务原文：`docs/prompts/phase2.2.1.md`；开始时 `git status` 干净，基线提交 `8db8207`。
- 按任务第三部分**只替换**了 10.5 节的「切分点」一条（换成指定的 3 行），
  `ARCHITECTURE.md:126-136` 其余 10.5 内容、10.1~10.4 全部保持原样。
- 2.2 报告中的偏差 1~6 全部保持不变。
- 提交信息：`phase2.2.1: unaligned takeover and zero-progress takeover`
  （只暂存 `docs crates scripts ARCHITECTURE.md Cargo.lock`，未 push、未打 tag）。

## 一、每项修改的位置

| 任务项 | 位置 | 内容 |
|---|---|---|
| 一 不对齐切分点 | `crates/idm-core/src/task/split.rs:255`（`compute_takeover_point`）、`:270`（`split_point`） | 接管 `split = reserved`，不再 `div_ceil(64 KiB)`；仍要求 `end − split ≥ 256 KiB`。对半切继续用 `compute_split_point` 向下对齐 64 KiB。删除了 `select_victim` / `try_split` 里 `split > start` 的旧限制。 |
| 二 零进度接管判定 | `split.rs:265`（`needs_deferred_allocation`）、`:505` | `mode == Takeover && split == start` 时跳到第 6/7 步之外，改记 `PendingChunk`。 |
| 二 延迟分配数据结构 | `split.rs:307`（`PendingChunk`）、`:318`（`launch_new_chunk`）、`:506`（`pending.push`） | 第 7 步抽成共用函数；零进度时只把 `victim_db_index / shared / init / deadline(5s)` 记入 pending，不分配写器、不发请求。第 6 步 `shrink_end` 在该分支不调用。 |
| 二 分块结果携带编号 | `crates/idm-core/src/task/download.rs:1020`（`ChunkResult::Completed { db_index }`）、`:641` | 主循环靠它判断"被接管分块的任务已结束"（`run_chunk` 返回前已 `pool.release`）。 |
| 二 主循环延迟处理 | `download.rs:624`（`pending`）、`:648`（完成事件后处理）、`:709`（定时检查）、`:1628`（`process_pending_chunks`） | victim 以 `Completed` 结束 → 立刻执行延迟的第 7 步；超过 5 秒仍未结束 → `Error::Io` 让任务失败。等待期间主循环照常用 `select!` 处理其它事件，不阻塞。pending 计入连接名额（`:665`、`:726`）并阻止提前收尾（`:630` 附近的循环条件）。 |
| 二 响应头唤醒点 | `download.rs:1360`~`:1376` | `req.send()` 只创建一次 send future（`std::pin::pin!`），`select!` 中同时监听 `wake_on_shrink`；`acked == end` 时丢弃请求（drop send future → 关闭连接）返回 `Completed`，否则继续等同一个 send future，不重发请求。 |
| 测试服务器 | `crates/idm-core/tests/support/mod.rs:55`（`StallConfig` 新增 `after_bytes=0` / `stall_used` / `variable_blocks`）、`:483`（只停第一个 `start==0` 请求）、`:492`（零进度：不发响应头）、`:515`~`:543`（1~16 KiB 变长块、恰好发满 `after_bytes`）、`:622`（`wait_for_client_close`） | 停住期间不发数据、不主动关闭，只记录客户端是否提前关闭。 |
| 测试断言适配 | `tests/phase2.rs:141`（`assert_chunks_partition`）、`:272`（`split_with_retries`）、`tests/phase1c_pre.rs:66` | 允许空区间记录；两个原有测试改为不依赖"恰好 N 个分块 / 两个分块谁先完成"的竞态（详见第六节）。 |
| 兼容 clippy | `download.rs:966`（`ChunkSpawner::min_split`） | `min_split_size` 移入启动器，使 `try_split` 参数不超过 7 个（项目禁止在 `win32.rs` 之外使用 `#[allow]`）。值不变。 |

## 二、每个新测试 / 改动过的测试：单独连续运行 10 次

| 任务项 | 测试名 | 位置 | 10 次结果 |
|---|---|---|---|
| 四.4 | `takeover_split_point_equals_reserved_and_requires_minimum_remaining` | `split.rs:825`（改名并改断言） | **10/10** |
| 四.4 | `zero_progress_takeover_uses_deferred_allocation` | `split.rs:860`（新增） | **10/10** |
| 四.4 | `takeover_commit_allows_reserved_equal_split` | `split.rs`（未改，回归） | **10/10** |
| 四.1 | `unaligned_stalled_first_chunk_is_taken_over` | `tests/phase2.rs:776`（替换 2.2 停顿测试） | **10/10** |
| 四.2 | `zero_progress_stalled_first_chunk_is_taken_over` | `tests/phase2.rs:825`（新增） | **10/10** |
| 四.3 | `very_slow_first_chunk_is_taken_over` | `tests/phase2.rs`（行为变化，未改代码） | **10/10** |
| 五（回归适配） | `completed_chunks_report_written_elapsed_speed_and_state` | `tests/phase1c_pre.rs:66` | **10/10** |
| 五（回归适配） | `split_with_retries` | `tests/phase2.rs:272` | **10/10** |
| 五（回归） | `server_side_concurrency_never_exceeds_connections` | `tests/phase2.rs` | **10/10** |
| 五（回归） | `resume_when_largest_split_chunk_already_completed` | `tests/phase2.rs` | **10/10** |
| 五（回归） | `cancel_during_split_then_resume` | `tests/phase2.rs` | **10/10** |

运行方式：单元测试 `cargo test -p idm-core --lib --locked <名字>`；集成测试
`cargo test -p idm-core --test phase2 --locked <名字>`（极慢/停顿测试加 `-- --nocapture`
记录耗时）；`phase1c_pre` 用 `cargo test -p idm-core --test phase1c_pre --locked <名字>`。
全部在最终代码上逐个单独连续运行 10 次，无一次失败。

## 三、第 1、2、3 项实测耗时

### 第 1 项：不对齐停顿（8 MiB / 2 连接；先按 1~16 KiB 变长块发满 100000 字节，再停 20 秒）

| 轮次 | 开启切分耗时 | 切分点 | 客户端关闭时刻 |
|---|---|---|---|
| 1 | 4.070 s | 100000 | 4035 ms |
| 2 | 4.062 s | 100000 | 4024 ms |
| 3 | 4.095 s | 100000 | 4060 ms |
| 4 | 4.040 s | 100000 | 4006 ms |
| 5 | 4.089 s | 100000 | 4055 ms |
| 6 | 4.133 s | 100000 | 4099 ms |
| 7 | 4.109 s | 100000 | 4076 ms |
| 8 | 4.080 s | 100000 | 4028 ms |
| 9 | 4.051 s | 100000 | 4016 ms |
| 10 | 4.136 s | 100000 | 4103 ms |

全部 < 5 秒；`split_point` 恒为 **100000**（不是 64 KiB 的倍数，证明没有向上对齐），
`takeover_count = 1`，数据库分块记录满足 10.4，文件逐字节正确。

### 第 2 项：零进度停顿（接受连接后 20 秒内不返回任何响应头，也不关闭连接）

| 轮次 | 开启切分耗时 | 分块记录数 | 客户端关闭时刻 |
|---|---|---|---|
| 1 | 4.021 s | 5 | 4004 ms |
| 2 | 4.076 s | 5 | 4059 ms |
| 3 | 4.095 s | 5 | 4077 ms |
| 4 | 4.084 s | 5 | 4066 ms |
| 5 | 4.084 s | 5 | 4067 ms |
| 6 | 4.021 s | 5 | 4003 ms |
| 7 | 4.049 s | 5 | 4030 ms |
| 8 | 4.021 s | 5 | 4003 ms |
| 9 | 4.062 s | 5 | 4044 ms |
| 10 | 4.020 s | 5 | 4003 ms |

全部 < 5 秒；数据库含 1 条 `[start, start)` 空区间记录，10.4 的覆盖/不重叠不变式
仍然成立；文件逐字节正确。

### 第 3 项：极慢分块（10 KiB/s；960 KiB / 3 连接）

| 轮次 | 开启切分 | `--no-split` |
|---|---|---|
| 1 | 2.563 s | 32.000 s |
| 2 | 2.193 s | 32.224 s |
| 3 | 2.230 s | 32.095 s |
| 4 | 2.541 s | 32.198 s |
| 5 | 2.458 s | 32.080 s |
| 6 | 2.512 s | 32.060 s |
| 7 | 2.099 s | 31.904 s |
| 8 | 2.550 s | 32.088 s |
| 9 | 2.532 s | 32.108 s |
| 10 | 2.513 s | 32.109 s |

开启切分 **2.099~2.563 s**（10 轮），对比 2.2 的 6.06~6.11 s，约缩短 59%。
原因：`split = reserved` 之后接管不再给原分块留下最多 64 KiB 的 10 KiB/s 尾巴。

## 四、测试服务器关于连接被提前关闭的记录

`StallConfig.closed_early` / `close_after_ms` 由服务器在停住期间通过
`tokio::time::timeout(20s, sock.read(...))` 写入：只有读到对端关闭（`Ok(0)` 或错误）
才置位；服务器自身既不发数据也不主动关闭连接。

- 第 1 项 10 轮：`closed_early = true`，检测到关闭的时刻 **4006~4103 ms**，
  全部远早于 20 s 停住期满；与 `run_download` 的 ~4.1 s 一致。
- 第 2 项 10 轮：`closed_early = true`，检测到关闭的时刻 **4003~4077 ms**；
  客户端是在等待响应头（`req.send()`）时被唤醒后主动 drop 请求，服务器读到 EOF。
- 停住期满（20 s）之前服务器没有向该连接写入任何额外字节；第 1 项恰好写满
  100000 字节，第 2 项连响应头都没写。

## 五、测试总数与验收

- `cargo test --workspace --locked`：**182 passed，0 failed，4 ignored**
  （idm-cli 7 + phase1d 1、idm-core lib 71、disk_concurrency 5、phase1b 39、
  phase1c1 12、phase1c_pre 2、phase1c_resume 9、phase1c_store 11、phase1d 13、
  phase2 12；ignored 为 2 个崩溃测试 + 2 个原有 `#[ignore]`）。
- 回归：phase2 第 5 项 `eight_connections_sixty_four_mib_fifty_seeds`（50/50 种子，
  73.66 s）通过；两个崩溃测试各 20 轮
  （`cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1`，
  93.02 s）通过。
- `scripts/verify-workspace.ps1`：**16 个步骤全部 PASS，exit code 0**
  （含 `cargo fmt --check`、`cargo check`、`cargo test --workspace`、
  `clippy -D warnings`、`cargo doc`、依赖静态检查与两个崩溃测试）。
- 稳定性：`cargo test --workspace --locked --no-fail-fast` 连续 2 次通过；
  `cargo test -p idm-core --test phase2 --locked`（12 个测试并发）连续 3 次通过。

## 六、与任务原文不一致之处（含实现解释）

1. **`ChunkResult::Completed` 增加 `db_index` 字段**。任务只说"等被接管分块的任务以
   Completed 结束"；要在主循环里把"某个 Completed 事件"和 pending 的 victim 对上，
   必须让结果携带编号。这是纯结构性改动，其它语义不变。
2. **`min_split_size` 移入 `ChunkSpawner`**。`try_split` 增加 `pending` 参数后会变成
   8 个参数，触发 clippy `too_many_arguments`；项目禁止在 `win32.rs` 之外使用
   `#[allow]`（验收脚本第 13 步），因此把 `min_split` 放进启动器。值不变。
3. **pending 计入连接名额**：`join_set.len() + pending.len() < max_concurrent` 才允许
   新的切分，且 `pending` 非空时主循环不能因为 `join_set` 暂时为空就收尾。
   否则延迟分块会与一次普通切分争用同一条空闲连接，或在启动前就被 finalize。
4. **零进度分块在满 4 秒"卡住"之前可能先被普通对半切**。它刚创建、速度为 0 时
   `is_fresh=true`，在某个其它分块完成后会被选中并做对半切（8 MiB/2 连接时最多
   两次：4 MiB → 2 MiB → 1 MiB）。到 4 秒时它变成"卡住"，`reserved == start` 的
   接管才走延迟分配。测试中数据库最终为 5 条记录（2 个初始 + 2 个对半切 + 1 个
   延迟接管），其中 1 条是空区间；任务只要求"最终走延迟路径"，并未禁止之前的对半切。
5. **测试服务器只停第一个 `start == 0` 的非 Probe 请求**（`stall_used`）。零进度接管
   产生的新分块起点仍然是 0，若继续被停住会无限接管；这是测试服务器为保证零进度
   场景可结束而加的开关，不是生产逻辑。
6. **原有测试的必要适配**（都保留了原断言意图）：
   - `phase1c_pre::completed_chunks_report_written_elapsed_speed_and_state`：
     `chunks.len() == 8` 改为 `>= 8`。2.2.1 起一个测量速度明显偏低的分块会被合法
     接管，多出一条分块记录；其余"终态 Done / 耗时 / 速度 / 覆盖字节数 /
     文件逐字节正确"断言不变。
   - `phase2::split_with_retries`：限速策略从 `only_slow_at(0, …)` 改为
     `slow_in_prefix(4 MiB, …)`。原配置下第 0 号分块重试后起点变成 256 KiB 就恢复
     全速，两个分块几乎同时结束，"重试 + 切分"依赖竞态，在并发负载下偶发
     `split_count=0`；改后第 0 号分块重试后仍慢，场景稳定复现。断言未放松。
   - `tests/support/mod.rs`：限速等待的 `sleep` 改为与 `sock.peek()` 并列的
     `select!`。客户端因接管提前关闭连接时，服务器会在限速 sleep 期间立即结束该响应
     并注销并发计数，否则 Phase 2 第 10 项的"服务器端并发峰值 ≤ 连接数"会把
     已经断开的旧连接和刚启动的新连接短暂一起计数（实测出现过峰值 5 > 4）。
     `peek` 不消费任何字节，其它测试行为不变。
   - `assert_chunks_partition`：允许 `[start, start)` 空区间、按 `(start, end)` 排序，
     并同时校验 `committed ∈ [start, end]`。
7. **第 1 项不再跑 `--no-split` 的 20 秒对照**（2.2 的停顿测试有）。任务第 4.1 项只
   要求"开启切分时 5 秒内完成 / 连接被提前关闭 / 逐字节正确"，因此测试从 ~24 秒
   缩短到 ~4 秒；报告里只给出开启切分的 10 轮耗时。第 3 项仍保留 `--no-split` 对照。
8. **接管后原分块保留的是 `[acked, reserved)`**：若唤醒时还有一次写盘在途
   （acked < reserved），唤醒检查会看到 `acked < end` 并继续等；写盘回执到达后
   由 2.2 偏差 4 的"回执后再确认"立即以 Completed 结束。不会再留下 64 KiB 尾巴。
2.2 报告的偏差 1~6 按要求全部保持不变。
