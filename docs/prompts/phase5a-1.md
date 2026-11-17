# 任务：idm-rs Phase 5a-1 —— 环境检查 + 抽取公共运行器模块（纯重构）

## 规则（必须遵守）
1. 先把本任务原文原样保存为 `docs/prompts/phase5a-1.md`。`git status` 必须干净，否则停下报告
2. 本任务是**纯重构**：程序行为不得有任何改变。不新增功能，不改测试断言
3. 不联网，不安装任何东西，不执行 `cargo update`，不新增依赖
4. 遇到不明确的地方、或任何一项检查不通过，**立即停下报告**，不要自己想办法绕过
5. 每完成一步，在 `docs/phase5a-progress.md` 里记一行（步骤号 + 结果）。上下文快满时，先写进度再停下

## 第 1 步：环境检查（只读，不安装）
执行并贴出原始输出：
- `node -v`、`npm -v`（找不到就写"未安装"，不要安装）
- WebView2 运行时版本：查询注册表 `HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` 的 `pv` 值（找不到就写"未找到"）
这一步只记录，不论结果如何都继续第 2 步。

## 第 2 步：新建 idm-core 的 runner 模块
在 `crates/idm-core/src/runner/mod.rs` 新建模块，把 `crates/idm-cli/src/main.rs` 里 `queue run --watch` 的运行器逻辑**搬过去**（是搬，不是重写）。包括：
- 抢 runner.lock（独占打开，200 毫秒重试，共 3 秒）
- 启动恢复（recover）、adopt_queued
- 每秒检查新的 Queued 任务
- 每秒检查 runner.stop 文件，发现后安全停止并删除该文件
- runner.log 写入（1 MiB 覆盖重写，不写 Cookie 和完整 URL 查询参数）
- 每 5 秒重读设置 global-max-speed

对外接口**必须**是下面这个样子（可以增加字段，不能减少）：

```rust
pub struct RunnerConfig {
    pub db_path: std::path::PathBuf,
    pub max_tasks: usize,
    pub host_max_connections: usize,
    pub proxy: Option<crate::net::client::ProxyConfig>,
}

pub enum RunnerStartError {
    /// runner.lock 被占用（另一个运行器正在运行）
    AlreadyRunning,
    Other(crate::error::Error),
}

pub struct Runner { /* 私有字段 */ }

impl Runner {
    /// 获取 runner.lock 并启动下载引擎。
    pub async fn start(store: std::sync::Arc<crate::store::Store>, cfg: RunnerConfig)
        -> Result<Runner, RunnerStartError>;
    /// 返回内部队列，供调用方增删任务和读取进度。
    pub fn queue(&self) -> &crate::queue::DownloadQueue;
    /// 等待直到运行器因 runner.stop 文件而停止。
    pub async fn wait_stopped(&self);
    /// 主动安全停止：取消活动任务（变为 Paused(User)）、释放锁、结束所有后台任务。
    pub async fn stop(self) -> Result<(), crate::error::Error>;
}
```

要求：
- `stop` 返回后，runner.lock 必须已释放，模块内启动的所有 tokio 任务必须已结束（之前修过的 Ctrl+C 任务问题不能再出现）
- 模块里**不得**处理 Ctrl+C，Ctrl+C 留在 CLI 里

## 第 3 步：CLI 改为调用 runner 模块
`queue run --watch` 改为：`Runner::start` → 拿不到锁时打印 "already running" 并以退出码 3 退出 → 等待 `wait_stopped()` 或 Ctrl+C（二者先到者）→ 调用 `stop()`。
不带 `--watch` 的 `queue run` 保持原样，不要改。

## 第 4 步：移动 validate_url
把 `crates/idm-ipc/src/bin/idm-nmhost.rs` 里的 `validate_url` 函数原样移到 idm-core（例如 `crates/idm-core/src/net/url_check.rs`），并从 `idm_core::net` 公开导出。host 改为调用它。原来在 host 里的单元测试一并移过去。

## 第 5 步：回归（全部通过才能提交）
依次执行，贴出每一项的最终结果行和退出码：
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `cargo test --workspace --locked`
4. `cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1`
5. `scripts/phase4b-e2e.ps1` 完整运行 3 次，每次都必须 `E2E RESULT: PASS`
6. `scripts/verify-workspace.ps1`，16 步全 PASS，退出码 0

任何一项失败：修复的是**你搬代码时引入的问题**，不得修改测试断言、不得删除测试、不得加 `#[allow]`。如果认为失败与本次改动无关，停下报告，不要自己处理。

## 第 6 步：提交
只暂存 `docs crates scripts`，提交信息：`phase5a-pre: extract runner into idm-core`。不 push，不打 tag。

## 报告（按这个顺序）
1. 第 1 步的环境检查原始输出
2. 新增和修改的文件列表（每个文件一句话说明改了什么）
3. `Runner` 接口最终的完整签名（原样贴出）
4. `crates/idm-cli/src/main.rs` 中 `queue run --watch` 分支改动后的完整代码（原样贴出）
5. 第 5 步每一项的结果和退出码
6. 提交 hash
7. 与本文不一致之处（没有就写"无"）
