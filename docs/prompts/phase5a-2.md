# 任务：idm-rs Phase 5a-2 —— 桌面程序空架子 + 验收脚本调整

## 规则（必须遵守）
1. 先把本任务原文原样保存为 `docs/prompts/phase5a-2.md`。`git status` 必须干净，否则停下报告
2. 本任务只搭空架子：一个能打开窗口、显示一行文字的 Tauri 程序。**不接入下载引擎，不写业务功能**
3. 允许一次性联网，范围仅限：`npm install`（只装下面第 2 步列出的包）和 `cargo fetch`（Tauri 相关 crate）。完成后恢复离线，其余构建与测试用 `--offline` 或 `--locked`
4. 禁止 `cargo update`。`Cargo.lock` 中已有包的版本不得改变（允许因 Tauri 新增同名包的其他版本）。做不到就停下报告
5. 不得使用 `#[allow]`，不得写 unsafe，不得用 `unwrap()` / `expect()`（workspace 的 clippy 规则会拒绝）
6. 遇到不明确的地方、或任何检查不通过，**立即停下报告**，不要自己绕过
7. 每完成一步在 `docs/phase5a-progress.md` 追加一行。上下文快满时先写进度再停下

## 第 1 步：环境确认
执行 `node -v`、`npm -v`，贴出输出。Node 主版本低于 20 或未安装就停下报告。

## 第 2 步：建立前端工程 `apps/idm-desktop/`
1. 手写以下文件（不要用脚手架命令生成）：`package.json`、`index.html`、`vite.config.ts`、`tsconfig.json`、`src/main.ts`、`src/App.vue`
2. `App.vue` 只显示一行中文："idm-rs 桌面程序（建设中）"
3. 依赖只允许这些：
   - dependencies：`vue`、`@tauri-apps/api`（2.x）
   - devDependencies：`vite`、`@vitejs/plugin-vue`、`@tauri-apps/cli`（2.x）、`typescript`、`vue-tsc`
4. `package.json` 的 scripts：`"build": "vue-tsc --noEmit && vite build"`、`"tauri": "tauri"`
5. 执行 `npm install`，提交生成的 `package-lock.json`
6. `.gitignore` 增加 `apps/idm-desktop/node_modules` 和 `apps/idm-desktop/dist`

## 第 3 步：建立 Rust 端 `apps/idm-desktop/src-tauri/`
1. crate 名 `idm-desktop`，加入根 `Cargo.toml` 的 workspace members，继承 workspace 的 lints
2. 依赖：`tauri`（2.x）、`serde`、`serde_json`（用 workspace 声明）。暂不依赖 idm-core，不引入任何 tauri 插件
3. `tauri.conf.json`：产品名 `idm-rs`，标识符 `com.idmrs.desktop`，`frontendDist` 指向 `../dist`，窗口标题 `idm-rs`，宽 1000 高 640；CSP 设为 `default-src 'self'`
4. `capabilities/default.json`：只授予 `core:default`，不加任何其他权限
5. `main.rs` 顶部可用 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`；启动失败时把错误写到 stderr 并以非 0 退出码退出（不得用 expect）
6. 图标：用 PowerShell 的 System.Drawing 生成一张 1024×1024 的占位 PNG（纯色底，居中写 "idm"），保存为 `src-tauri/icons/source.png`，再执行 `npx tauri icon src-tauri/icons/source.png` 生成全套图标并提交
7. 执行 `cargo fetch` 拉取依赖，之后恢复离线

## 第 4 步：修改 `scripts/verify-workspace.ps1`
1. STEP 8：改为分别对 `idm-core`、`idm-cli`、`idm-ipc` 三个 crate 执行 `cargo tree -p <crate>`，任一出现 `tauri`、`wry`、`tao` 即 FAIL；`idm-desktop` 不检查
2. STEP 10：只允许 `apps/idm-desktop/package.json` 存在；仓库其他任何位置（包括 `extensions/`）出现 `package.json` 即 FAIL；任何位置有被 git 跟踪的 `node_modules` 即 FAIL
3. STEP 12、13 不变（会自动覆盖新 crate）
4. 新增 STEP 17：在 `apps/idm-desktop` 执行 `npm ci --prefer-offline` 和 `npm run build`，失败即 FAIL
5. 新增 STEP 18：`cargo build -p idm-desktop --locked`，失败即 FAIL（必须在 STEP 17 之后，因为它需要 `dist` 目录）
6. 注意 STEP 4/5/6 的 `cargo check/test/clippy --workspace` 也会编译 idm-desktop，它同样需要 `dist` 存在。若因此失败，把 STEP 17 移到 STEP 4 之前执行，并在报告中说明
7. 最终 SUMMARY 改为按实际步骤数统计

## 第 5 步：验证
1. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`，全部通过
2. `scripts/verify-workspace.ps1` 全部步骤 PASS，退出码 0
3. `scripts/phase4b-e2e.ps1` 完整运行 1 次 PASS（确认没有影响已有功能）
4. **不要启动桌面程序的窗口**，窗口验收由用户手动完成

## 第 6 步：提交
只暂存 `docs apps scripts Cargo.toml Cargo.lock .gitignore`，提交信息 `phase5a-2: desktop scaffold`。不 push，不打 tag。

## 报告（按这个顺序）
1. `node -v`、`npm -v` 输出
2. `npm install` 实际安装的包及版本（`npm ls --depth=0` 原样贴出）
3. `Cargo.lock`：新增包数量；已有包版本是否有变化（逐一核对，贴出结论）
4. `verify-workspace.ps1` 完整的步骤列表与结果、退出码
5. 新增和修改的文件列表（每个一句话）
6. 用户手动启动窗口的命令（例如在 `apps/idm-desktop` 下执行 `npm run tauri dev`）
7. 提交 hash
8. 与本文不一致之处（没有就写"无"）
