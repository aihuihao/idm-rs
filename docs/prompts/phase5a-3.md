# 任务：idm-rs Phase 5a-3 —— 桌面程序服务层（接入下载引擎）

## 规则（必须遵守）
1. 先执行 `node -v`，必须不补路径就能识别；不能识别就停下报告。然后把本任务原文原样保存为 `docs/prompts/phase5a-3.md`，`git status` 必须干净
2. 不联网，不新增任何依赖（crate 与 npm 包都不加），不执行 `cargo update`
3. 不得使用 `#[allow]`、unsafe、`unwrap()`、`expect()`
4. **任何安全护栏或权限拦截你的操作时，立即停下报告，不得换方式绕过**
5. 遇到不明确的地方、现有 API 不够用、或任何检查不通过，立即停下报告，不要自己发明大改动
6. 每完成一步在 `docs/phase5a-progress.md` 追加一行。上下文快满时先写进度再停下
7. 不要启动桌面程序窗口

## 第 1 步：顺手清理
`apps/idm-desktop/src-tauri/gen/schemas` 是 Tauri 每次构建自动生成的文件：加入 `.gitignore`，并用 `git rm -r --cached` 移出版本库（不删除本地文件）。

## 第 2 步：工程结构
1. `idm-desktop` 增加依赖 `idm-core`（path 依赖）、`tokio`（workspace 声明）
2. 新建 `src-tauri/src/lib.rs`，公开两个模块：`service`、`throttle`。`main.rs` 只负责启动
3. `service` 与 `throttle` 模块**不得引用 tauri**，这样可以脱离 Tauri 单独测试

## 第 3 步：service 模块（接口必须如下，可加字段不可减）
```rust
pub enum ServiceError {
    ReadOnly,               // 观察模式下的写操作
    InvalidInput(String),   // 校验失败，消息不得含 Cookie 和带凭据的 URL
    Internal(String),
}
// 为 ServiceError 实现 Display 和 serde::Serialize（序列化为字符串）

#[derive(serde::Serialize)]
pub struct TaskView {
    pub id: i64,
    pub file_name: Option<String>,
    pub url_display: String,     // 只保留 scheme://host[:port]/path，去掉用户名密码、查询参数、片段
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub speed_bps: u64,          // 当前速度，非活动任务为 0
    pub state: String,
    pub pause_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub download_dir: String,
    pub global_max_speed: u64,     // 字节/秒，0 表示不限速
    pub max_tasks: usize,          // 1..=16
    pub default_connections: usize // 1..=32
}

pub struct DesktopService { /* 私有 */ }

impl DesktopService {
    /// 打开数据库并尝试成为运行器：调用 idm_core::runner::Runner::start。
    /// 返回 AlreadyRunning 时进入观察模式：不启动引擎、不执行 recover。
    pub async fn open(db_path: std::path::PathBuf) -> Result<Self, ServiceError>;
    pub fn is_read_only(&self) -> bool;
    pub async fn list_tasks(&self) -> Result<Vec<TaskView>, ServiceError>;
    pub async fn add_task(&self, url: String, connections: Option<usize>) -> Result<i64, ServiceError>;
    pub async fn pause_task(&self, id: i64) -> Result<(), ServiceError>;
    pub async fn resume_task(&self, id: i64) -> Result<(), ServiceError>;
    pub async fn remove_task(&self, id: i64, delete_file: bool) -> Result<(), ServiceError>;
    pub async fn get_settings(&self) -> Result<Settings, ServiceError>;
    pub async fn set_settings(&self, s: Settings) -> Result<(), ServiceError>;
    /// 安全停止：运行器模式下调用 Runner::stop（活动任务变为 Paused(User)），然后关闭数据库。
    pub async fn shutdown(self) -> Result<(), ServiceError>;
}
```
要求：
- 只调用 idm-core 现有的公开 API。`add_task` 用 `idm_core::net::validate_url` 校验
- 设置存在数据库 settings 表中，键名：`download-dir`、`global-max-speed`、`max-tasks`、`default-connections`。`global-max-speed` 修改后立即生效（运行器每 5 秒会重读，也可直接调用队列的限速接口）
- `max-tasks` 如果现有队列不支持运行中修改，**停下报告**，不要自己改队列
- `download_dir` 必须存在或能创建，否则返回 InvalidInput
- 列表里**不得出现任何请求头或 Cookie**

