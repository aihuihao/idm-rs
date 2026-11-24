# idm-rs 维护说明

## 1. 自定义 NSIS 安装模板

仓库包含两个自定义 NSIS 文件：

- `apps/idm-desktop/src-tauri/nsis/installer.nsi`：安装程序模板（Tauri Handlebars 语法，含 `{{product_name}}`、`{{#if installer_hooks}}` 等占位符）。
- `apps/idm-desktop/src-tauri/nsis/hooks.nsh`：卸载钩子 `NSIS_HOOK_PREUNINSTALL`。

**模板来源版本**：模板从 Tauri CLI 2.x 内置 NSIS 模板提取，具体为本仓库 `apps/idm-desktop/package-lock.json` 锁定的 `@tauri-apps/cli` **2.12.5**（`package.json` 声明 `^2`）。仓库内没有单独记录"提取时所用 CLI 版本"的文件；上述版本依据锁定的 CLI 版本确定，升级 CLI 时应按第 2 节重新核对模板。模板文件头还保留 Tauri 上游 PR 链接（installer.nsi 中的 PR #10106、#15422）与 NSIS 官方文档链接。

**我们的修改位置与原因**：

- `installer.nsi` 第 546–550 行，`currentUser` 安装模式分支：把默认 `$LOCALAPPDATA\${PRODUCTNAME}` 改为 `$LOCALAPPDATA\Programs\${PRODUCTNAME}`（即 `%LOCALAPPDATA%\Programs\idm-rs`）。原因：数据目录为 `%LOCALAPPDATA%\idm-rs\idm.db`（见 `Store::default_db_path`），若安装目录仍为 `%LOCALAPPDATA%\idm-rs` 会与数据目录重叠，卸载时可能误删数据；改到 `Programs\` 子目录实现程序与数据分离。
- `hooks.nsh` 第 14–25 行，卸载前钩子：调用 `idm-cli nm uninstall` 注销 Native Messaging host（注册表 + 清单），并删除 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `idm-rs-desktop`、`idm-rs-runner` 两个值。原因：Tauri 默认模板只删除名为 `${PRODUCTNAME}`（`idm-rs`）的 Run 值，与实际写入的两个值名不符；钩子不删除 `%LOCALAPPDATA%\idm-rs` 下任何内容，以保留下载记录、设置与日志供重装后继续使用。

`tauri.conf.json` 中 `bundle.windows.nsis` 通过 `nsis.template = "nsis/installer.nsi"` 与 `nsis.installerHooks = "nsis/hooks.nsh"` 引用这两个文件，`installMode` 为 `currentUser`；`bundle.resources` 把签名 xpi 映射为安装目录下的 `idm-rs-firefox.xpi`。

## 2. 升级 Tauri 时的同步步骤

1. **重新提取新版本模板**：从新版本 Tauri CLI 的 NSIS 模板重新生成 `installer.nsi`（替换仓库中的模板），保留 `{{...}}` Handlebars 占位符原样；对照 CLI 安装目录下 NSIS 资源中的默认模板。
2. **重新应用我们的修改**：
   - 在 `currentUser` 分支把 `StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"` 改为 `StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${PRODUCTNAME}"`（第 1 节的安装目录一行）。
   - 确认 `installerHooks` 仍指向 `nsis/hooks.nsh`，且 `hooks.nsh` 中的 `nm uninstall` 与两个 Run 值删除逻辑仍存在。
   - 核对 `installer.nsi` 中 `{{#if installer_hooks}}` 相关钩子调用块未被新模板移除。
3. **生成并检查**：运行 `pwsh -File scripts/build-release.ps1` 生成安装包；检查 `target\release\bundle\nsis` 下生成的 `.nsi`（构建中间产物）中：
   - 安装目录为 `$LOCALAPPDATA\Programs\idm-rs`（不是 `$LOCALAPPDATA\idm-rs`）；
   - 卸载钩子包含 `idm-cli nm uninstall` 与两个 Run 值的删除语句；
   - 确认安装包 `idm-rs_<版本>_x64-setup.exe` 生成成功、大小正常。

## 3. 更新 Firefox 扩展（随安装包分发）

签名后的扩展作为资源随安装包安装到 `%LOCALAPPDATA%\Programs\idm-rs\idm-rs-firefox.xpi`（安装目录根，文件名固定为 `idm-rs-firefox.xpi`，与扩展自身版本号无关）。更新步骤如下：

1. **修改扩展**：在 `extensions/firefox/` 修改 `manifest.json` / `background.js`，按需提升 `manifest.json` 中的 `version`（扩展版本独立于程序版本；程序升级但扩展未改时扩展版本保持不变）。
2. **打包**：运行 `pwsh -File scripts/package-extension.ps1` 生成 `dist/idm-rs-firefox-<扩展版本>.zip`，自检通过后提交给 Mozilla 签名。
3. **签名**：把 zip 提交到 Mozilla 附加组件开发者中心的"自行分发"（unlisted）签名；下载签名后的 `.xpi`（文件名含扩展版本）。
4. **放入 dist**：把签名 xpi 放入仓库根 `dist/` 下（文件名形如 `idm-rs-firefox-<扩展版本>-signed.xpi`）；`dist/` 与 `*.xpi` 已被 `.gitignore` 忽略，不入库。
5. **更新 SHA256（单一位置）**：`scripts/build-release.ps1` 顶部的 `$ExtensionSha256` 是期望哈希的唯一来源；`$ExtensionSource` 指向 `dist/` 下的签名文件名。更新扩展时只改这两行（哈希与源文件名），不要在其他文件重复硬编码哈希。若源文件名变化，同时更新 `tauri.conf.json` 的 `bundle.resources` 中暂存文件名无需改动（`build-release.ps1` 会把源复制为固定的 `resources/idm-rs-firefox.xpi`）。
6. **重新打包安装程序**：运行 `pwsh -File scripts/build-release.ps1`——该脚本先确认签名 xpi 存在且 SHA256 与 `$ExtensionSha256` 一致（不一致即失败退出），再复制到 `apps/idm-desktop/src-tauri/resources/idm-rs-firefox.xpi`，由 `bundle.resources` 打包进 NSIS 安装包。检查生成的 `target\release\nsis\x64\installer.nsi` 中含 `File /a "/oname=idm-rs-firefox.xpi"` 与卸载 `Delete "$INSTDIR\idm-rs-firefox.xpi"`。

## 4. 发布新版本检查清单

1. **版本号**：`Cargo.toml`（workspace `version`）、`crates/*/Cargo.toml` 与 `apps/idm-desktop/src-tauri/Cargo.toml`（`version.workspace = true`）、`apps/idm-desktop/src-tauri/tauri.conf.json`、`apps/idm-desktop/package.json` 均为目标版本；Firefox 扩展 `extensions/firefox/manifest.json` 的版本单独维护（未改动时保持原版本）。
2. **验收脚本**：依次全部通过——`verify-workspace.ps1`、`phase4a-acceptance.ps1`、`phase4b-acceptance.ps1`、`phase4b-e2e.ps1`；新测试按规则做 10 次重复与变异验证。
3. **安装包**：`build-release.ps1` 生成 `idm-rs_<版本>_x64-setup.exe`，确认大小与安装目录/卸载钩子（第 2 节）。
4. **扩展打包与签名**：`package-extension.ps1` 生成 `dist/idm-rs-firefox-<版本>.zip`；经 Mozilla 自行分发（unlisted）签名得到 `.xpi`，记录文件名、大小、SHA256；签名文件不入库（`.gitignore` 忽略 `*.xpi` 与 `/dist/`）。
5. **版本说明**：新建 `docs/release-notes-<版本>.md`，列本次修复与已知限制，并从旧版本说明中移除已修复项；更新 `README.md` 的下载/安装/版本引用。
6. **标签**：提交后创建 annotated 标签 `git tag -a v<版本> -m "idm-rs v<版本>"`，用 `git cat-file -p v<版本>` 确认指向发布提交；按约定不 push（除非另有授权）。
