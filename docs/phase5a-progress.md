# Phase 5a 进度记录

- 步骤 0（保存任务原文 + git status 检查）：完成。git status 干净（HEAD 386f4fa），任务原文已保存为 docs/prompts/phase5a-1.md。
- 步骤 1（环境检查，只读）：完成。node -v = 未安装；npm -v = 未安装；WebView2 pv = 135.0.3179.85。
- 步骤 2（抽取 runner 模块）：用户裁决方案 1 —— recover 从 main 全局路径移到 Runner::start（仅 queue run --watch 跳过 main 的全局 recover）；顺序为抢锁→recover→adopt；拿不到锁不 recover。parse_max_speed 移到 idm-core::net::ratelimit 并公开导出，CLI 全部改调用它、测试一并移走。proxy None→System 与 "runner start failed" 前缀变化记入报告。补预置 Downloading 任务 recover 测试与 AlreadyRunning 测试。恢复实施。
- 步骤 2 结果：`crates/idm-core/src/runner/mod.rs` 新建完成，接口与约定一致；`cargo check --workspace --all-targets` 通过；两个新集成测试 `crates/idm-core/tests/runner.rs` 均 PASS（recover→Paused(Interrupted)、持锁→AlreadyRunning 且状态不变）。
- 步骤 3 结果：`crates/idm-cli/src/main.rs` 的 `queue run --watch` 改为调用 `Runner::start` → AlreadyRunning 打印 "already running" 退出码 3 → `select!` 等 `wait_stopped()` 或 Ctrl+C → `stop()`；非 watch 分支保持原样；main() 全局 recover 仅对 `queue run --watch` 跳过。编译通过。
- 步骤 4 结果：`validate_url` 原样移到 `idm-core/src/net/url_check.rs`，`idm_core::net` 公开导出；host 删除本地函数与测试、改调用公共版本；两个 url_tests 一并移入 core。编译通过。
- 步骤 5（完整回归）：`cargo fmt --all -- --check` 首轮有差异，已 `cargo fmt --all` 修正并复检 PASS。
  - 阻塞点：`cargo clippy --workspace --all-targets --locked -- -D warnings` 失败，唯一错误是 `idm-core/src/net/url_check.rs:47` 测试里的 `println!` 触发 `clippy::print_stdout`（idm-core 的 lib.rs 顶部有 `#![warn(clippy::print_stdout, clippy::print_stderr)]`，而原 host 二进制没有该 lint；测试是按任务要求“原样”搬过来的）。规则要求停下报告，等待用户裁决处理方式。未改任何断言、未删测试、未加 `#[allow]`。
  - 用户裁决选项 1：删除那行调试 `println!` 及 `--nocapture` 注释，函数名与全部断言保持原样；记入报告“与本文不一致之处”。已执行：`validate_url_returns_for_three_inputs` 的三条断言原样保留，仅去掉调试输出及仅供其使用的 inputs/循环。继续第 5 步剩余回归。
  - test（`cargo test --workspace --locked`）PASS；crash_resume 运行中。
  - e2e 回归发现真实偶发：NEW 版 cleanup 偶发残留 watcher（4 次 2 败），对照 HEAD worktree 8/8 PASS。二分定位：`Runner::stop` 用 `watch_task.await` 等其自然结束（内部最多 1s 轮询 + shutdown），watch_task 持有的 store/queue clone 延迟释放，拖慢调用方 `store.shutdown()` 与进程退出。修复：stop 中先 `stop_flag` 再对 watch_task `abort()+await`，停机收尾（"runner stopping"/"runner stopped" 日志、queue.shutdown、删哨兵、释放锁）全部集中于 stop；watch_task 仅探测哨兵并 `break`+通知。修复后 NEW 版 e2e 8/8 PASS。记入报告“与本文不一致之处”。
  - 最终结果：fmt PASS；clippy PASS；`cargo test --workspace` PASS；crash_resume（2 ignored）PASS；e2e ×3 全 PASS（exit 0,0,0；修复后另做过 8/8 压力）；verify-workspace 16 步全 PASS（exit 0）。
- 步骤 6（提交）：只暂存 `docs crates scripts`，提交 `phase5a-pre: extract runner into idm-core`（hash 8509da2）。未 push、未打 tag。
