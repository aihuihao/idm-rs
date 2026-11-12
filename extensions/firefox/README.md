# idm-rs Firefox 扩展（Phase 4b）

Manifest V2 扩展，把 Firefox 新建的 `http` / `https` 下载交给本机的
`idm-rs` Native Messaging host 入队。`blob:`、`data:`、`file:` 等一律由
Firefox 自己下载；开关关闭时也一样放行。

## 加载方法（临时加载，不需要签名）

1. 在 Firefox 地址栏打开 `about:debugging#/runtime/this-firefox`。
2. 点击「临时载入附加组件」(Load Temporary Add-on)。
3. 选择本目录下的 `manifest.json`（任选目录内一个文件亦可）。
4. 工具栏出现 `idm-rs` 按钮，徽标 `ON`（绿）表示接管开启。

按钮点击可在 ON / OFF 之间切换，状态保存在 `storage.local`。
徽标 `!`（红）表示最近一次接管失败；此时 Firefox 的原始下载**未被取消**。

## 安装 Native Messaging host

扩展的 `browser_specific_settings.gecko.id` 是 `idm-rs@local`，命令里的
`--extension-id` 必须与它一致：

```
idm-cli nm install --extension-id idm-rs@local
```

该命令：

- 在数据目录（数据库所在目录）写出 `idm_rs.json`；
- 写 `HKCU\Software\Mozilla\NativeMessagingHosts\idm_rs`，
  默认值为清单文件的绝对路径；
- 清单的 `path` 指向与 `idm-cli.exe` 同目录的 `idm-nmhost.exe`，
  文件不存在时报错且不写注册表。

重复执行是幂等的（覆盖）。查看当前状态：

```
idm-cli nm status
```

卸载（删除注册表项与清单文件）：

```
idm-cli nm uninstall
```

安装后需要**重启 Firefox** 才会读到新的注册表项。

## 常见问题

**找不到 host / `Error: No such native application idm_rs`**

- 运行 `idm-cli nm status`，确认 `registry_exists: true`、
  `host_path_exists: true`、`manifest_exists: true`。
- 确认 `idm-rs@local` 出现在 `allowed_extensions` 里，且与扩展
  `browser_specific_settings.gecko.id` 完全一致。
- 安装后重启 Firefox。
- 若用了 `--host-name` 自定义名字，扩展里的 `HOST_NAME` 常量
  （`background.js` 顶部）也要同步改成同样的名字。

**点击下载没反应 / 仍由 Firefox 下载**

- 看工具栏徽标：`OFF` 表示开关关闭，点一下打开；`!` 表示最近一次接管失败。
- 打开 `about:debugging` 的扩展控制台，会看到固定文本
  `idm-rs: handoff failed; keeping Firefox download`（不含任何 Cookie 值）。
- 确认 `idm-nmhost.exe` 存在且能被启动；确认后台常驻运行器已运行
  （`idm-cli queue run --watch`），否则任务只会入队不会开始下载。
- 只有 `http` / `https` 会被接管；其它 scheme 按设计放行。

**会不会丢下载？**

不会。扩展先 ping host，再发送 `enqueue`，只有收到 `{"ok":true}` 才调用
`downloads.cancel` 和 `downloads.erase`。host 未安装、返回错误、超时或
任何异常都直接进入失败分支，Firefox 的原始下载保持不动。

## 说明

- 纯 JavaScript，无 npm、无 `package.json`、无第三方库、无构建步骤。
- 扩展不记录 Cookie 的值到控制台；Cookie 只在 `sendNativeMessage` 的
  `enqueue` 消息里传给本机 host。
- 真实 Firefox 端到端验收需手动完成（临时加载 + 触发一次下载）。
