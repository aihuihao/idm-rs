# 任务：idm-rs Phase 4b —— Native Messaging 注册与 Firefox 扩展

沿用全部工作规则。先把本任务原文保存为 `docs/prompts/phase4b.md`，`git status` 必须干净。基线为 4a 的提交。
遇到不明确的地方先停下报告。Firefox 的具体规则（清单字段、注册表位置、消息大小限制）如果你不确定，先查 MDN 官方文档确认，文档与本文冲突时停下报告。

## 一、注册命令（`idm-cli nm install / uninstall / status`）

1. `idm-cli nm install --extension-id <ID> [--host-name <名字>]`：
   - 默认 host 名 `idm_rs`（只允许小写字母、数字、下划线、点，符合 Firefox 的命名规则）
   - 在数据目录下写出清单文件 `<host名>.json`，内容：`name`、`description`、`path`（`idm-nmhost.exe` 的**绝对路径**，必须存在，否则报错）、`type: "stdio"`、`allowed_extensions: [<ID>]`
   - 注册表：`HKCU\Software\Mozilla\NativeMessagingHosts\<host名>`，默认值为清单文件的绝对路径。用 `reg.exe`（`std::process::Command`）读写，不新增依赖，不写 unsafe。只写 HKCU，绝不写 HKLM
   - 重复执行是幂等的（覆盖）
2. `nm uninstall [--host-name]`：删除注册表项和清单文件
3. `nm status [--host-name]`：打印清单路径、host 路径是否存在、注册表项是否存在、允许的扩展 ID
4. 扩展 ID 校验：非空，只允许字母、数字、`@`、`.`、`_`、`-`，长度不超过 128

## 二、Firefox 扩展（`extensions/firefox/`）

纯 JavaScript，不使用 npm、不放 package.json、不引入任何第三方库。

1. `manifest.json`：
   - Manifest V2（Firefox 仍支持，且后台页可持久运行，逻辑最简单）
   - `browser_specific_settings.gecko.id` 为 `idm-rs@local`
   - 权限只要这几项：`downloads`、`nativeMessaging`、`cookies`、`storage`、`<all_urls>`；不要多余权限
   - `background.scripts`、`browser_action`（工具栏按钮，点击切换开关）
2. **拦截逻辑**（`background.js`）：
   - 监听 `browser.downloads.onCreated`
   - 只处理 `http` / `https` 的 URL；`blob:`、`data:`、`file:` 等一律放行，由 Firefox 自己下载
   - 开关关闭时放行
   - 拦截流程：先用 `browser.runtime.sendNativeMessage` 发送 ping 确认 host 可用，可用才继续；然后收集 Cookie（`cookies.getAll({url})` 拼成 `name=value; ...`）、Referer（下载项的 `referrer`）、`navigator.userAgent`、文件名；发送 `enqueue`；收到 `ok:true` 后才 `downloads.cancel(id)` 并 `downloads.erase({id})`，让 Firefox 不再保留这个下载
   - **任何一步失败都不取消 Firefox 的下载**（host 没安装、返回错误、超时）：此时放行并在工具栏图标上显示一个失败标记。宁可让 Firefox 自己下载，也不能让下载丢失
   - 同一个 URL 重复触发（例如 Firefox 对同一资源连续创建）时去重，避免重复入队
3. 工具栏按钮：点击切换"接管开/关"，状态保存在 `storage.local`，图标/徽标显示当前状态
4. 扩展里不要记录 Cookie 的值到控制台
5. `extensions/firefox/README.md`：加载方法（`about:debugging` → 此 Firefox → 临时载入附加组件）、安装 host 的命令、卸载方法、常见问题（找不到 host、没反应）

## 三、自动测试（不需要 Firefox）

1. 注册命令的测试：用测试专用的 host 名（例如 `idm_rs_test_<随机数>`），install 后 `reg query` 能查到、清单内容正确；status 正确；uninstall 后注册表项和清单文件都不存在。测试结束必须清理，即使测试失败。绝不使用默认的 `idm_rs` 名
2. 非法输入：非法扩展 ID、非法 host 名、`idm-nmhost.exe` 不存在，都返回错误且不写注册表
3. 清单 JSON 的字段逐项断言（`allowed_extensions` 是数组，`path` 是绝对路径，没有 `allowed_origins`）
4. 扩展的静态检查（写在验收脚本里，纯 PowerShell）：`manifest.json` 是合法 JSON；`permissions` 恰好是上面 5 项；没有 `package.json` 和 `node_modules`；`background.js` 中包含 `sendNativeMessage` 和 `downloads.cancel`；不包含 `eval(`、`new Function`、`innerHTML`
5. 验收脚本现有的"禁止 package.json"检查要排除 `extensions/` 之外的位置不变，并在 `extensions/` 下同样保持"不得有 package.json"
6. 回归：全部原有测试、两个崩溃测试

## 四、提交与报告

1. 每个新测试单独连续运行 10 次；验收脚本全部 PASS
2. 只暂存 `docs crates scripts extensions ARCHITECTURE.md Cargo.lock`，提交信息 `phase4b: native messaging registration and firefox extension`，不得 push，不打 tag
3. 报告：自查表（测试名 + 10 次结果）；清单文件的实际内容（示例）；注册表项的实际内容；每项实现的位置；与本文不一致之处

**真实 Firefox 验收由用户手动完成，不需要你做，也不要安装到默认的 `idm_rs` 名下。**
