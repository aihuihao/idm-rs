# Phase 2.2 报告：卡住与极慢分块的接管

- 任务原文：`docs/prompts/phase2.2.md`；开始时 `git status` 干净，基线提交 `adeb9dc`。
- `ARCHITECTURE.md` 第 126~133 行按任务原文**原样**追加了 10.5 节；10.1~10.4 的
  协议步骤、编号与顺序均未改动。
- 提交信息：`phase2.2: takeover for stalled and slow chunks`
  （只暂存 `docs crates scripts ARCHITECTURE.md Cargo.lock`，未 push、未打 tag）。

## 一、每项修改的位置

| 任务项 | 位置 | 内容 |
|---|---|---|
| 二.1 接管判断 / 切分点 | `crates/idm-core/src/task/split.rs:27`（256 KiB 门槛）、`:31`（`SplitMode`）、`:253`（`compute_takeover_point`）、`:262`（`split_point`）、`:315`（`select_victim`）、`:386`（`try_split`） | 卡住（`is_stalled` `:194`）或明显慢（`< avg/4`）时用接管，否则对半切；先过滤再比较仍保留。 |
| 二.1 日志 / 汇总表区分 | `split.rs:479`（`mode=half\|takeover` 日志字段）、`crates/idm-core/src/task/progress.rs:22/:33/:61/:228`（`takeover` 标志与 `takeover_count`）、`crates/idm-cli/src/main.rs:353`（`source=takeover_from_N`）、`:695`、`:845`（`RESULT ... takeovers=N`） | 汇总表区分 `initial` / `split_from_N` / `takeover_from_N`；`split_count` 含接管，`takeover_count` 单列。 |
| 二.2 唤醒通道 | `split.rs:63`（`watch` 发送/接收端）、`:109`（`subscribe_end`）、`:214`（`commit_split` 锁外 `send_replace`） | end 的镜像通道，状态语义，订阅后发生的缩小不会漏掉。 |
| 二.2 等待网络数据时唤醒 | `crates/idm-core/src/task/download.rs:1118`（`wake_on_shrink`）、`:1403`（`resp.chunk()` 的 `select!` 分支） | 被唤醒后加锁读 end，`acked == end` 时丢弃响应（关闭连接）返回 Completed。 |
| 二.2 退避等待时唤醒 | `download.rs:1223`~`:1240`（带 deadline 的 `select!` 循环） | 接管到 acked 时立即 Completed；否则继续等完原本的退避时长。 |
| 二.3 剩余长度为 0 的保护 | `download.rs:1293`（`start >= end` 直接 Completed）、`:1300`（Range 只剩 `format!("bytes={}-{}", start, end-1)`）、`:1489`（写盘回执后再确认一次） | 删除了 `expected == 0` 时发送 `bytes=0-0` 的分支；分块请求不再可能出现起点等于终点。 |
| 二.1（配套）空闲连接定期重试 | `download.rs:618`（100ms `interval`）、`:681`（`split_retry.tick()` 分支） | 卡住要等满 4 秒窗口才成立，只在"分块完成"时尝试一次不够；有空闲连接且暂时无合法候选时继续重试。 |
| 测试服务器能力 | `crates/idm-core/tests/support/mod.rs:55`（重写 `StallConfig`）、`:89`（`break_at_start`）、`:117`（`chunk_range_sink`）、`:140`（`block`）、`:489`~`:520`（停住/提前关闭检测）、`:549`（按起点断开）；`tests/support/speed.rs:41`（`slow_first_and_two_others`） | 停住期间不发数据、不主动关闭，只观察客户端是否提前关闭；去掉了 2.1 的"新请求到达时放开连接"机制。 |

## 二、新增测试与 10 次连续运行的结果

| 任务项 | 测试名 | 位置 | 10 次连续运行 |
|---|---|---|---|
| 三.1 | `takeover_split_point_aligns_up_and_requires_minimum_remaining` | `split.rs:781` | **10/10** |
| 三.1 | `takeover_commit_allows_reserved_equal_split` | `split.rs:811` | **10/10** |
| 三.2 | `wake_on_shrink_returns_completed_within_100ms` | `download.rs:1613` | **10/10** |
| 三.3 | `stalled_first_chunk_is_taken_over` | `tests/phase2.rs:738` | **10/10** |
| 三.4 | `very_slow_first_chunk_is_taken_over` | `tests/phase2.rs:797` | **10/10** |
| 三.5 | `backoff_wait_is_taken_over` | `tests/phase2.rs:852` | **10/10** |
| 二.1（汇总表） | `chunk_summary_distinguishes_takeover` | `crates/idm-cli/src/main.rs:1009` | **10/10** |

