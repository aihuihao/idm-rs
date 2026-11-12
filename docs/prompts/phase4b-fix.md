# 任务：Phase 4b 修正 + 端到端冒烟测试（不需要 Firefox）

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase4b-fix.md`，`git status` 必须干净。
全程所有下载只用本地回环服务器，不访问外网；不安装到默认的 `idm_rs` 名之外的任何位置，冒烟测试用随机 host 名并在结束时清理。

## 一、修改 extensions/firefox/background.js
1. handoff 失败（catch 分支）时执行 `recent.delete(url)`，使用户可以立即重试
2. 失败后 30 秒内不再尝试接管：记录 `lastFailAt`，30 秒内到来的新下载直接放行（不调用 ping），徽标保持 "!"；30 秒后恢复尝试。成功一次后清除失败状态
3. `downloads.cancel` 之后，在 try/catch 中调用 `downloads.removeFile({id})`（失败忽略），再 `downloads.erase`
4. 其余逻辑不变：只有 `reply.ok === true` 之后才允许 cancel；Cookie 值不得写入控制台

## 二、README 与静态检查
1. `extensions/firefox/README.md` 写明：扩展固定使用 host 名 `idm_rs`；用 `nm install` 安装给浏览器用时不要改 `--host-name`（自定义名仅用于测试）；加载步骤、卸载步骤、常见问题
2. `phase4b-acceptance.ps1` 的扩展静态检查增加：background.js 中 `downloads.cancel` 只出现在 `reply.ok === true` 判断之后（文本顺序检查）；`catch` 分支里不得出现 `downloads.cancel`；包含 `removeFile`；包含 `lastFailAt`

## 三、扩展逻辑的自动测试（不依赖 Firefox）
background.js 是纯 JavaScript 但本机没有 Node，不新增依赖。写一个 PowerShell 测试，用文本与结构检查覆盖：
- 失败分支包含 `recent.delete`
- 30 秒退避常量存在
- 没有 `eval(`、`new Function`、`innerHTML`
这些检查放进验收脚本，不需要运行 JavaScript

## 四、端到端协议冒烟测试（用真实 idm-nmhost.exe 与 idm-cli.exe，模拟扩展）
写脚本 `scripts/phase4b-e2e.ps1`，流程如下，全部自动，结束时清理：
1. 创建临时目录 `%TEMP%\idm4b-e2e-<随机数>`，把 release 版 `idm-cli.exe` 与 `idm-nmhost.exe` 复制进去；用随机 host 名 `idm_rs_e2e_<随机数>` 执行 `nm install --db <临时目录\e2e.db>`，确认注册表项与清单存在
2. 启动测试用本地回环 HTTP 服务器（可用 PowerShell 的 `System.Net.HttpListener`，提供一个 3 MB 的确定性内容文件，支持 Range 请求和 Probe 的 `Range: bytes=0-0`）。若 HttpListener 需要管理员权限而失败，改用仓库内已有的 Rust 测试服务器写一个小的辅助二进制或测试，不要联网下载文件
3. 以与 Firefox 相同的帧协议，启动 `idm-nmhost.exe`（环境变量 `IDM_RS_DB` 指向临时数据库），依次发送 ping、enqueue（带 referrer、cookies、userAgent、filename），断言收到 `ok:true` 与 `taskId`
4. 等待 host 拉起的 `idm-cli queue run --watch`（它应在同目录下）开始运行，轮询临时数据库直到该任务状态为 Completed（最多 60 秒）；断言下载文件内容与服务器内容一致（对比 SHA256），`.idm-part` 不存在，`task_headers` 为空
5. 本地服务器记录收到的请求头，断言 Probe 和分块请求里都有 Referer、Cookie、User-Agent
6. 终止 `idm-cli.exe queue run --watch` 进程，等 3 秒确认 `runner.lock` 被释放
7. 再发一次 enqueue 到同一个数据库，确认 host 会再次拉起运行器，任务完成
8. `nm uninstall`，确认注册表项与清单已删除；删除临时目录；确认没有残留的 `idm-cli.exe`、`idm-nmhost.exe` 进程
9. 任何一步失败都要清理后以非零退出码结束，并打印失败步骤和原始输出

## 五、提交
1. 每个新增检查/测试单独连续运行 10 次；`phase4b-e2e.ps1` 完整运行 3 次；`verify-workspace.ps1` 16 步全 PASS
2. 只暂存 `docs crates scripts extensions`，提交信息 `phase4b-fix: retry dedup, failure backoff, e2e smoke test`，不 push，不打 tag
3. 报告：每项修改位置；e2e 三次运行的原始输出；与本文不一致之处

**说明：真实 Firefox 里点击下载的最后一步由用户手动完成，不需要你做，也不要启动 Firefox。**
