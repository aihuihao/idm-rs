# idm-rs Firefox 扩展（Phase 4b）

Manifest V2 扩展，把 Firefox 新建的 `http` / `https` 下载交给本机的
`idm-rs` Native Messaging host 入队。`blob:`、`data:`、`file:` 等一律由
Firefox 自己下载；开关关闭时也一样放行。

## host 名固定为 `idm_rs`

`background.js` 顶部的 `HOST_NAME` 常量固定为 `idm_rs`，扩展只会用这个名字
调用 `browser.runtime.sendNativeMessage`。因此：

- **安装给浏览器用时，直接 `idm-cli nm install --extension-id idm-rs@local`，
  不要传 `--host-name`。** 默认 host 名就是 `idm_rs`，与扩展一致。
- 若传了自定义的 `--host-name`，清单里的 `name`、注册表项名、扩展实际查找的
  名字三者不一致，浏览器会报 `No such native application idm_rs`，接管必然失败。
- 自定义 `--host-name` **仅用于自动化测试**（用随机专用名，结束时清理），
  不要用它做正式安装。

> 真实 Firefox 的加载与点击下载由用户手动完成；本文档不要求自动安装到浏览器。
> 自动化测试绝不写默认的 `idm_rs` 注册表项。

## 加载步骤（临时加载，不需要签名）

1. 构建 release 版 `idm-cli.exe` 与 `idm-nmhost.exe`，并确保两者在同一目录。
2. 执行 `idm-cli nm install --extension-id idm-rs@local`
   （不要加 `--host-name`）。可用 `idm-cli nm status` 检查：
   `manifest_exists: true`、`host_path_exists: true`、`registry_exists: true`、
   `allowed_extensions: idm-rs@local`。
3. **重启 Firefox**（注册表项只在启动时读取）。
4. 地址栏打开 `about:debugging#/runtime/this-firefox` → 「临时载入附加组件」
   (Load Temporary Add-on) → 选择本目录下的 `manifest.json`。
5. 工具栏出现 `idm-rs` 按钮，徽标 `ON`（绿）表示接管开启。

按钮点击可在 ON / OFF 之间切换，状态保存在 `storage.local`。
徽标 `!`（红）表示最近一次接管失败；此时 Firefox 的原始下载**未被取消**。

## 卸载步骤

1. 在 `about:debugging` 中移除临时附加组件（或直接关闭 Firefox）。
2. 执行 `idm-cli nm uninstall`（同样不要加 `--host-name`）。
   该命令删除 `HKCU\Software\Mozilla\NativeMessagingHosts\idm_rs` 注册表项
   和数据目录下的 `idm_rs.json` 清单文件。
3. 用 `idm-cli nm status` 确认 `manifest_exists: false`、
   `registry_exists: false`。
4. 再次重启 Firefox。

## 常见问题

**找不到 host / `Error: No such native application idm_rs`**

- 先确认安装时**没有**传 `--host-name`：扩展固定查找 `idm_rs`。
- 运行 `idm-cli nm status`，确认 `registry_exists: true`、
  `host_path_exists: true`、`manifest_exists: true`。
- 确认 `idm-rs@local` 出现在 `allowed_extensions` 里，且与扩展
  `browser_specific_settings.gecko.id` 完全一致。
- 安装后重启 Firefox。

**点击下载没反应 / 仍由 Firefox 下载**

- 看工具栏徽标：`OFF` 表示开关关闭，点一下打开；`!` 表示最近一次接管失败。
- 打开 `about:debugging` 的扩展控制台，会看到固定文本
  `idm-rs: handoff failed; keeping Firefox download`（不含任何 Cookie 值）。
- **失败后 30 秒内扩展不再尝试接管**（不 ping、不发送原生消息），新下载直接
  由 Firefox 处理，徽标保持 `!`；30 秒后自动恢复尝试。退避由 `lastFailAt`
  控制，切换工具栏按钮不会清除它，请等待 30 秒或重新加载附加组件。
  成功接管一次会立刻清除失败状态。
- 同一 URL 在 10 秒内重复触发会被去重；接管失败时会清掉去重记录，
  因此 30 秒退避结束后用户重试不会被去重挡住。
- 确认 `idm-nmhost.exe` 存在且能被启动；确认后台常驻运行器已运行
  （`idm-cli queue run --watch`），否则任务只会入队不会开始下载。
- 只有 `http` / `https` 会被接管；其它 scheme 按设计放行。

**会不会丢下载？**

不会。扩展先 ping host，再发送 `enqueue`，只有收到 `{"ok":true}` 才调用
`downloads.cancel`；随后尽力 `downloads.removeFile`（失败忽略）、
`downloads.erase` 清理历史记录。host 未安装、返回错误、超时或任何异常都
直接进入失败分支，Firefox 的原始下载保持不动。

## 说明

- 纯 JavaScript，无 npm、无 `package.json`、无第三方库、无构建步骤。
- 扩展不记录 Cookie 的值到控制台；Cookie 只在 `sendNativeMessage` 的
  `enqueue` 消息里传给本机 host。
- 真实 Firefox 端到端验收需手动完成（临时加载 + 触发一次下载）。
