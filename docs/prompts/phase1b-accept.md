# 任务：Phase 1b 收尾 —— 下载限速 + 限速条件下的真实网络验收

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase1b-accept.md`，`git status` 必须干净。
**核心要求：本任务全程不得占满网络带宽。** 任何一次真实网络下载都必须带限速参数运行，不允许出现不限速的真实网络下载。

## 第一部分：实现下载限速（不访问外网）

1. 在 `crates/idm-core/src/net/ratelimit.rs` 实现令牌桶限速器 `RateLimiter`：
   - `RateLimiter::new(bytes_per_sec: u64)`；桶容量为 1 秒的配额，最小 256 KiB
   - `async fn acquire(&self, n: u64)`：令牌不足时用 `tokio::time::sleep` 等待；不新增任何依赖
   - 用 `Arc` 共享：同一次下载的所有分块共用一个限速器，限制的是这次下载的**总速度**，而不是每个连接各自的速度
2. 接入下载流程：`DownloadOptions` 新增 `max_speed: Option<u64>`（字节/秒），`None` 表示不限速。每个分块从网络读到一块数据后、交给线程池写入之前，先调用 `acquire(数据长度)`。读取被延后后，TCP 接收窗口会被填满，服务器端随之放慢发送，所以能真正限制网络流量。等待令牌时也要能响应取消信号
3. CLI：`idm-cli download` 新增 `--max-speed <值>`，支持 `500K`、`3M` 这样的写法（K=1024，M=1024×1024）。格式非法时报错，退出码 2
4. 测试（只用本地测试服务器）：
   - 限速 2 MiB/s 下载 10 MiB，耗时在 4.25 到 5.75 秒之间（理论值 5 秒，允许 ±15%）
   - 8 个连接、总限速 2 MiB/s，总速度同样受限（证明是总限速，不是每个连接各自限速）
   - 限速状态下取消，能在 2 秒内返回 `Cancelled`
   - 不限速时行为与之前完全一致（现有测试全部通过）
5. 验收脚本全部 PASS 后，只暂存 `docs crates Cargo.lock`，提交信息 `phase1b: download rate limiter`，不得 push

## 第二部分：改造网络验收脚本

`scripts/network-acceptance.ps1`：
1. 新增**必填**参数 `-MaxSpeed`（例如 `3M`）。不提供时脚本拒绝运行
2. A、B、D 的每次下载都加 `--max-speed <MaxSpeed>`
3. E 项两个进程同时运行，每个进程的限速取 `MaxSpeed` 的一半，保证两者合计不超过 `MaxSpeed`
4. 每项下载结束后，计算平均速度。如果超过限速值的 120%，该项判 FAIL，并在报告中注明"限速未生效"
5. 报告中写明本次使用的限速值
6. 用本地回环服务器把改造后的脚本完整跑一遍自测（不访问外网），确认 A 到 F 全部能正常走通
7. 只暂存 `docs scripts`，提交信息 `phase1b: acceptance script with speed cap`，不得 push

## 第三部分：限速条件下的真实网络验收

**本部分只做测量和记录，不修改代码。** 任何一项不符合预期时，立即停止并报告完整原始输出。

1. 准备：`git status` 干净；用 `cargo build --release -p idm-cli --offline` 编译（`--offline` 保证编译过程不访问网络。如果因为缺少依赖缓存而失败，停止并报告）；删除 `D:\dl-test` 和 `D:\dl-test2`
2. 用上一轮验证过的 WMI 后台方式运行（先 `cd /d D:\IDM_RUST`），每 2 到 3 分钟查看一次日志，直到结束。不设时间上限，不得中途结束进程：
   ```
   powershell -ExecutionPolicy Bypass -File .\scripts\network-acceptance.ps1 -Url "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso" -Sha256 "a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355" -MaxSpeed 3M
   ```
   如果返回 404，停止并报告，不要自行更换链接
3. 真实 Ctrl+C 测试（G 项）：
   - 在仓库外的临时目录写辅助脚本，用 `Add-Type` 内嵌 C# 调用：`FreeConsole` → `AttachConsole(目标PID)` → `SetConsoleCtrlHandler(NULL, TRUE)` → `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`
   - 用 `Start-Process` 让 idm-cli 在独立的控制台窗口中运行（不重定向输出）：
     ```
     .\target\release\idm-cli.exe download "<同一链接>" --dir D:\dl-test2 --connections 8 --max-speed 3M
     ```
   - 15 秒后发送 Ctrl+C。检查：进程在 10 秒内退出，退出码非 0；存在 `.idm-part`；立即对它 `Rename-Item` 再 `Remove-Item`，两步都成功
   - 如果无法附加控制台，如实报告原因并跳过，这一项由我手动完成。不得用 `Stop-Process` 或 `--cancel-after` 代替
4. 把 G 项结果追加到 `docs/phase1b-network-acceptance.md`；删除 `D:\dl-test2` 和临时辅助脚本；确认 `D:\dl-test` 只剩 logs 目录
5. 只暂存 `docs`，提交信息 `phase1b: real network acceptance`，不得 push

## 报告

- 第一部分：RateLimiter 的设计说明；新增测试名及实测耗时
- 第二部分：脚本自测结果
- 第三部分：A 到 G 结果汇总表（PASS/FAIL、耗时、平均速度、是否在限速以内）；单连接与 8 连接的速度对比；两个文件的 SHA256 与官方值的比对结果
- 三次提交的 hash
- 与本文不一致之处（没有就写"无"）
