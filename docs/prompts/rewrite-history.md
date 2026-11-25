# 任务：改写提交作者信息、添加许可证（仅本地，不推送）

用户准备公开 GitHub 仓库。历史审查发现全部提交的作者与提交者为真实姓名和个人邮箱，需要在公开前改写。

## 新身份
- 名字：aihuihao
- 邮箱：5765999+aihuihao@users.noreply.github.com

## 规则（必须遵守）
1. 不得推送到任何远程；不得删除或修改远程配置
2. 不得自行把需要审批的命令改为免审批；审批超时或任何安全护栏拦截，立即停下报告，不得换方式重试
3. 联网仅限：从 polyformproject.org 官方网站获取 PolyForm Noncommercial License 1.0.0 的原文。不得安装任何工具（包括 git-filter-repo），只用 Git for Windows 自带的 git filter-branch
4. 任何核对项不符合预期，立即停下报告，不要尝试修复；备份可用于恢复，但恢复前也要先停下报告
5. 把本任务原文保存为 docs/prompts/rewrite-history.md，在最后的许可证提交中一并提交

## 第 1 步：记录与备份
1. 确认 git status 干净
2. 记录改写前的信息到临时文件（放在仓库外，例如 D:\IDM_RUST_rewrite_before.txt）：各分支与标签指向的提交编号；每个标签的类型（轻量或附注）、说明文字、打标签人；master 上全部提交按顺序的"提交编号 + 目录树编号（%T）"清单；提交总数
3. 完整备份：git clone --mirror D:\IDM_RUST D:\IDM_RUST_backup_before_rewrite.git。核对备份中的提交总数与标签数和原仓库一致
4. 贴出以上记录的摘要

## 第 2 步：改写
1. 只改写本地分支与标签，不改远程跟踪引用：使用 `-- --branches --tags` 而不是 `--all`
2. 用 Git for Windows 的 bash 执行 git filter-branch，设置环境变量 FILTER_BRANCH_SQUELCH_WARNING=1，--env-filter 中把 GIT_AUTHOR_NAME、GIT_AUTHOR_EMAIL、GIT_COMMITTER_NAME、GIT_COMMITTER_EMAIL 全部设为新身份；保留原有的作者时间与提交时间；使用 --tag-name-filter cat
3. 附注标签本身也带有打标签人信息，filter-branch 不会修改它。改写后对每个附注标签：读取它的说明文字与新的目标提交，删除后用新身份重新创建（git -c user.name=... -c user.email=... tag -a 名称 提交 -m 说明）。轻量标签只需确认已指向新提交
4. 删除 filter-branch 留下的 refs/original 下的引用

## 第 3 步：核对（全部满足才继续）
1. 提交总数与改写前相同
2. master 上全部提交按顺序的目录树编号（%T）与改写前的清单逐一相同（证明文件内容完全没变）
3. 在本地所有分支与标签可达的提交中，搜索作者名、作者邮箱、提交者名、提交者邮箱，不得再出现 张浩 或 haozhang1983@163.com
4. 每个附注标签的打标签人为新身份，说明文字与改写前相同，指向的提交与改写前对应位置的提交树相同
5. 每个标签都指向改写后的提交
6. git status 干净；贴出 git log --format="%h %an <%ae> %s" -5
如有任何一项不符，停下报告

## 第 4 步：本仓库今后的提交身份
在本仓库（不是全局）设置 user.name 与 user.email 为新身份，并确认 git config --get 的结果

## 第 5 步：许可证
1. 从 polyformproject.org 获取 PolyForm Noncommercial License 1.0.0 的完整原文，保存为仓库根目录的 LICENSE.md，不得改动条款文字。在文件开头加上一行：Required Notice: Copyright (c) 2026 aihuihao (https://github.com/aihuihao/idm-rs)
2. 新建 THIRD_PARTY_NOTICES.md：说明 apps/idm-desktop/src-tauri/nsis/installer.nsi 源自 Tauri CLI 2.12.5 的内置 NSIS 模板，Tauri 采用 MIT 或 Apache-2.0 双许可，附 Tauri 仓库地址与其 MIT 许可证全文（可从 Tauri 官方仓库的 LICENSE 文件获取；如果无法联网获取，停下报告，不要凭记忆编写）。另说明：程序依赖的各个开源库的许可证声明，将在后续版本中随安装包一并提供
3. README.md 增加"许可证"一节：本项目源码可见，采用 PolyForm Noncommercial 1.0.0，允许个人、学习、研究等非商业用途；商业用途需事先取得作者授权；第三方组件见 THIRD_PARTY_NOTICES.md。把 README 中把本项目称为"开源"的说法改为"源码可见"
4. 以新身份提交，信息 docs: add PolyForm Noncommercial license and third-party notices。不打新标签

## 报告
1. 改写前记录摘要、备份路径与核对结果
2. 第 3 步每一项核对结果与原始输出
3. 许可证文件来源网址；提交编号
4. 新旧标签对照表（名称、旧提交、新提交、类型）
5. 给用户的下一步：在 GitHub 与 Gitee 删除旧仓库并重建同名空私有仓库
6. 与本文不一致之处
