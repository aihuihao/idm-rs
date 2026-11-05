# 任务：idm-rs Phase 1c.1 —— 持久化边界路径与多进程安全修正

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase1c1.md`，`git status` 必须干净。
本轮不需要真实网络验收。检查点协议（ARCHITECTURE.md 9.2）的顺序保持不变。

## 一、所有退出路径都必须收尾（必须修）

现状：`download_with_store` 中以下路径在有 ctx 时没有更新数据库状态，其中第 5 条还会遗留检查点后台任务（每 2 秒执行一次，并一直持有 TargetFile）：
1. Probe 之前取消
2. Probe 之后、建文件之前取消（两处）
3. 建文件时磁盘满
4. 建文件出错（包括顺序下载路径中的 `TargetFile::create(...)?`）
5. 分块启动循环中取消（`aborted_before_all_spawned` 分支）

要求：
- 整理代码，让所有退出路径都经过同一个收尾函数，不允许出现绕过它的 `return` 或 `?`
- 收尾函数负责：停止检查点任务，并 **await 它真正结束**（不能只调用 abort）；停止外部取消的转发任务；在目标文件已创建时执行最后一次检查点；写入最终的数据库状态；非 Completed 时调用 purge
- 各路径的数据库状态：

| 路径 | 数据库状态 | 磁盘上的文件 |
|---|---|---|
| 1、2（建文件之前取消） | Paused(User)，part_path 为空 | 无 |
| 3（建文件时磁盘满） | Paused(DiskFull)，part_path 为空 | 无 |
| 4（建文件出错） | Failed，记录错误 | 无 |
| 5（分块启动中取消） | 先执行最后一次检查点，再改为 Paused(User) | 保留 .idm-part |

- 对 part_path 为空的 Paused 或 Failed 任务执行 resume 时，重新执行完整流程（现有逻辑已支持，确认测试覆盖）
- 测试：上表每条路径一个测试，断言数据库状态和磁盘文件都符合上表。另外，对第 5 条以及"下载进行中取消"，在 download 返回后等待 3 个检查点间隔，断言该任务的 `updated_at` 没有再变化（以此证明检查点任务确实已经停止）

## 二、最后一次检查点失败时不得完成（必须修）

- 在完成路径上，最后一次检查点必须成功，才能进入 Finalizing
- 如果失败：失败原因是磁盘满 → Paused(DiskFull)；其他原因 → Failed 并记录错误。两种情况都保留 .idm-part，不改名
- 测试：从下载开始就设置 `fail_sync` 钩子，让所有检查点都失败。数据全部下载完后，最终结果必须是 Failed（不是 Completed），.idm-part 保留，数据库中的 committed 仍然等于各分块的 start。然后清除钩子，执行 resume，必须能完整下载完成，最终文件逐字节正确

## 三、多进程所有权：用文件共享模式实现（必须修）

1. TargetFile 创建和打开 .idm-part 时，通过 `std::os::windows::fs::OpenOptionsExt::share_mode` 设置为只允许其他程序读（FILE_SHARE_READ = 0x1），不允许写和删除。不需要写 unsafe
2. 启动恢复（recover）处理 Downloading 任务时，先尝试以读写方式打开它的 .idm-part：
   - 打开失败且错误码为 32（ERROR_SHARING_VIOLATION）→ 文件正被另一个进程使用，跳过这个任务，状态不变
   - 打开成功，或文件不存在 → 按原规则改为 Paused(Interrupted)。打开成功后立即关闭文件
   - RecoveryReport 新增"跳过的数量"
3. resume 打开 .idm-part 时遇到错误码 32 → 返回新的错误变体 `TaskInUse { id }`，数据库状态不变
4. 确认 1b.1 中"改名成功证明句柄已释放"的那些测试，在新的共享模式下依然通过（现在这些测试才真正有意义）
5. 把下面这段原样追加到 ARCHITECTURE.md，作为 9.5 节：

```
### 9.5 多进程所有权
- 一个任务的 .idm-part 同一时间只能被一个进程以可写方式打开：TargetFile 使用 FILE_SHARE_READ 共享模式。
- 启动恢复只把"无人占用"的 Downloading 任务改为 Paused(Interrupted)。.idm-part 被占用（错误码 32）说明另一个进程正在下载，跳过这个任务。
- resume 遇到被占用的 .idm-part 时返回 TaskInUse，数据库状态不变。
- 所有文件句柄都必须在改名之前释放；由于共享模式不允许删除，只要还有句柄没释放，改名就会失败。
```

6. 测试：
   - 一个任务正在下载时，用另一个 Store 实例执行 recover，该任务状态不变，"跳过的数量"为 1
   - 对正在下载的任务执行 resume → 返回 TaskInUse，数据库状态不变
   - 下载过程中，另一个句柄以只读方式打开 .idm-part 能成功，以读写方式打开失败

## 四、其他修正

1. **finish_finalizing 的文件名冲突**：与正常下载流程保持一致。最终文件名被占用时，用 `resolve_conflict` 另选一个名字，并输出 warn 日志，不要直接报错。补测试
2. **Pending / Probing 状态的残留任务**：recover 时，状态为 Pending 或 Probing、且 `updated_at` 距今超过 5 分钟的任务，改为 Failed，记录错误"interrupted before download started"（Probe 的总超时只有 60 秒，正在运行的进程不可能在这两个状态停留 5 分钟）。补测试
3. **原子操作的内存顺序**：acked 的 `fetch_add` 改用 `Release`，检查点读取快照改用 `Acquire`，并在注释中说明理由

## 五、1c 遗留问题

1. **会随机失败的测试**：`cancel_during_download` 不要再用固定的 200 毫秒延迟。改为等到进度显示已经写入数据（或者 .idm-part 已经存在）之后再取消。修复后，在 4 线程并行和 1 线程两种方式下，各连续运行 20 次，全部通过才算修好，并报告运行结果
2. **F.7 改用真实事件测试**：给测试服务器加一个运行期间可以切换 ETag 的开关（用原子变量或 Mutex 实现）。测试流程：下载到一半暂停 → 切换 ETag → resume → 必须得到 RemoteChanged，.idm-part 保留
3. **F.10 改用真实事件测试**：给测试服务器加"只对前 N 个响应在第 M 个字节处断开"的模式。测试流程：真实地中途断开导致下载失败 → resume → 最终文件逐字节正确
4. **崩溃测试纳入验收脚本**：在验收脚本中新增一步，单独运行 `crash_resume` 测试（加 `--ignored --test-threads=1`），失败即判 FAIL

## 六、提交与报告

1. 验收脚本全部 PASS，exit code 0
2. 只暂存 `docs crates scripts ARCHITECTURE.md Cargo.lock`，提交信息 `phase1c.1: exit-path cleanup, ownership via share mode, final-checkpoint gate`，不得 push
3. 报告：
   - 收尾函数的位置，以及每条退出路径与它的对应关系
   - 第一到第五部分每一项对应的文件、行号和测试名
   - 第五部分第 1 项两种运行方式的 20 次结果
   - 测试总数
   - 验收脚本输出
   - 与本文不一致之处
