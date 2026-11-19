# 任务：idm-rs Phase 5b-1 —— 体验修复与小功能

## 规则（必须遵守）
1. 先确认 `git status` 干净，把本任务原文原样保存为 `docs/prompts/phase5b-1.md`；新建进度文件 `docs/phase5b-progress.md`
2. 不联网，不新增任何 crate 或 npm 包
3. 不得使用 `#[allow]`、unsafe、`unwrap()`、`expect()`；前端不得使用 `v-html`、`innerHTML`、`eval`
4. 任何安全护栏或权限拦截时立即停下报告，不得绕过
5. 遇到不明确之处、现有 API 不够用、或任何检查不通过，立即停下报告
6. 测试中凡是写注册表，一律使用带随机后缀的测试专用值名，并用 Drop 守卫保证失败也清理；绝不使用默认值名 `idm-rs-runner`、`idm-rs-desktop`
7. 每完成一步在进度文件追加一行。不要启动桌面程序窗口

## 第 1 步：界面错误处理
1. 顶部错误提示右侧加一个"×"按钮可手动关闭；另外 8 秒后自动消失
2. 点击暂停或继续时，如果后端返回"任务已不在活动或排队状态"这类错误，不显示错误，只立即重新获取一次任务列表
3. 删除确认框里的错误显示保持不变

## 第 2 步：文件名统一
1. 修改 `crates/idm-core/src/net/filename.rs` 的 `resolve_conflict`：识别以下复合扩展名 `.tar.gz`、`.tar.xz`、`.tar.bz2`、`.tar.zst`（不区分大小写），序号插在完整扩展名之前。例如已存在 `bash-5.2.tar.gz` 时得到 `bash-5.2 (1).tar.gz`；普通文件仍是 `a (1).zip`
2. 新增公开函数 `strip_browser_dup_suffix(name: &str) -> String`：去掉紧贴在扩展名（含上述复合扩展名）之前的 `(数字)` 或 ` (数字)`。例如 `bash-5.2(1).tar.gz` → `bash-5.2.tar.gz`，`a (2).zip` → `a.zip`，`a(1)` → `a`；不符合模式的保持原样，例如 `report(v2).pdf`、`(1).zip` 不变
3. idm-nmhost 处理浏览器传来的 filename 时，先取文件名部分，再调用 `strip_browser_dup_suffix`，再调用 `sanitize_windows_filename`
4. 单元测试覆盖以上所有例子，以及大小写 `.TAR.GZ`

## 第 3 步：打开所在文件夹
1. `DesktopService` 新增 `pub async fn reveal_task(&self, id: i64) -> Result<(), ServiceError>`：只对已完成的任务有效；从数据库取最终文件路径，文件不存在返回 InvalidInput("文件不存在")
2. 启动 `explorer.exe`，参数为 `/select,"<完整路径>"`。因为 explorer 对引号的解析特殊，使用 `std::os::windows::process::CommandExt::raw_arg` 拼参数，路径中不得含双引号（有则返回 InvalidInput）
3. 把"构造参数字符串"写成独立的纯函数并单元测试（含空格、中文路径）；实际启动 explorer 不做自动测试
4. 观察模式下也允许此操作（只读操作）
5. 新增 Tauri 命令 `reveal_task`；界面中已完成任务的操作栏增加"打开文件夹"按钮

## 第 4 步：清除已完成
1. `DesktopService` 新增 `pub async fn clear_completed(&self) -> Result<usize, ServiceError>`：删除所有已完成任务的记录（不删除文件），返回删除条数；观察模式返回 ReadOnly
2. 新增 Tauri 命令；表格上方增加"清除已完成"按钮，点击前用自定义确认框确认
3. 列表按任务 id 倒序显示（最新的在最上面）

## 第 5 步：开机启动改为桌面程序
1. 把 idm-cli 中读写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的代码移到 idm-core 新模块 `autostart`（公开函数：`install(value_name, command_line)`、`uninstall(value_name)`、`query(value_name) -> Option<String>`），CLI 的 `runner install/uninstall/status` 改为调用它，行为不变
2. `Settings` 增加字段 `autostart: bool`：读取时看 Run 中是否存在值 `idm-rs-desktop`
3. 开启时：写入 `idm-rs-desktop` = `"<当前 idm-desktop.exe 绝对路径>" --minimized`，并删除 `idm-rs-runner`（如存在），避免开机时两个运行器抢运行权；关闭时：只删除 `idm-rs-desktop`
4. 为了测试，值名通过 DesktopService 内部可配置（默认 `idm-rs-desktop` / `idm-rs-runner`），测试中换成随机测试名
5. 桌面程序收到命令行参数 `--minimized` 时，启动后不显示主窗口，只显示托盘图标
6. 设置面板增加"开机自动启动"复选框
7. 观察模式下不允许修改

## 第 6 步：测试
- 第 2 步所有单元测试
- `reveal_args` 纯函数测试
- `clear_completed_removes_only_completed`：一个已完成、一个已暂停，清除后只剩已暂停的，已完成任务的文件仍在
- `autostart_toggle`：使用测试值名，开启后两个值的状态符合第 5 步第 3 条；关闭后只删除桌面值；Drop 守卫清理
- `runner_install_still_works`：CLI 的 runner install/uninstall 迁移后行为不变（测试值名）
- 新测试各单独连续运行 10 次；原有桌面测试和 idm-core 测试全部运行 1 次

## 第 7 步：验证与提交
1. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`npm run build`
2. `verify-workspace.ps1` 全部 PASS；`phase4b-e2e.ps1` 运行 1 次 PASS
3. 暂存 `docs apps scripts crates Cargo.toml Cargo.lock .gitignore`，提交信息 `phase5b-1: ux fixes, reveal, clear, desktop autostart`，不 push、不 amend

## 报告（按顺序）
1. 每步的修改位置（文件:行号）
2. `strip_browser_dup_suffix` 与 `resolve_conflict` 测试用例表（输入 → 输出）
3. 测试自查表（测试名 | 10 次结果）
4. verify 与 e2e 结果
5. 手动验收清单（写成用户能照做的步骤，包含 5a 未测完的：暂停/继续、删除勾选与不勾选、托盘、退出、观察模式；以及本步新增：关闭错误提示、打开文件夹、清除已完成、开机启动开关、--minimized 启动）
6. 提交 hash
7. 与本文不一致之处
