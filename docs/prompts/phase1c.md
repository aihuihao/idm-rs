# 任务：idm-rs Phase 1c —— SQLite 持久化与断点续传

沿用全部工作规则（docs/prompts/ 下的历次任务原文）。先把本任务原文原样保存为 `docs/prompts/phase1c.md`，`git status` 必须干净。
本阶段不做自动重试（1d）、不做动态分块（Phase 2）。所有真实网络下载必须带 `--max-speed 3M`。
本阶段是整个项目数据一致性的核心。凡是本文写明的顺序和规则，不得改动；如果认为有问题，先停下报告。

---

## A. 诊断能力（先做，单独提交）

1. CLI 的 `download` 结束时，无论成功还是失败，都打印每个分块的汇总表：分块编号、区间、已写入字节数、耗时、平均速度、最终状态
2. 失败时打印完整的错误链：错误本身，以及逐层的 source
3. 新增 `--log-file <路径>`：把 tracing 日志（debug 级别）同时写入该文件
4. 补测试；验收脚本 PASS 后提交：`phase1c-pre: per-chunk summary and log file`

---

## B. 写入 ARCHITECTURE.md（原样追加为新的第 9 节）

```
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
```

---

## C. 数据库（`crates/idm-core/src/store/`）

1. **独立线程**：`Store` 在一个专用线程中持有 rusqlite 连接，对外提供 async 方法，通过 mpsc 发送命令、用 oneshot 接收结果。不得在 Tokio 工作线程上执行任何 SQL。
2. **打开时**：设置 WAL、synchronous=NORMAL、busy_timeout=5000、foreign_keys=ON。
3. **迁移**：迁移脚本以数组形式维护，根据 `PRAGMA user_version` 依次执行；每条迁移在一个事务内完成，并更新 user_version。本阶段为版本 1。
4. **表结构（版本 1）**：
   - `tasks`：id（自增主键）、url、final_url、dir、file_name、part_path、total_size（可空）、supports_range、etag（可空）、etag_weak、last_modified（可空）、strategy、connections、state、pause_reason（可空）、last_error（可空）、resumable、created_at、updated_at
   - `chunks`：task_id（外键，删除任务时级联删除）、chunk_index、start、end、committed、state；主键为 (task_id, chunk_index)
5. **API**：创建任务、保存分块、写检查点（单个事务，committed 只增不减）、更新状态、读取任务、列出任务、删除任务、启动恢复（按 9.4 的规则处理）。
6. 任务 ID 改为使用数据库的自增 ID，删除进程内的 `NEXT_TASK_ID`。
7. **默认数据库路径**：`%LOCALAPPDATA%\idm-rs\idm.db`（目录不存在时自动创建）；CLI 可以用 `--db <路径>` 覆盖。

---

## D. 下载流程改造

1. **acked 跟踪**：每个分块的 acked 位置用一个 `AtomicU64` 记录，只在 `pool.write` 返回成功之后才推进。
2. **检查点**：每个任务运行一个检查点循环，严格按 ARCHITECTURE.md 9.2 执行。间隔可以配置，默认 2 秒。
3. **状态持久化**：任务的每次状态变化都写入数据库。暂停原因分为 `DiskFull`、`User`、`Interrupted` 三种。CLI 收到 Ctrl+C 或 `--cancel-after` 到时，任务改为 Paused（原因 User），而不是 Cancelled。
4. **续传 `resume(task_id)`**：
   - 状态为 Paused 或 Failed 的任务都可以续传
   - 打开 .idm-part（`open_existing`，检查长度等于 total_size）；文件不存在或长度不符 → 返回明确的错误，任务保持原状态
   - 重新执行 Probe，比较总长度、ETag、Last-Modified。有任何变化 → 返回 `RemoteChanged`，任务改为 Failed，.idm-part 保留
   - 对 committed 小于 end 的每个分块，从 committed 位置开始申请区间并发送 Range 请求，规则与 1b 相同（If-Range、严格校验 Content-Range 等）
   - 不可续传的任务（总长未知或不支持 Range）：在报告和输出中明确说明后，删除 .idm-part 并从头重新下载