另有两处被改动的既有单元测试，也各自跑了 10/10：
`victim_selection_skips_unsplittable_chunks`（把"太小"的分块从 1 MiB 改为 200 KiB）、
`victim_selection_prefers_stuck_chunk`（新增 `mode == Takeover` 断言）。

运行方式：单元测试 `cargo test -p idm-core --lib --locked <名字>` /
`cargo test -p idm-cli --bin idm-cli --locked <名字>`；集成测试
`cargo test -p idm-core --test phase2 --locked <名字>`。每个测试单独连续运行 10 次，
无一次失败。

## 三、第 3、4 项实测耗时

### 第 3 项：停住 20 秒（8 MiB / 2 连接；第 0 号分块写出 256 KiB 后静默 20 秒）

| 轮次 | 开启切分 | `--no-split` | 服务器观察到客户端关闭的时刻 |
|---|---|---|---|
| 1 | 4.052 s | 20.043 s | 4021 ms |
| 2 | 4.071 s | 20.063 s | 4021 ms |
| 3 | 4.114 s | 20.049 s | 4085 ms |
| 4 | 4.077 s | 20.046 s | 4046 ms |
| 5 | 4.037 s | 20.047 s | 4009 ms |
| 6 | 4.099 s | 20.060 s | 4058 ms |
| 7 | 4.052 s | 20.043 s | 4022 ms |
| 8 | 4.063 s | 20.048 s | 4034 ms |
| 9 | 4.068 s | 20.047 s | 4028 ms |
| 10 | 4.121 s | 20.047 s | 4051 ms |

开启切分 4.037~4.121 s（全部 < 5 s，每轮 `takeover_count = 1`）；`--no-split`
20.043~20.063 s（全部 ≥ 20 s）；两种方式的文件都逐字节正确。

### 第 4 项：极慢分块（960 KiB / 3 连接 × 320 KiB；第 0 号分块 10 KiB/s）

| 轮次 | 开启切分 | `--no-split` |
|---|---|---|
| 5 | 6.075 s | 31.999 s |
| 6 | 6.100 s | 31.796 s |
| 7 | 6.084 s | 32.123 s |
| 8 | 6.113 s | 32.011 s |
| 9 | 6.060 s | 31.984 s |
| 10 | 6.067 s | 32.053 s |

（10 轮全部通过；上表是后 6 轮打印出的数值，前 4 轮只记录了通过/失败。）
开启切分约 6.1 s、`--no-split` 约 32 s，快约 5.2 倍。接管后第 0 号分块仍保留
`[acked, align_up(reserved))`（最多 64 KiB），这一小段仍走 10 KiB/s 的慢连接，
所以开启切分不是"瞬间"完成；这是 10.5 切分点公式的必然结果。

## 四、测试服务器关于连接被提前关闭的记录

`StallConfig.closed_early` / `close_after_ms` 由服务器在停住期间通过
`tokio::time::timeout(20s, sock.read(...))` 写入：只有读到对端关闭（`Ok(0)` 或错误）
才置位；服务器自身既不发数据也不主动关闭连接。

- 开启切分的 10 轮：`closed_early = true`，检测到关闭的时刻 4009~4085 ms，
  全部远早于 20 s 停住期满；与 `run_download` 的耗时（~4.1 s）一致。
- `--no-split` 的 10 轮：`closed_early = false`（第 0 号分块一直等到服务器停满
  20 s 后继续发送剩余数据才完成）。
- 第 5 项退避接管测试的服务器请求记录（10 轮一致）：
  `["bytes=0-4194303", "bytes=4194304-8388607", "bytes=2228224-4194303", "bytes=262144-2228223"]`。
  其中没有倒置/空区间请求，没有分块请求使用 `bytes=0-0`，起点 256 KiB 的请求只出现
  1 次（接管产生的新分块），说明退避中的旧分块没有再发重试请求；第 0 号分块状态为
  `Done`，文件逐字节正确。

## 五、测试总数与验收

