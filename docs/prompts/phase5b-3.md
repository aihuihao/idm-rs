# 任务：idm-rs Phase 5b-3 —— 界面美化（IDM 风格布局）

## 规则（必须遵守）
1. 先确认 `git status` 干净，把本任务原文原样保存为 `docs/prompts/phase5b-3.md`
2. 不联网，不新增任何 crate 或 npm 包。图标全部用内联 SVG 自己绘制简单图形，不得使用任何第三方图标库，不得复制 IDM 或其他软件的图标、图片
3. 不得使用 `#[allow]`、unsafe、`unwrap()`、`expect()`；前端不得使用 `v-html`、`innerHTML`、`eval`
4. 任何安全护栏或权限拦截时立即停下报告，不得绕过
5. 遇到不明确之处或任何检查不通过，立即停下报告
6. 每完成一步在 `docs/phase5b-progress.md` 追加一行。不要启动桌面程序窗口

## 第 1 步：修复文件夹选择框占用工作线程
`pick_download_dir` 目前在异步命令里直接调用 `blocking_pick_folder`，对话框打开期间会一直占用一个异步工作线程。改为放进 `tauri::async_runtime::spawn_blocking` 中执行，等待其结果后返回。行为不变。

## 第 2 步：任务类型分类（Rust 侧，便于测试）
1. `TaskView` 增加字段 `kind: String`，取值：`archive`（zip rar 7z tar gz xz bz2 zst iso）、`video`（mp4 mkv avi mov wmv flv webm）、`audio`（mp3 flac wav aac ogg m4a）、`document`（pdf doc docx xls xlsx ppt pptx txt epub）、`program`（exe msi apk dmg deb rpm）、`other`
2. 按文件名（没有时按 url_display）的扩展名判断，不区分大小写；复合扩展名如 `.tar.gz` 算 archive
3. 写成独立纯函数 `pub fn classify(name: &str) -> &'static str`，单元测试覆盖每一类至少两个例子、大写扩展名、无扩展名、`.tar.gz`

## 第 3 步：整体布局
1. **工具栏**（顶部）：添加任务、继续、暂停、删除、清除已完成、选项。每个按钮是"图标 + 文字"。继续、暂停、删除作用于当前选中的任务，没有选中或选中的任务都不适用时按钮禁用
2. **网址输入**：点"添加任务"弹出一个小对话框输入网址（自定义模态框），回车或点"开始下载"提交，失败在对话框内红字提示。取消原来常驻的输入框
3. **左侧分类栏**（宽约 160px）：上半部分按状态：全部、下载中、未完成（排队、暂停、失败）、已完成；下半部分按类型：压缩包、视频、音频、文档、程序、其他。每项右侧显示数量。点击筛选右侧表格
4. **任务表格**：去掉每行的操作按钮。列为：类型图标、文件名、大小、进度、速度、剩余时间、状态。保持固定列宽与等宽数字
   - 进度条：下载中蓝色、已完成绿色、暂停灰色、失败红色，百分比文字显示在进度条内
   - 单击选中一行；Ctrl+单击多选；选中行高亮
   - 双击已完成任务：打开所在文件夹
   - 右键一行弹出自定义右键菜单（不用浏览器默认菜单）：继续、暂停、删除、打开文件夹（不适用的项禁用）；右键时若该行未被选中，先只选中它
   - 键盘 Delete 键：删除选中任务（弹出原有删除确认框，多选时显示"确定删除选中的 N 个任务吗？"）
5. **底部状态栏**：下载中任务数、总速度（所有任务速度之和）、当前全局限速（0 显示"不限速"）
6. 多选时的批量操作：逐个调用已有命令。某个任务已结束导致的错误静默忽略，最后刷新一次列表
7. 选中状态在列表刷新后保持（按任务 id）；被删除的任务从选中集合中移除

## 第 4 步：外观
1. 用 CSS 变量定义颜色，同时支持浅色和深色：跟随系统（`prefers-color-scheme`）
2. 字体使用系统默认的中文界面字体（`"Microsoft YaHei UI", "Segoe UI", sans-serif`），字号 13px
3. 表格行高约 28px，隔行浅色底纹，鼠标悬停高亮
4. `tauri.conf.json` 设置窗口最小尺寸：宽 860、高 520
5. 观察模式横幅保留在工具栏下方；观察模式下工具栏中除"选项"外全部禁用，右键菜单全部禁用

