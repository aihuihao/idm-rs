# 任务：idm-rs Phase 1b.1 —— 下载编排修正 + 真实网络验收

沿用全部工作规则（docs/prompts/phase1a.md、phase1b.md）。先把本任务原文原样保存为 `docs/prompts/phase1b1.md`，`git status` 必须干净。
本任务分两部分：第一部分是代码修正，提交后再做第二部分的真实网络验收。

---

# 第一部分：代码修正

## 一、超时（最高优先级）

1. 报告 `DownloadConfig::request_timeout()` 当前的取值和计算方式。
2. 修改为三层超时：
   - 构建 Client 时设置 `connect_timeout`（连接超时）和 `read_timeout`（两次收到数据之间的最长间隔，即空闲超时）
   - 分块下载请求和顺序下载请求不得设置总时长 timeout（reqwest 中针对单个请求设置的 `.timeout()` 是整个请求的总时长，包括读取全部响应体的时间，大分块在慢速网络下一定会因此失败）
   - 只有 Probe 请求可以保留一个总时长 timeout（它不读取正文）
3. 补测试（用本地测试服务器）：
   - 慢速但持续发送：总耗时达到 read_timeout 的 3 倍以上，但每两次发送的间隔都小于 read_timeout → 下载必须成功
   - 发送一部分后停住，停顿时间超过 read_timeout → 返回超时相关的错误，保留 `.idm-part`

## 二、协作式取消，杜绝写入器和文件句柄泄漏

1. 每次下载创建一个内部 `CancelSignal`。外部取消、任意分块出错、任意分块 panic 时，都先触发这个内部信号。分块任务改为订阅内部信号；外部信号到来时转发给内部信号。
2. 触发内部信号后，等待所有分块任务自行退出，最多等 5 秒。分块退出时会走到 `pool.release`。只有超过 5 秒才 `abort_all`。
3. `DiskWriterPool` 新增 `purge(&TargetFile)`：移除该文件在所有落盘线程中残留的写入器，并返回移除的数量。每次下载以非 Completed 结果结束时都要调用一次，作为兜底；移除数量大于 0 时输出 warn 日志。
4. 补测试：在"下载进行中取消"、"中途断开"、"对 Range 请求返回 200"三种情况下，`download` 返回后立即执行：
   - `std::fs::rename` 把 `.idm-part` 改为其他名字，必须成功（证明没有残留的文件句柄）
   - 改回原名后，用 `TargetFile::open_existing` 打开，重新申请与原来相同的分块区间，必须成功（证明区间已注销）

## 三、改名不得覆盖已有文件

1. 最终改名前，检查最终文件名是否已存在。如果已存在，用 `resolve_conflict` 重新选一个名字，并输出 warn 日志。
2. 改名改为"不覆盖"的方式：在 win32.rs 中封装 `MoveFileExW`，不带 `MOVEFILE_REPLACE_EXISTING` 标志，目标已存在时返回错误。不再使用 `std::fs::rename`（它在 Windows 上会直接覆盖同名文件）。
3. 补测试：下载过程中，在目标目录手动创建一个同名文件，下载完成后：用户创建的那个文件内容不变；下载结果被保存为 `name (1).ext`。

## 四、不再创建空的 .idm-part

1. 删除 `ensure_part_file`。只有在 `TargetFile` 已经创建成功之后，才存在 `.idm-part`。
2. 以下情况不在磁盘上留下任何文件：Probe 之前取消、Probe 失败、Probe 之后建文件之前取消、创建文件时磁盘已满。
3. 已经写入过数据的 `.idm-part`，在失败、取消、磁盘满时都照常保留。
4. 修改相应的现有测试：断言上面这几种情况下目录里没有残留文件。

## 五、文件名冲突要考虑 .idm-part

`resolve_conflict` 判断名字是否被占用时，`name.ext` 或 `name.ext.idm-part` 任一存在都算已占用。补测试：两个任务同时下载同一个 URL，两者都能成功，生成两个不同名字的文件，内容都正确。

## 六、小改进

只有弱 ETag 但有 Last-Modified 时，用 Last-Modified 作为 If-Range；同时继续校验每个分块响应中的弱 ETag。

