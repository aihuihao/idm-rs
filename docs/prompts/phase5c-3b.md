# 任务：idm-rs Phase 5c-3b —— 最终验证与 v0.1.0 版本标签

## 规则（必须遵守）
1. 先确认 git status 干净，把本任务原文原样保存为 docs/prompts/phase5c-3b.md
2. 不联网，不新增依赖，不修改产品代码
3. 不得自行把需要审批的命令改为免审批；审批超时或任何安全护栏拦截，立即停下报告，不得换方式重试
4. 不得结束任何不是你启动的进程；不得修改用户的数据目录、注册表与 Firefox 配置
5. 任何验证失败，停下报告，不得修改测试断言、不得修复代码
6. 每完成一步在 docs/phase5c-progress.md 追加一行

## 第 1 步：记录签名扩展
签名文件：D:\IDM_RUST\dist\idm-rs-firefox-0.1.0-signed.xpi，大小 10894 字节，SHA256 D918224C3DF5800289A8311245D555A4328053C2B8AD980CDD229729C60F3227，由 Mozilla 以"自行分发"方式签名（扩展 ID idm-rs@local，版本 0.1.0）。
1. 重新计算该文件 SHA256，必须与上面一致，否则停下报告
2. 写入 docs/release-notes-0.1.0.md：签名扩展的文件名、大小、SHA256、签名方式
3. 更新 README.md 中安装 Firefox 扩展的部分：改为把签名后的 .xpi 文件拖入 Firefox 窗口安装；说明安装确认窗口中"允许在隐私窗口中运行"建议不勾选及原因（隐私窗口中的下载会被记入 idm-rs 的下载记录）；删除或改写"临时载入"的说明，仅作为开发者调试方法保留在开发者附录中

## 第 2 步：全部验收脚本
1. 列出 scripts/ 目录下的全部脚本，说明各自用途
2. 依次完整运行所有验收与验证类脚本（至少包括 verify-workspace.ps1、phase4b-acceptance.ps1、phase4b-e2e.ps1、package-extension.ps1、build-release.ps1），贴出每个的最终结果与退出码
3. 对不属于验收类的脚本（如历史阶段的专用脚本）逐一判断是否仍可运行、是否已过时；过时的只列出，不修改
4. 任何一个失败，停下报告

## 第 3 步：版本一致性
确认以下各处版本号均为 0.1.0，列出文件与行号：workspace Cargo.toml、各 crate、tauri.conf.json、package.json、扩展 manifest.json、安装包文件名、release notes

## 第 4 步：提交与标签
1. 暂存 docs README.md，提交信息 release: v0.1.0 notes and readme，不 push、不 amend
2. 确认 git status 干净
3. 创建带说明的标签：git tag -a v0.1.0 -m "idm-rs v0.1.0"，标签指向上一步的提交
4. 贴出 git log --oneline -5 与 git show v0.1.0 --stat 的输出
5. 不 push 提交，也不 push 标签

## 报告
1. 第 1 步校验结果
2. 第 2 步脚本清单、各自运行结果；过时脚本清单
3. 第 3 步版本号清单
4. 提交 hash、标签信息
5. 与本文不一致之处
