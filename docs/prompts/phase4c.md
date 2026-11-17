# 任务：idm-rs Phase 4c —— 运行器脱离浏览器生命周期

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase4c.md`，`git status` 必须干净。全程不访问外网，所有下载只用本地回环服务器。遇到不明确之处先停下报告。

## 背景
Firefox 每次 sendNativeMessage 都会新起一个 idm-nmhost 进程，回复后即关闭。host 拉起的运行器与 host 同属 Firefox 的作业对象，随 host 一起被终止，导致任务永远停在 Queued。本阶段改为：运行器独立常驻，host 不再拉起任何进程。

## 一、清理现场（只做一次）
默认数据库里有 4 条真实验收留下的 Queued 任务（搜狗拼音、腾讯会议）。列出后执行 `idm-cli remove <id>` 全部删除，不下载、不启动运行器。报告删除前后的 `idm-cli list`。

## 二、常驻运行器管理（idm-cli runner 子命令）
1. `runner install [--name <值名>]`：在 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 写入值（默认名 `idm-rs-runner`），内容为 `"<idm-cli.exe 绝对路径>" queue run --watch --db "<数据库绝对路径>"`。用 reg.exe，只写 HKCU。`--name` 仅供测试
2. `runner uninstall [--name]`：删除该值
3. `runner status`：打印是否已注册开机启动、运行器当前是否在运行（以 runner.lock 是否被占用判断）
4. `runner start`：运行器未运行时，以 `DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` 启动 `queue run --watch`；先附加 `CREATE_BREAKAWAY_FROM_JOB` 尝试，若因拒绝访问失败则去掉该标志重试。启动后 3 秒内确认 runner.lock 已被占用，否则报错退出（退出码非 0）
5. `runner stop`：在数据目录写入 `runner.stop` 文件；`--watch` 循环每秒检查该文件，发现后按现有取消流程安全停止所有活动任务（Paused(User)），删除该文件后退出。stop 命令等待最多 10 秒确认锁已释放
6. 运行器启动抢锁时，若锁被占用，先每 200 毫秒重试，共 3 秒，仍占用再以退出码 3 退出（避免与 host 的瞬时探测撞车）
7. watch 模式写运行日志 `runner.log`（数据目录，info 级别，单文件 1 MiB 覆盖重写），记录启动、停止、任务开始与结束；不得出现 Cookie 值与完整 URL 的查询参数
8. `nm install` 成功后打印提示：请执行 `idm-cli runner install` 与 `idm-cli runner start`（不自动执行）

## 三、host 不再拉起运行器
1. 删除 `spawn_runner_if_needed` 及所有启动子进程的代码
2. enqueue 前检查运行器是否在运行：以只读、不创建的方式打开 runner.lock；文件不存在或能打开（打开后立即关闭）= 未运行；共享冲突（错误码 32）= 运行中
3. 未运行：不入队，回复 `{"ok":false,"error":"runner_not_running"}`
4. 同一 URL 已有状态为 Queued / Probing / Downloading 的任务：不重复入队，回复 `{"ok":true,"taskId":<已有 id>,"duplicate":true}`
5. 每次 enqueue 写一行 nmhost.log：结果、task id、运行器状态、是否重复。不写 URL 与请求头的值

## 四、扩展
1. 取消 Firefox 下载的条件不变：只有 `reply.ok === true`
2. 收到 `runner_not_running` 时按失败处理（不取消、徽标 "!"），工具栏标题提示"运行器未启动：请执行 idm-cli runner start"
3. 更新 README：安装顺序为 nm install → runner install → runner start → 载入扩展

## 五、全局限速配置
1. 新增设置键 `global-max-speed`（`idm-cli config set global-max-speed 2M`，支持 K/M 后缀；`0` 表示不限速）
2. watch 运行器启动时读取，并每 5 秒重新读取一次，变化时调用全局限速器的 set_rate
3. 默认不限速

## 六、测试
1. runner install/uninstall/status：使用测试专用值名 `idm-rs-runner-test-<随机数>`，Drop 守卫保证失败也清理；绝不使用默认值名
2. runner start/stop：start 后锁被占用；入队一个慢速本地任务，stop 后 10 秒内锁释放、任务状态为 Paused(User)、runner.stop 已删除
3. 运行器启动抢锁重试：在运行器启动的同时有另一个句柄短暂占用锁 500 毫秒后释放，运行器仍能成功启动
4. host：运行器未运行时 enqueue → `runner_not_running`，数据库无新任务；用测试进程持有 runner.lock 模拟运行中 → enqueue 成功；同一 URL 再次 enqueue → `duplicate:true` 且无新行
5. host 源码中不得出现 `Command::new`（加入 phase4b-acceptance 静态检查）
6. global-max-speed：设置 2M，下载本地 8 MiB 文件，平均速度不超过 2.4 MiB/s；运行中改为 0 后，剩余部分明显加快
7. runner.log 中不出现测试用 Cookie 值
8. 更新 `scripts/phase4b-e2e.ps1`：先 `runner start` 再发 ping/enqueue；加一步"enqueue 后强制结束 idm-nmhost 进程，运行器仍在运行并完成下载"；结束时 `runner stop`
9. 回归：全部原有测试、两个崩溃测试

## 七、ARCHITECTURE.md 第 12 节
把"idm-nmhost 只做校验和入队……两者只通过 SQLite 队列通信"一条改为（原样写入）：
```
- idm-nmhost 只做校验和入队，不启动任何进程。下载由独立常驻的队列运行器执行（开机启动，不属于浏览器的进程树）。两者只通过 SQLite 队列和 runner.lock 通信；运行器未运行时 host 拒绝入队，浏览器保留自己的下载。
```

## 八、提交
1. 每个新测试单独连续运行 10 次；e2e 完整运行 3 次；verify-workspace.ps1 16 步全 PASS
2. 只暂存 docs crates scripts extensions ARCHITECTURE.md Cargo.lock，提交信息 `phase4c: standalone runner, host never spawns`，不 push
3. 报告：清理前后的 list；每项修改位置；测试结果；e2e 三次输出；与本文不一致之处