## 七、CLI 新增测试用参数

`idm-cli download` 新增可选参数 `--cancel-after <秒>`：到达指定秒数后，触发与 Ctrl+C 完全相同的取消路径（同一个 CancelSignal、同样的退出码、同样的输出）。在帮助文本中注明"用于测试"。不得为它另写一套取消逻辑。

## 八、第一部分的提交

1. 验收脚本全部 PASS，exit code 0；运行 1GB 压力测试
2. 只暂存 `docs crates scripts Cargo.lock`，提交信息 `phase1b.1: timeouts, cooperative cancel, safe rename`，不得 push

---

# 第二部分：真实网络验收

**规则**：本部分只做测量和记录，不修改代码。任何一项结果不符合预期时，立即停止，报告完整的原始输出，等我确认后再处理。

## 1. 准备

- `cargo build --release -p idm-cli`，之后统一使用 `.\target\release\idm-cli.exe`
- 测试目录：`D:\dl-test`（放在仓库外）。开始前如果该目录已存在且非空，先列出其中内容并停止，等我确认

## 2. 选择测试文件

从国内开源镜像站（mirrors.tuna.tsinghua.edu.cn、mirrors.ustc.edu.cn、mirrors.aliyun.com 任选）挑一个 Linux 发行版的 ISO 文件，要求：
- 大小在 1GB 到 4GB 之间
- 同一目录下有官方校验文件（`SHA256SUMS` 或 `sha256sum.txt` 等），能查到这个文件的官方 SHA256 值
- 用 `idm-cli download` 的 Probe 输出确认：支持 Range、总长已知
- 单连接下载耗时应超过 60 秒（用来验证超时修正）。如果单连接 60 秒内就下完了，换一个更大的文件，或者换一个较慢的镜像站

记录：URL、文件大小、官方 SHA256、Probe 输出（ETag、Last-Modified、是否支持 Range）。

## 3. 验收项目

**A. 单连接下载**
```
idm-cli.exe download "<URL>" --dir D:\dl-test --connections 1
```
记录完整的结束汇总、耗时、平均速度。

**B. 8 连接下载**（同一个 URL、同一个目录）
```
idm-cli.exe download "<URL>" --dir D:\dl-test --connections 8
```
预期：文件自动命名为 `xxx (1).iso`。记录结束汇总、耗时、平均速度。

**C. 哈希校验**
```
Get-FileHash D:\dl-test\*.iso -Algorithm SHA256
```
预期：两个文件的 SHA256 都与官方值一致（忽略大小写）。

**D. 取消**
```
idm-cli.exe download "<URL>" --dir D:\dl-test --connections 8 --cancel-after 15
```
预期：
- 退出码非 0
- 目录中存在 `.idm-part` 文件，且大小大于 0
- 进程退出后立即执行 `Rename-Item` 把它改名，再 `Remove-Item` 删除，两者都必须成功

**E. 两个进程同时下载同一个 URL**

用 `Start-Process` 同时启动两个进程，各用 `--connections 4`，等两者都结束。预期：
- 两者都成功
- 生成两个不同名字的文件
- SHA256 都与官方值一致

**F. 清理检查**

全部完成后，`D:\dl-test` 中不得残留任何 `.idm-part` 文件或 0 字节文件。

## 4. 记录与清理

1. 把以上全部结果写入 `docs/phase1b-network-acceptance.md`，包括：
   - 测试时间、测试文件信息、镜像站
   - A 到 F 各项的命令、关键输出、耗时、平均速度、哈希值、是否通过
   - 单连接与 8 连接的速度对比
2. 记录完成后，删除 `D:\dl-test` 中所有下载得到的文件
3. 只暂存 `docs`，提交信息 `phase1b: real network acceptance`，不得 push

## 5. 最终报告

- 第一部分：修改前 request_timeout 的取值；每一项修改对应的文件和测试名；win32.rs 新增的 unsafe 块及其 SAFETY 理由；测试数量；压力测试耗时；验收脚本输出
- 第二部分：A 到 F 的结果汇总表
- 两次提交的 hash
- 与本文不一致之处（没有就写"无"）
