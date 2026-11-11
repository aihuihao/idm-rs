# 任务：idm-rs Phase 4a —— Native Messaging host、请求头透传、常驻队列运行器

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase4a.md`。当前工作区有 `Cargo.lock` 与 `crates/idm-ipc/Cargo.toml` 两处未提交改动（serde、serde_json 依赖），这是预期的，随本任务一起提交，不算不干净。
本阶段不写浏览器扩展（4b）。所有测试只用本地回环服务器和测试用的数据库，不访问外网，不碰真实注册表。
遇到不明确的地方先停下报告，不得自行变通。提交前必须完成第八部分自查表，任何一项测试缺失都不得提交。
上下文快满时，先把进度写进 `docs/phase4a-progress.md` 再停下。

## 一、依赖（已完成，不再联网）

- `idm-ipc` 已声明 `serde`（derive）和 `serde_json`，锁文件已更新，离线缓存已有。其他 crate 不得引入这两个包
- 根 `workspace.dependencies` 中声明，子 crate 用 `workspace = true`（如当前是直接写在 idm-ipc 里，请迁移到 workspace 声明，版本不变）
- 禁止 `cargo update`，所有构建用 `--offline`。报告里确认 Cargo.lock 相对基线只新增了 `serde_json 1.0.154` 和 `zmij 1.0.23`

## 二、数据库迁移 V4（`store/`）

1. 新表 `task_headers(task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, name TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(task_id, name))`
2. 新表 `settings(key TEXT PRIMARY KEY, value TEXT NOT NULL)`
3. Store 新方法：`set_task_headers(id, &[(String, String)])`、`get_task_headers(id)`、`clear_task_headers(id)`、`get_setting(key)`、`set_setting(key, value)`
4. 任务到达 `Completed` 时自动清除该任务的请求头（Cookie 不得长期留在数据库里）；`remove` 任务时随外键级联删除
5. `enqueue_task` 新增可选的 `headers` 参数，在同一个事务里写入任务和请求头

## 三、请求头透传（`idm-core`）

1. `DownloadConfig` 新增 `extra_headers: Vec<(String, String)>`。只允许这三个名字（不区分大小写）：`Referer`、`Cookie`、`User-Agent`；其他名字返回 `Error::InvalidArgument`。值中不得含 `\r` 或 `\n`，每个值不超过 8 KiB
2. Probe 请求和每个分块请求（包括重试、续传）都带上这些头。`User-Agent` 覆盖默认值
3. 队列路径（`build_options`）和 `resume` 从数据库读取任务的请求头并设置到配置里
4. 日志中不得打印 Cookie 的值（只打印"有 Cookie"）；错误信息中同样不得包含 Cookie 值
5. 注意 reqwest 跨域重定向时会自动去掉 Cookie：不需要额外处理，在报告里说明你实际观察到的行为

## 四、常驻队列运行器（`idm-cli queue run --watch`）

1. 新增参数 `--watch`：队列空闲后不退出，持续运行，每秒检查一次数据库中新增的 `Queued` 任务并调度。Ctrl+C 安全停止所有活动任务（沿用现有取消流程）
2. 单实例：运行器启动时在数据目录（数据库所在目录）下创建 `runner.lock`，用 `OpenOptionsExt::share_mode(0)` 独占打开并保持到进程退出，不需要 unsafe。已有实例持有时，打印 "already running" 并以退出码 3 退出
3. 不带 `--watch` 时行为与现在完全一致
4. 新增 `idm-cli config set <key> <value>` 和 `idm-cli config get <key>`，目前只支持 key `download-dir`（目录必须存在或能创建）

## 五、Native Messaging host（`idm-ipc` 的二进制 `idm-nmhost`）

1. **协议**：Firefox 的 Native Messaging 帧格式：每条消息前 4 个字节为消息长度（本机字节序，Windows 上为小端），后面是 UTF-8 JSON。host 循环读取，直到标准输入关闭
2. **输出纪律（必须）**：标准输出只写协议帧，任何日志、panic 信息都不得写到标准输出。日志写到数据目录下的 `nmhost.log`（追加，限制单文件 1 MiB，超过则覆盖重写）。设置 panic hook 把 panic 写到日志
3. **请求**（浏览器 → host）：
   - `{"type":"ping"}` → `{"ok":true,"version":"<crate版本>"}`
   - `{"type":"enqueue","url":"...","referrer":"...","cookies":"...","userAgent":"...","filename":"..."}` → 成功 `{"ok":true,"taskId":<id>}`，失败 `{"ok":false,"error":"<简短原因>"}`；`referrer`、`cookies`、`userAgent`、`filename` 可选
4. **校验（不可信输入）**：
   - 单条消息长度上限 64 KiB，超过则返回错误并关闭，不得先分配这么大的缓冲区再检查
   - URL 必须能解析，scheme 只能是 `http` 或 `https`，长度不超过 8 KiB，必须有 host
   - `filename` 只取文件名部分：去掉路径分隔符和上级目录片段，按已有的 Windows 合法化函数处理，空则不指定
   - 请求头按第三部分的规则校验；不合法的请求头直接丢弃该头（不影响入队），并在日志中记录
   - 未知的 `type` 返回错误，不得 panic；JSON 非法返回错误
5. **入队**：读取设置 `download-dir`（未设置则用 `%USERPROFILE%\Downloads\idm-rs`），连接数使用默认值，调用 `enqueue_task`
6. **拉起运行器**：入队成功后，尝试以独占方式打开 `runner.lock`：打不开（被占用）说明运行器在运行，不做任何事；能打开则立即关闭它，并启动与 host 同目录的 `idm-cli.exe queue run --watch --db <同一数据库>`，使用 `CommandExt::creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)`（常量值自行定义，不需要 unsafe），标准输入输出重定向到空。找不到 `idm-cli.exe` 时，入队仍然成功，在回复里附加 `"runner":"not_started"`
7. 数据库路径：默认数据库；环境变量 `IDM_RS_DB` 可覆盖（测试用）

## 六、测试

放在 `crates/idm-ipc/tests/` 和 `crates/idm-core/tests/`。

1. 帧读写单元测试：正常、长度为 0、长度超过上限（不分配缓冲区）、半截数据后输入关闭、连续多条
2. `host_ping_roundtrip`：启动 `idm-nmhost` 子进程，用真实帧协议通信
3. `host_enqueue_creates_queued_task`：入队后数据库里有一条 `Queued` 任务，URL、目录、请求头正确
4. `host_rejects_bad_input`：非 http(s) scheme（`file:///`、`javascript:`、`ftp://`）、无 host、超长 URL、超大消息、非法 JSON、未知 type、含换行的请求头，每种都返回错误且数据库里没有新任务，host 进程不崩溃，继续能处理下一条正常消息
5. `host_stdout_contains_only_frames`：用会触发 panic 的测试开关（仅在测试构建里存在）验证标准输出没有帧以外的字节，panic 写进日志文件
6. `filename_is_sanitized`：`..\..\evil.exe`、`C:\Windows\x.dll`、`a/b/c.zip`、保留名 `CON`，都被处理成安全文件名
7. `headers_reach_server`：入队带 Referer、Cookie、User-Agent 的任务，运行队列，测试服务器记录 Probe 和每个分块请求的头，三个头都出现；重试和续传请求也带上
8. `headers_cleared_on_completion`：任务完成后 `task_headers` 为空
9. `cookie_not_in_logs`：运行任务并收集日志输出，断言 Cookie 的值不出现
10. `invalid_headers_rejected_in_core`：非白名单名字、含换行、超长的值
11. `watch_runner_picks_up_new_tasks`：启动 `queue run --watch`，空闲后再入队一个任务，该任务在 3 秒内开始并下载完成，逐字节正确
12. `watch_runner_single_instance`：第二个实例以退出码 3 退出；第一个实例退出后，第三个实例可以启动
13. `host_spawns_runner_once`：连续入队两个任务，只有一个运行器进程存在（用测试里的假 `idm-cli`，或统计进程）
14. 迁移 V4：从 V3 升级成功，重复执行不出错
15. 回归：全部原有测试、两个崩溃测试