5. **完成阶段**：严格按 ARCHITECTURE.md 9.4 执行。

---

## E. CLI

- `download`：创建任务前先打印任务 ID；其余行为保持不变
- `resume <id>`：续传
- `list`：列出所有任务，包括 ID、文件名、状态、进度（committed 总和 / 总长度）、最后错误
- `remove <id> [--delete-file]`：删除任务记录；带 `--delete-file` 时同时删除 .idm-part
- 所有子命令都支持 `--db`；`download` 和 `resume` 都支持 `--max-speed` 和 `--log-file`
- 启动时自动执行一次启动恢复

---

## F. 测试

**数据库**
1. 迁移：新库升级到版本 1；对已经是版本 1 的库重复执行不会出错
2. 检查点：committed 只增不减；写检查点的事务失败时，所有分块都不变
3. 两个进程（或两个 Store 实例）同时使用同一个数据库，都能正常工作

**检查点协议**
4. 通过测试钩子让 `sync` 返回错误 → 数据库中的 committed 不变
5. 通过测试钩子，验证每次写入数据库的值都不超过 sync 开始前已收到回执的位置

**续传（用本地测试服务器）**
6. 下载到一半暂停，再续传，最终文件逐字节正确
7. 续传时远端已变化（ETag 变化）→ `RemoteChanged`，.idm-part 保留
8. .idm-part 丢失或长度不符 → 明确报错，任务状态不变
9. 不可续传的任务 → 从头下载，最终文件正确
10. 失败（中途断开）后续传，最终文件正确

**崩溃测试（最重要）**
11. 在测试中启动 `idm-cli download` 子进程，下载本地测试服务器提供的慢速文件。等数据库显示进度达到一个随机比例后，用 `Child::kill` 强制结束进程。然后：
    - 先校验：每个分块在 [start, committed) 范围内的字节，必须与期望内容逐字节一致
    - 再执行 `idm-cli resume`，最终文件逐字节正确
    - 用不同的随机强杀时机，重复 20 次
    - 说明：强制结束进程不会丢失操作系统缓存里的数据，所以这个测试验证不了断电的情况。断电安全靠测试 4 和 5 验证协议顺序来保证。这一点要在报告中写明
12. 在 Finalizing 的每个阶段构造中断场景（重命名前、重命名后但数据库还没更新），启动恢复后结果都符合 9.4 的规则

**原有测试**全部继续通过。

---

## G. 真实网络验收（全程带 `--max-speed 3M`）

使用 Debian ISO 链接（见 docs/phase1b-network-acceptance.md），用 WMI 后台方式运行，每 2 到 3 分钟查看一次：

1. 用 `--connections 8 --max-speed 3M --log-file <路径>` 开始下载。进度到 30% 左右时，用 `Stop-Process -Force` 强制结束进程
2. `idm-cli list` 确认任务状态为 Paused（原因 Interrupted），进度约 30%
3. `idm-cli resume <id> --max-speed 3M` 续传完成，SHA256 与官方值一致
4. 再下载一次，到 50% 左右时用上一轮的方法发送真实 Ctrl+C。确认状态为 Paused（原因 User），然后续传完成，SHA256 正确
5. 把结果写入 `docs/phase1c-network-acceptance.md`，包括续传前后的进度、耗时、哈希值，以及第 1 步下载结束时打印的分块汇总表
6. 删除下载得到的文件

---

## H. 提交与报告

- 第 A 部分完成后单独提交；B 到 F 完成后提交 `phase1c: sqlite persistence and resume`；G 完成后提交 `phase1c: real network acceptance`。只暂存 `docs crates scripts ARCHITECTURE.md Cargo.lock`，不得 push
- 报告：
  - 数据库表结构
  - 检查点协议在代码中的位置（文件:行号），以及对应的测试名
  - 启动恢复的规则实现
  - 测试数量与结果；崩溃测试 20 次的结果
  - G 部分的结果
  - 与本文不一致之处
