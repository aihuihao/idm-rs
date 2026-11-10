# 任务：Phase 3 整合验收（真实网络，全程限速）

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase3-accept.md`。本任务只做测量和记录，不修改代码。任何一项不符合预期，停止并报告完整原始输出。

## 准备
1. `git status` 干净；`cargo build --release -p idm-cli --offline`；删除 `D:\dl-test`
2. 使用 Debian ISO 链接（见 docs/phase1b-network-acceptance.md）和其官方 SHA256

## 验收项（用 WMI 后台方式运行，每 2~3 分钟查看一次）

**A. 队列 + 全局限速**
- 用 `idm-cli enqueue` 把同一个 ISO URL 入队 3 次，输出文件名分别为 a.iso、b.iso、c.iso，每个 `--connections 4`
- `idm-cli queue run --max-tasks 2 --global-max-speed 3M --max-host-connections 6`
- 预期：任何时刻活动任务不超过 2 个；3 个文件 SHA256 都与官方值一致；总平均速度不超过 3M 的 120%；排队的第 3 个任务在前面任务完成后自动开始
- 记录：每个任务的开始/结束时间、峰值并发任务数、总耗时、总平均速度

**B. 运行中暂停与恢复**
- 重新入队 1 个任务，开始后约 60 秒，用 `idm-cli queue` 的暂停能力暂停它（若 CLI 没有暂停命令，用一个 Ctrl+C 发送辅助脚本，方法同 Phase 1b 的 G 项），再用 `idm-cli resume` 续传完成，SHA256 一致
- 如果 CLI 缺少暂停命令，如实报告，不要改代码

**C. 代理**
- 本机用测试代理不方便时，如果本机没有可用的 HTTP 或 SOCKS5 代理，如实报告并跳过。有的话：`--proxy <URL> --max-speed 3M` 下载一次，SHA256 一致
- 另外验证 `--proxy` 与 `--no-proxy` 同时传入时退出码为 2

## 记录与提交
1. 结果写入 `docs/phase3-network-acceptance.md`（命令、关键输出、耗时、速度、哈希、是否通过）
2. 删除下载文件；只暂存 `docs`，提交信息 `phase3: integration acceptance`，不得 push
3. 报告：A、B、C 的结果汇总表；与本文不一致之处
