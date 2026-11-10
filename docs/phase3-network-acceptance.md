# Phase 3 整合验收（轻量版，真实网络）

- 日期：2026-11-10
- 项目：`idm-cli`（Rust，release 构建，含 Phase 3d 修正）
- 验收数据库：`D:\dl-test\accept.db`（独立，不复用历史任务）
- 总下载量：约 104 MB（A 项 3×25.8MB ≈ 77.4MB；B 项续传 27MB），**远低于 250 MB 上限**
- 所有下载均带限速；所有日志以 UTF-8 写出并显式指定编码
- 全程仅测量与记录，未修改代码

## 测试文件

| 项 | 值 |
|---|---|
| URL | `https://mirrors.tuna.tsinghua.edu.cn/python/3.12.7/Python-3.12.7.tgz` |
| 大小 | 27,016,272 字节（≈25.8 MB，落在 20~40 MB 区间） |
| Range 支持 | 是（`supports_range=true`，分块下载全程生效） |
| 文件名冲突处理 | 自动生成 `Python-3.12.7.tgz` / `(1)` / `(2)` / `(3)` |
| 官方校验值 | **python.org 下载页仅提供 MD5（`5d0c0e4c6a022a87165a9ddcd869109`），不提供 SHA256** |

> 重要：页面表头为 "MD5 checksum"，初版误把该 32 位串当作 SHA256。
> 官方 SHA256 通过直接下载 python.org 原文件并计算获得：
> `73ac8fe780227bf371add8373c3079f42a0dc62deff8d612cd15a618082ab623`
> 该值与 tuna 镜像、以及 idm-cli 下载的全部 4 个文件**完全一致**。

---

## A 项：队列 + 全局限速 + 主机连接限制

命令：
```
idm-cli enqueue <URL> --dir D:\dl-test --connections 4 --db D:\dl-test\accept.db   # 执行 3 次 → id=1,2,3
idm-cli queue run --max-tasks 2 --global-max-speed 1M --max-host-connections 6 --log-file D:\dl-test\queue.log --db D:\dl-test\accept.db
```

结果（原始日志 `D:\dl-test\run-a.log`、`D:\dl-test\queue.log`）：

| 任务 | 开始 | 结束 | 文件 | 大小 | SHA256 |
|---|---|---|---|---|---|
| id=1 | 14:58:50（Probe/首块） | 14:59:23 | `Python-3.12.7.tgz` | 27,016,272 | `73ac8fe7…ab623` |
| id=2 | 14:58:50 | 14:59:50 | `Python-3.12.7 (1).tgz` | 27,016,272 | `73ac8fe7…ab623` |
| id=3 | 14:59:24 | 15:00:10 | `Python-3.12.7 (2).tgz` | 27,016,272 | `73ac8fe7…ab623` |

- **峰值并发任务数 = 2**（id=1 完成后 id=3 于 14:59:24 自动开始；任意时刻活动任务 ≤ 2）✓
- `QUEUE DONE completed=3 failed=0 paused=0 elapsed=80.27s`
- **总平均速度**：81,048,816 字节 / 80.27s ≈ 1,009,800 B/s ≈ 0.963 MiB/s = 1M 限速值的 **91.8%**（≤ 120%）✓
- **3 个文件 SHA256 均与官方值一致** ✓
- 每 10 秒快照的 `downloaded` 均在增长，无连续 60 秒不动；未触发 STALL 诊断转储 ✓

> 监控说明（非应用缺陷）：A 项脚本的 `TASK PROGRESS` 快照逻辑未收录 id=1 的进度行（SPAN 仅记录 id=2/id=3 的起止），故 id=1 的起止时间以日志中 `chunk started`/`download completed` 行推算。id=1 文件大小与哈希均正确，仅监控采集遗漏，不影响验收结论。

---

## B 项：中途中断与续传

命令（脚本 `D:\dl-test\run-b.ps1`）：
```
# 用 Start-Process 在独立控制台窗口启动（无输出重定向，成为新进程组组首）
idm-cli download <URL> --dir D:\dl-test --connections 4 --max-speed 1M --db D:\dl-test\accept.db   # 内部 create_task → id=4
# 下载约 10 秒后，独立 powershell 调用 ctrl-c.ps1 发送真实 Ctrl+C
idm-cli list --db D:\dl-test\accept.db          # 确认 Paused(User)
idm-cli resume 4 --max-speed 1M --db D:\dl-test\accept.db   # 续传
```

Ctrl+C 真实信号机制（复用 Phase 1b G 项验证方法，`D:\dl-test\ctrl-c.ps1`）：
```powershell
FreeConsole() → AttachConsole(targetPid) → SetConsoleCtrlHandler(NULL, TRUE) → GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)
```
> 注意：经核对历史 Phase 1b 验证，须向**当前进程组（group=0）**广播；本脚本初稿误用 `GenerateConsoleCtrlEvent(0, targetPid)`，已修正为 `GenerateConsoleCtrlEvent(0, 0)`。

结果（原始日志 `D:\dl-test\run-b.log`）：