- `cargo test --workspace --locked`：**180 passed，0 failed，4 ignored**
  （idm-cli 7 + phase1d 1、idm-core lib 70、disk_concurrency 5、phase1b 39、
  phase1c1 12、phase1c_pre 2、phase1c_resume 9、phase1c_store 11、phase1d 13、
  phase2 11；ignored 为 2 个崩溃测试 + 2 个原有 `#[ignore]`）。
- 回归：`phase2` 第 4 项 `slow_chunk_split_beats_no_split`（1.156 s vs 2.134 s）、
  第 5 项 `eight_connections_sixty_four_mib_fifty_seeds`（62.27 s，50/50 种子）、
  第 11 项与原有崩溃测试各 20 轮（`cargo test -p idm-cli --test crash_resume --
  --ignored --test-threads=1`，88.32 s）全部通过。
- `scripts/verify-workspace.ps1`：**16 个步骤全部 PASS，exit code 0**。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked --
  -D warnings`：通过，0 警告。

## 六、与任务原文不一致之处（含实现解释）

1. **新增"空闲连接定期重试切分"**（`download.rs:618/:681`）。10.5 没有提到这一点，
   但"卡住"要等分块启动满 4 秒且窗口内无数据才成立；原实现只在某个分块完成
   （连接空闲）时尝试一次切分，20 秒停住场景下初次尝试发生在 ~10 ms，之后没有
   完成事件就不会再尝试。加了 100 ms 的定时重试（仅当有空闲连接时），才可能在
   ~4.0 s 触发接管并在 5 s 内完成。
2. **接管方式的判断位置**。任务写"在 `select_victim` 选出分块后判断"；实现是
   在同一次状态快照里算出方式（2.1 的"先过滤再比较"需要按该方式的切分点过滤
   候选），随选择结果一起交给 `try_split`；`try_split` 再按该方式执行第 2~7 步。
   语义与顺序一致，只是结构上把判断放进了选择器。
3. **"明显慢"的边界条件**。字面上速度为 0 也低于 `avg/4`，但刚创建、还没有
   速度数据的新分块会被立刻接管成空区间，导致无限生成新分块。因此额外要求
   `speed > 0 && !is_fresh`；速度为 0 且已启动超过 4 秒的仍按"卡住"接管。
   接管还要求 `split > start`，因为写器区间不能缩成空区间。
4. **唤醒后的补充确认**（`download.rs:1489`）。10.5 只要求唤醒等待中的任务；
   若 `pool.write` 在唤醒时仍在途（acked 落后于 reserved），唤醒检查会看到
   `acked < end` 而继续等待。实现在写盘回执到达后再确认一次 `acked >= end`
   并立即以 Completed 结束，否则极慢分块场景会卡在一个永远不来的网络块上。
5. **测试服务器的 `block` 能力**。默认 64 KiB 响应块在 10 KiB/s 下会"先发 64 KiB
   再静默 6.4 秒"，被判成"卡住"而不是"明显慢"。为让第 4 项真正验证"极慢分块"
   这条触发条件，测试服务器新增可选块大小（该测试用 4 KiB），使 10 KiB/s 变成
   持续有数据的慢流。这是测试服务器能力，不是生产代码改动。
6. **既有测试的调整**（未削弱断言，只是适配新规则）：
   - `victim_selection_skips_unsplittable_chunks` 的"太小"分块从 1 MiB 改成
     200 KiB——1 MiB 在 10.5 下已经可以通过接管切分，不再是不可切分的样本；
   - `victim_selection_prefers_stuck_chunk` 增加 `mode == Takeover` 断言；
   - `chunk_summary_contains_all_columns` 补上新增的 `takeover: false` 字段；
   - 2.1 的 `stalled_first_chunk_is_split_and_beats_no_split` 按任务要求**删除**，
     由 `stalled_first_chunk_is_taken_over` 取代（服务器不再有"新请求到达就放开
     卡住连接"的机制）。
7. **"起点等于终点的请求"的判定**：`bytes=X-(X-1)`（倒置/空区间）与分块请求中的
   `bytes=0-0`（被删除的兜底分支）都算违规；Probe 的 `bytes=0-0` 是合法的 1 字节
   探测，故第 5 项用只记录非 Probe 请求的 `chunk_range_sink` 来断言。
8. Phase 2 遗留的 10.3 第 6 步 `shrink_end` 对 `ChunkNotRegistered` 记 warn 后跳过
   的偏差（见 `docs/phase2-report.md`）保持不变。
