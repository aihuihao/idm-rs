# 任务：Phase 3 整合验收（轻量版，真实网络）

沿用全部工作规则。上一次整合验收因流量过大被用户手动终止。先把本任务原文保存为 `docs/prompts/phase3-accept-lite.md`。本任务只做测量和记录，不修改代码。任何一项不符合预期，停止并报告完整原始输出。
本任务总下载量不得超过 250 MB，所有下载必须带限速。

## 准备
1. `git status` 干净；确认没有残留的 `idm-cli.exe` 进程（有则列出并停止，等我确认）
2. 删除 `D:\dl-test`；上一次验收遗留的数据库任务不要复用：本任务全程使用独立数据库 `--db D:\dl-test\accept.db`
3. 选测试文件：从 mirrors.tuna.tsinghua.edu.cn 或 mirrors.ustc.edu.cn 找一个 **20~40 MB**、同目录有官方校验值的文件（例如某个较小的软件包）。记录 URL、大小、官方 SHA256，并确认支持 Range、总长已知
4. 所有日志文件用 UTF-8 写出，读取时显式指定编码

## A. 队列 + 全局限速 + 主机连接限制
- 用 `idm-cli enqueue` 把同一个 URL 入队 3 次，每个 `--connections 4`，`--dir D:\dl-test`（文件名由冲突处理自动生成，不要使用 `--out`）
- `idm-cli queue run --max-tasks 2 --global-max-speed 1M --max-host-connections 6 --log-file D:\dl-test\queue.log`
- 预期：任何时刻活动任务不超过 2 个；第 3 个任务在前面有任务完成后自动开始；3 个文件 SHA256 都与官方值一致；总平均速度不超过 1M 的 120%
- 记录：每个任务的开始和结束时间、峰值并发任务数、总耗时、总平均速度
- **诊断**：每 10 秒记录一次各任务的 `downloaded` 数字。如果任何一个任务的 `downloaded` 连续 60 秒不变，立即保存此刻 `queue.log` 的最后 200 行以及 `idm-cli list` 的输出，并继续等待（不要终止），记录它最终是恢复了还是一直卡住

## B. 中途中断与续传
- 新入队 1 个任务（同一 URL，`--connections 4 --max-speed 1M`），用 `idm-cli download` 或 `queue run` 启动均可
- 下载进行约 10 秒后，用 Ctrl+C 辅助脚本（`AttachConsole` + `GenerateConsoleCtrlEvent`，方法同 Phase 1b 的 G 项）发送真实 Ctrl+C
- 确认状态为 Paused(User)，用 `idm-cli resume <id> --max-speed 1M` 续传完成，SHA256 一致
- 无法发送 Ctrl+C 时如实报告原因并跳过，不要用 Stop-Process 代替

## C. 代理
- 若本机没有可用的 HTTP 或 SOCKS5 代理，如实报告并跳过
- 验证 `--proxy` 与 `--no-proxy` 同时传入时退出码为 2（上一次已验证，可复用结果并注明）

## 记录与提交
1. 结果写入 `docs/phase3-network-acceptance.md`（命令、关键输出、耗时、速度、哈希、是否通过，以及 CLI 的两个缺口：`enqueue --out` 被忽略、`queue` 没有 `pause` 命令）
2. 删除下载文件，保留日志；只暂存 `docs`，提交信息 `phase3: integration acceptance`，不得 push
3. 报告：A、B、C 结果汇总表；任务是否出现长时间不动及其原因；与本文不一致之处