## 七、文档

`ARCHITECTURE.md` 追加第 12 节（原样写入）：

```
## 12. 浏览器接管（Phase 4 确立）
- 通信：只使用 Native Messaging（Firefox 清单的 allowed_extensions 限定扩展 ID）。不监听任何本地网络端口。
- idm-nmhost 只做校验和入队，不做下载；下载由独立的常驻队列运行器执行。两者只通过 SQLite 队列通信。
- 浏览器传来的一切数据都不可信：限制长度、限制 scheme、请求头白名单（Referer / Cookie / User-Agent）、文件名只取安全的文件名部分。
- Cookie 只在任务存续期间保存在 task_headers 表，任务完成或删除时清除；日志和错误信息中不得出现 Cookie 的值。
- idm-nmhost 的标准输出只能写协议帧。
```

## 八、提交前自查表

提交前在 `docs/phase4a-report.md` 中列出，任何一项测试名为空或未满 10/10 通过均不得提交：

| 第六部分编号 | 要求摘要 | 测试名 | 单独连续运行 10 次的结果 |
|---|---|---|---|

## 九、提交与报告

1. 每个新测试单独连续运行 10 次；验收脚本全部 PASS，exit code 0
2. 只暂存 `docs crates scripts ARCHITECTURE.md Cargo.toml Cargo.lock`，提交信息 `phase4a: native messaging host, header passthrough, watch runner`，不得 push，不打 tag
3. 报告：自查表；Cargo.lock 新增的包；每项实现的代码位置（文件:行号）；跨域重定向时 Cookie 的实际行为；测试总数；与本文不一致之处