## 第 4 步：throttle 模块
```rust
pub struct Throttle { /* 私有 */ }
impl Throttle {
    pub fn new(min_interval: std::time::Duration) -> Self;
    /// 内容与上次推送相同返回 false；距上次推送不足 min_interval 返回 false；否则返回 true 并记录。
    pub fn should_emit(&mut self, now: std::time::Instant, snapshot: &[u8]) -> bool;
}
```

## 第 5 步：Tauri 薄包装（只在 main.rs / 独立的 commands.rs 里出现 tauri）
1. 启动时用默认数据库路径（与 idm-cli 一致；环境变量 `IDM_RS_DB` 可覆盖）调用 `DesktopService::open`，失败则写 stderr 并以非 0 退出
2. 为第 3 步的每个方法注册一个同名 Tauri 命令（`shutdown` 除外），另加 `is_read_only` 命令。命令函数只做转发，不写业务逻辑
3. 后台任务每 250 毫秒调用 `list_tasks`，把结果序列化为 JSON，用 `Throttle::new(250ms)` 判断，需要时发送事件 `tasks-updated`
4. 程序退出时调用 `shutdown`
5. capabilities 保持只有 `core:default`

## 第 6 步：测试（放在 `apps/idm-desktop/src-tauri/tests/`）
可以复用 `crates/idm-core/tests/support/mod.rs` 的本地测试服务器（用 `#[path]` 引用，idm-cli 的测试就是这么做的）。每个测试用独立的临时数据库。
1. `add_task_downloads_correctly`：添加后任务完成，文件逐字节正确
2. `pause_and_resume`：暂停后状态为 Paused(User)，继续后完成且逐字节正确
3. `remove_task_with_and_without_file`：delete_file 为真时文件与 .idm-part 都被删除；为假时保留文件
4. `global_speed_applies_immediately`：设为 512 KiB/s，下载 4 MiB，平均速度不超过 614 KiB/s
5. `invalid_url_rejected`：`file:///x`、`http://user:pw@host/x`、`http:///x` 都返回 InvalidInput，且数据库没有新任务
6. `list_hides_headers_and_cookies`：用 store 直接入队一个带 Cookie 头的任务，list_tasks 序列化后的 JSON 不含 Cookie 值、不含 headers 字段
7. `url_display_strips_secrets`：`https://u:p@h.com:8443/a/b.zip?token=x#f` 显示为 `https://h.com:8443/a/b.zip`
8. `observer_mode_is_read_only`：先由测试持有 runner.lock，再 open：is_read_only 为真；所有写操作返回 ReadOnly；list_tasks 可用；预置的 Downloading 任务状态不变（证明没执行 recover）
9. `settings_validation`：max_tasks 为 0 或 17、default_connections 为 0 或 33 被拒绝；download_dir 不存在但可创建时成功并已创建
10. `throttle_rules`：相同内容不推送；250 毫秒内第二次不推送；内容变化且超过间隔则推送；1 秒内快速变化推送不超过 4 次
11. `shutdown_pauses_active_task`：下载中调用 shutdown，任务变为 Paused(User)，runner.lock 已释放

## 第 7 步：验证
1. 每个新测试单独连续运行 10 次，全部通过
2. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `scripts/verify-workspace.ps1` 全部步骤 PASS（如果 STEP 4 的 npm ci 被安全护栏拦截，按规则 4 停下报告）
4. `scripts/phase4b-e2e.ps1` 运行 1 次 PASS

## 第 8 步：提交
只暂存 `docs apps scripts Cargo.toml Cargo.lock .gitignore`，提交信息 `phase5a-3: desktop service layer`。不 push，不打 tag，不 amend。

## 报告（按顺序）
1. `node -v` 输出（确认没有补路径）
2. 第 3 步接口最终完整签名（原样贴出）
3. 每个方法调用了 idm-core 的哪些 API（一个方法一行）
4. 自查表：测试名 | 对应第 6 步编号 | 10 次结果
5. verify 步骤与结果、退出码；e2e 结果
6. 新增和修改的文件（每个一句话）
7. 提交 hash
8. 与本文不一致之处（没有就写"无"）
