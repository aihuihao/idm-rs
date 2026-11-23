# 任务：idm-rs Phase 5c-3a —— 扩展提示改进、签名前准备、使用说明

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5c-3a.md
2. 允许联网的范围仅限：查阅 Mozilla 官方文档（MDN、extensionworkshop.com）确认扩展清单与签名要求。不得下载或安装任何工具（包括 web-ext），不得新增 npm 包或 crate
3. 不得自行把需要审批的命令改为免审批；审批超时或任何安全护栏拦截，立即停下报告，不得换方式重试
4. 不得结束任何不是你启动的进程；不得修改用户的数据目录与注册表
5. 遇到不明确之处、或任何检查不通过，停下报告
6. 每完成一步在 docs/phase5c-progress.md 追加一行

## 第 1 步：扩展的失败提示（background.js）
用户反馈：重装后浏览器集成未启用，扩展只显示 "last handoff failed"，看不出该怎么办。改为按失败原因给出中文提示（工具栏图标的标题文字）：
1. 找不到本机程序（sendNativeMessage 抛出的错误信息中包含 "No such native application" 或 "not found"，不区分大小写）：标题为"未连接到 idm-rs：请打开 idm-rs →选项→浏览器集成，点击启用"
2. 程序返回 runner_not_running：保持现有提示
3. 超时：标题为"idm-rs 无响应：请确认 idm-rs 正在运行"
4. 其他错误：标题为"交接失败，已交给 Firefox 下载"
5. 默认状态的标题从英文改为中文："idm-rs：接管已开启" / "idm-rs：接管已关闭"
6. 任何提示中不得包含网址、Cookie、请求头的值；控制台输出保持固定文字
7. phase4b-acceptance.ps1 的扩展静态检查必须继续全部通过；新增一项检查：background.js 中包含 "No such native application" 的判断

## 第 2 步：签名前检查
1. 查阅 Mozilla 官方文档，确认以下各项当前的要求，在报告中给出文档链接与结论：
   - 自行分发（unlisted）签名是否接受 Manifest V2
   - browser_specific_settings.gecko.id 的格式要求；idm-rs@local 是否符合
   - 新提交的扩展是否必须声明 data_collection_permissions（或其他新增的必填字段），以及本扩展应如何声明（本扩展不收集、不发送任何数据到任何服务器，只与本机程序通信）
   - 是否需要 strict_min_version，以及建议值
2. 按查到的要求修改 manifest.json。扩展 ID 保持 idm-rs@local；如果文档表明该 ID 不符合要求，停下报告，不要自行更换
3. 扩展版本号保持 0.1.0

## 第 3 步：打包
1. 新增脚本 scripts/package-extension.ps1：把 extensions/firefox 下的扩展文件（manifest.json、background.js 等运行所需文件，不含 README）打包为 dist/idm-rs-firefox-0.1.0.zip，文件位于 zip 根目录（不能多一层文件夹）
2. 脚本打包后自行检查：zip 根目录含 manifest.json；manifest 中的 version 与文件名一致；列出 zip 内全部文件
3. dist/ 加入 .gitignore

## 第 4 步：使用说明与版本说明
1. 新建 README.md（仓库根目录，中文），面向使用者，包含：功能简介；安装（运行安装包、"未知发布者"提示的处理）；首次使用（启用浏览器集成、安装 Firefox 扩展）；常用设置说明（每任务连接数、每台服务器最大连接数、全局限速、开机启动、继续时使用当前连接数）；卸载（数据保留在 %LOCALAPPDATA%\idm-rs）；常见问题（扩展显示"!"怎么办、下载没有被接管、观察模式横幅是什么意思）
2. 新建 docs/release-notes-0.1.0.md：主要功能列表；已知限制（只支持 Firefox；程序未做代码签名；开发阶段记录的遗留问题，从进度文件中整理，例如 CLI 的 enqueue --out 未生效、queue 没有 pause 子命令、host 读取消息无超时、Tauri 升级时需同步自定义安装模板）
3. 内容必须与实际行为一致，不确定的地方不要写

## 第 5 步：验证与提交
1. phase4b-acceptance.ps1、verify-workspace.ps1、phase4b-e2e.ps1 全部 PASS
2. 运行 scripts/package-extension.ps1，贴出 zip 文件清单与大小
3. 暂存 docs extensions scripts README.md .gitignore，提交信息 phase5c-3a: extension hints, signing prep, readme，不 push、不 amend、不打 tag

## 报告
1. 第 2 步每项的文档链接与结论；manifest.json 的修改（贴出修改后全文）
2. zip 路径、大小、文件清单
3. 验证结果、提交 hash
4. 给用户的签名操作步骤（写成照做即可的清单）：在 Mozilla 附加组件开发者中心登录、选择"自行分发"、上传 zip、等待自动审核、下载签名后的 .xpi、在 Firefox 中安装并移除临时载入的版本
5. 与本文不一致之处