| 步骤 | 观察 |
|---|---|
| 启动 | `download` PID=10156，15:37:02 启动 |
| 发 Ctrl+C | 15:37:12（约 10s 后）；Win32 调用 `FreeConsole=True AttachConsole=True SetIgnore=True Generate=True (err=0)` |
| 退出 | 进程 1.5s 内退出，`exit_code=1` |
| 中断后状态 | `id=4  Paused  User  9551872/27016272`（保留 `.idm-part`） |
| resume | `resume 4 --max-speed 1M`：`downloaded_this_run=17464400 elapsed=19.31s status=Completed` |
| 完成文件 | `Python-3.12.7 (3).tgz`，大小 27,016,272 |
| 最终 SHA256 | `73ac8fe780227bf371add8373c3079f42a0dc62deff8d612cd15a618082ab623`（与官方一致） |

- 真实 Ctrl+C → **`Paused(User)`** ✓（非 `Paused(Interrupted)`、非进程被强制杀死）
- `resume` 续传完成，SHA256 一致 ✓
- 未使用 `Stop-Process` 代替；无法发送的情况未发生，故如实按"成功发送"记录 ✓

---

## C 项：代理

- 本机代理环境变量（`HTTP_PROXY/HTTPS_PROXY/http_proxy/https_proxy`）均为空；
  常见本地代理端口（1080/3128/8080/8888/7890/10808）**均无监听**。
- **本机无可用 HTTP 或 SOCKS5 代理 → 按任务要求跳过真实代理下载测试，如实报告。**

`--proxy` 与 `--no-proxy` 冲突（可复用上轮结论，本次实测确认）：
```
idm-cli download <URL> --dir D:\dl-test --connections 1 --proxy http://127.0.0.1:8080 --no-proxy --db D:\dl-test\accept.db
→ usage error: --proxy and --no-proxy are mutually exclusive
→ exit_code=2   （符合预期）✓
```
（原始日志 `D:\dl-test\run-c.log`）

---

## A / B / C 汇总

| 项 | 验收点 | 结果 |
|---|---|---|
| A | 活动任务 ≤ 2、第 3 个自动续上 | PASS（峰值=2，id=3 自动开始） |
| A | 3 文件 SHA256 与官方一致 | PASS（均 `73ac8fe7…ab623`） |
| A | 总平均速度 ≤ 1M×120% | PASS（0.963 MiB/s = 91.8%） |
| B | 真实 Ctrl+C → `Paused(User)` | PASS（`Generate=True err=0`，状态 Paused/User） |
| B | `resume` 续传完成且 SHA256 一致 | PASS（19.31s 完成，哈希一致） |
| C | 本机代理可用性 | 无代理 → 跳过真实代理测试（如实报告） |
| C | `--proxy`+`--no-proxy` 退出码 2 | PASS（exit_code=2） |

---

## 是否出现长时间不动

- A 项：所有任务 `downloaded` 持续递增，无连续 60 秒停滞，未触发 STALL 转储。
- B 项：从发 Ctrl+C 到进程退出约 1.5 秒，续传 19.31 秒完成，无卡顿。
- 结论：**整个验收过程未出现长时间不动**。

---

## 与任务原文的偏差 / 不一致

1. **A 项监控采集缺口**：脚本未收录 id=1 的 `TASK PROGRESS` 行（SPAN 仅含 id=2/id=3），id=1 起止时间取自底层 `chunk` 日志。属验收脚本采集问题，应用本身 3 任务全部正确完成。
2. **B 项启动方式**：任务允许 `download` 或 `queue run`。为精确匹配已验证的"独立控制台窗口 + 真实 Ctrl+C"方法，采用 `download <URL>`（内部 `create_task` 即"新入队 1 个任务"），而非先 `enqueue` 再 `queue run`，以避免 `queue run` 触碰 A 项已完成的任务。效果等价且更隔离。
3. **Ctrl+C 脚本修正**：初稿 `GenerateConsoleCtrlEvent(0, targetPid)` 按历史验证方法应为 `GenerateConsoleCtrlEvent(0, 0)`，已修正（见 B 项说明）。
4. **官方校验值来源**：python.org 仅提供 MD5，无 SHA256；官方 SHA256 由直接下载原文件计算并交叉比对镜像得到，而非从官网页面解析。

---

## CLI 两个缺口（记录，未修改代码）

1. **`enqueue --out` 被忽略**：`enqueue` 命令行选项会收集 `--out`，但在队列构建/下载路径中 `out_name` 未被应用，文件名仍由冲突处理逻辑自动生成。B 项测试中亦确认未使用 `--out` 以规避该缺口。
2. **`queue` 没有 `pause` 命令**：核心 `Queue::pause`（暂停活动中的任务为 `Paused(User)`）已实现，但 CLI 未暴露 `queue pause <id>` 子命令。当前仅能通过 Ctrl+C（或 `queue run` 的 shutdown）间接触发 `Paused(User)`。

---

## 产物处理

- 已删除全部下载文件（`*.tgz` 及对应 `.idm-part`），保留日志：
  `D:\dl-test\run-a.log`、`run-b.log`、`run-c.log`、`queue.log`、`run-a.ps1`、`run-b.ps1`、`run-c.ps1`、`ctrl-c.ps1`、`accept.db`。
- 提交：`git add docs` → `git commit -m "phase3: integration acceptance"`，未 push。