## 第 5 步：测试与验证
1. `classify` 单元测试各单独连续运行 10 次；原有桌面测试全部运行 1 次（`TaskView` 新增字段，相关测试如需补字段请补上）
2. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`npm run build`
3. `verify-workspace.ps1` 全部 PASS；`phase4b-e2e.ps1` 运行 1 次 PASS
4. 暂存 `docs apps scripts crates Cargo.toml Cargo.lock .gitignore`，提交信息 `phase5b-3: idm-style layout`，不 push、不 amend

## 报告（按顺序）
1. 每步修改位置（文件:行号）
2. `classify` 测试用例表
3. 测试自查表；verify 与 e2e 结果
4. 手动验收清单，必须合并以下所有项，写成用户能照做的步骤：
   - 本步：工具栏、添加对话框、分类筛选与数量、单选/多选、双击打开文件夹、右键菜单、Delete 键、状态栏、深浅色、窗口最小尺寸、速度变化时不跳动
   - 5b-2：选项对话框保存/取消、选择文件夹、完成通知、失败通知（网址 http://127.0.0.1:1/nope.bin）、单实例
   - 5b-1 与 5a 遗留：暂停/继续（先把限速改成 200）、删除勾选与不勾选文件、托盘隐藏与恢复、托盘退出、执行 idm-cli runner stop 后程序自动退出、观察模式
5. 提交 hash
6. 与本文不一致之处

## 补充：第 3.5 步 任务详情面板（分块信息）

用户要求能看到每个下载连接的信息，类似 IDM 的分段显示。

### Rust 侧
1. 先调研：idm-core 是否已有公开 API 能获取某个任务的分块列表（每块的起止位置、已下载量、状态）以及每块的实时速度。在进度文件中记录调研结论
2. 新增 `DesktopService::task_detail(&self, id: i64) -> Result<TaskDetail, ServiceError>` 和对应 Tauri 命令 `task_detail`：
   ```rust
   #[derive(serde::Serialize, Clone)]
   pub struct ChunkView {
       pub index: usize,
       pub start: u64,
       pub end: u64,          // 包含
       pub downloaded: u64,
       pub speed_bps: u64,    // 拿不到实时速度时为 0
       pub state: String,     // "active" | "done" | "waiting" | "retrying"
   }
   #[derive(serde::Serialize, Clone)]
   pub struct TaskDetail {
       pub id: i64,
       pub total_size: Option<u64>,
       pub connections: usize,
       pub chunks: Vec<ChunkView>,
   }
   ```
   不得包含请求头、Cookie、完整 URL
3. 如果现有公开 API 拿不到分块数据：允许在 idm-core 中新增**只读**的查询接口（例如从数据库读取分块记录，或从队列读取活动任务的分块进度快照）。**如果需要修改下载、分块、重试的内部逻辑才能拿到数据，停下报告，不要改**
4. 测试 `task_detail_reports_chunks`：用慢速本地服务器以 4 个连接下载，中途调用 task_detail：分块数不少于 4；所有分块的范围首尾相接、覆盖整个文件且不重叠；各块 downloaded 之和与 list_tasks 中该任务的 downloaded 相差不超过 1 MiB；任务完成后再调用，所有块为 "done"。单独连续运行 10 次
5. 观察模式下也允许调用（只读）

### 前端
1. 表格下方增加详情面板，**仅在恰好选中一个任务时显示**，高度约 200px，可点标题栏右侧按钮折叠
2. 面板内容：
   - 标题行：文件名、总大小、连接数
   - 分段进度条：整条宽度代表整个文件，每个分块按其在文件中的位置和长度绘制，已下载部分填色（下载中蓝色、已完成绿色、重试中橙色），未下载部分浅灰；块与块之间留 1px 分隔
   - 连接表：编号、范围（自动换算单位）、已下载/总量、速度、状态（中文：下载中、已完成、等待、重试中）
3. 面板打开期间每 500 毫秒调用一次 task_detail；面板关闭或选中项变化时停止旧的定时器；组件卸载时清理
4. 大小未知（不支持分块）的任务显示"该任务为单连接下载，无分段信息"

### 手动验收清单增加
- 选中一个正在下载的大文件，详情面板显示多个分块，分段进度条各段同时推进，连接表速度在变化
- 选中已完成任务，所有分块为"已完成"
- 多选或不选时面板隐藏
