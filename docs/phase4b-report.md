# Phase 4b 提交前自查报告

任务原文：`docs/prompts/phase4b.md`。基线 HEAD：`4860907`（4b 前置改动
`phase4b-pre: reject URL credentials in nmhost enqueue`）；4a 基线 `97babb2`。
所有构建/测试均使用 `--offline`，未执行 `cargo update`；未 push、未打 tag。

## 一、MDN 官方规则核对（任务第 4 行要求）

开工前按任务要求查了 MDN 官方文档，与任务原文**无冲突**：

| 项 | MDN 原文要点 | 任务原文 | 判定 |
|---|---|---|---|
| 清单字段 | `name`、`description`、`path`、`type:"stdio"`、`allowed_extensions`（Chrome 才是 `allowed_origins`） | 完全一致 | 无冲突 |
| host 名 | `^\w+(\.\w+)*$`，可含大小写字母数字下划线点；不得以点开头/结尾，不得连续点 | 只允许小写字母、数字、下划线、点 | 任务是更严格的子集；实现按 `小写 + 不得以点开头/结尾 + 不得连续点`（见第六节第 1 条） |
| 注册表 | per-user `HKEY_CURRENT_USER\SOFTWARE\Mozilla\NativeMessagingHosts\<name>`，默认值为清单路径 | `HKCU\Software\Mozilla\NativeMessagingHosts\<host名>`，默认值为清单绝对路径 | 无冲突 |
| 消息大小 | host→扩展单条 ≤ 1 MB；扩展→host ≤ 4 GB；帧为 4 字节无符号长度（本机字节序）+ UTF-8 JSON | 4a 已实现 64 KiB 上限 | 4a 的 64 KiB 是更严格的应用侧限制，不违反 MDN |
| MV2 | MDN Native messaging 官方示例本身即 `manifest_version: 2` + `background.scripts` + `browser_action` | 要求 Manifest V2 | 无冲突 |
| 扩展 ID | `^[a-zA-Z0-9-._]*@[a-zA-Z0-9-._]+$`，建议 ≤ 80 字符 | 校验 ≤ 128 字符；实际 ID `idm-rs@local`（11 字符）符合 | 校验上限更宽松，实际值合规（见第六节第 2 条） |

MDN 页面：`Native_manifests`、`Native_messaging`、`manifest_version`、
`browser_specific_settings`（developer.mozilla.org，均为 Mozilla 官方文档）。

## 二、自查表（每个新测试单独连续运行 10 次）

`scripts/phase4b-acceptance.ps1 -Section all -Repeat 10`：
`SUMMARY: all 12 items reached 10/10 : PASS`，`ACCEPT_EXIT=0`（日志 `target/accept-4b.out`）。
判定标准：cargo run 退出码 0 **且**输出含 `1 passed`；静态检查项为纯 PowerShell 判定。

| 任务项 | 检查内容 | 测试名 / 检查名 | 结果 |
|---|---|---|---|
| 3.4 | `manifest.json` 是合法 JSON，MV2 / gecko.id / background.scripts / browser_action | `static_manifest_json_is_valid` | 10/10 PASS |
| 3.4 | `permissions` 恰好 5 项、无多余无重复 | `static_permissions_are_exactly_five` | 10/10 PASS |
| 3.4 / 3.5 | 全仓（含 `extensions/`）不得有 `package.json`，`extensions/` 下不得有 `node_modules` | `static_no_package_json_anywhere` | 10/10 PASS |
| 3.4 | `background.js` 含 `sendNativeMessage`、`downloads.cancel`、`downloads.erase`、`onCreated`、徽标 | `static_background_required_calls` | 10/10 PASS |
| 3.4 | `background.js` 不含 `eval(` / `new Function` / `innerHTML` | `static_background_no_forbidden_code` | 10/10 PASS |
| 1 / 3.1 | install 后 `reg query` 查得到；status 正确；uninstall 后注册表项与清单都不存在；失败也清理 | `nm_install_registers_manifest_and_registry` | 10/10 PASS |
| 3.3 | 清单字段逐项断言：恰好 5 字段、`allowed_extensions` 是数组且 1 项、`path` 绝对且存在、无 `allowed_origins` | `nm_manifest_json_fields_are_exact` | 10/10 PASS |
| 3.2 | 非法扩展 ID（空/空格/斜杠/冒号/换行/引号/129 字符）返回错误且不写注册表 | `nm_rejects_invalid_extension_id` | 10/10 PASS |
| 3.2 | 非法 host 名（空/大写/连字符/首尾点/连续点/空格）返回错误且不写注册表 | `nm_rejects_invalid_host_name` | 10/10 PASS |
| 3.2 | `idm-nmhost.exe` 不存在返回错误且不写注册表/清单 | `nm_rejects_missing_host_exe` | 10/10 PASS |
| 1 | 重复 install 幂等（清单与注册表值一致，`allowed_extensions` 不累加） | `nm_install_is_idempotent` | 10/10 PASS |
| 1.3 | uninstall 后 status 报告 `manifest_exists:false` / `registry_exists:false` | `nm_status_after_uninstall_reports_absent` | 10/10 PASS |

**新增测试共 12 项（5 个静态检查项 + 7 个 Rust 集成测试），120 次运行全部 PASS。**
测试用 `idm_rs_test_<pid>_<nanos>_<seq>` 专用 host 名，绝不使用默认 `idm_rs`；
`RegGuard` 的 `Drop` 保证断言失败时也删除注册表项与清单文件。

## 三、清单文件的实际内容（示例）

`target/demo-4b.out` 的实测输出（演示用 host 名 `idm_rs_demo_11016`，非默认名；
演示结束后已 uninstall 并清理）。`idm-cli nm install --extension-id idm-rs@local
--host-name idm_rs_demo_11016 --db <临时>\data\idm.db` 写出的
`idm_rs_demo_11016.json`：

```json
{
  "name": "idm_rs_demo_11016",
  "description": "idm-rs Native Messaging host",
  "path": "R:\\Temp\\idm4b_demo_11016\\bin\\idm-nmhost.exe",
  "type": "stdio",
  "allowed_extensions": ["idm-rs@local"]
}
```

`path` 是绝对路径、指向真实存在的 `idm-nmhost.exe`；没有 `allowed_origins`。

## 四、注册表项的实际内容

同一演示的 `reg query HKCU\Software\Mozilla\NativeMessagingHosts\idm_rs_demo_11016 /ve`：

```
HKEY_CURRENT_USER\Software\Mozilla\NativeMessagingHosts\idm_rs_demo_11016
    (默认)    REG_SZ    R:\Temp\idm4b_demo_11016\data\idm_rs_demo_11016.json
```

只写 HKCU，默认值是清单文件的绝对路径。`idm-cli nm status` 实测输出：

```
host_name: idm_rs_demo_11016
manifest: R:\Temp\idm4b_demo_11016\data\idm_rs_demo_11016.json
manifest_exists: true
host_path: R:\Temp\idm4b_demo_11016\bin\idm-nmhost.exe
host_path_exists: true
registry_key: HKCU\Software\Mozilla\NativeMessagingHosts\idm_rs_demo_11016
registry_exists: true
registry_value: HKEY_CURRENT_USER\Software\Mozilla\NativeMessagingHosts\idm_rs_demo_11016
    (默锟斤拷)    REG_SZ    R:\Temp\idm4b_demo_11016\data\idm_rs_demo_11016.json
allowed_extensions: idm-rs@local
```

`uninstall` 后实测：`MANIFEST_EXISTS_AFTER=False`、`REG_KEY_EXISTS_AFTER=False`
（脚本 `DEMO_EXIT=0`）。`registry_value` 里的 `(默认)` 在 PowerShell 5.1 控制台
按 GBK 解码 reg.exe 的 UTF-16 输出显示为乱码，仅显示问题，注册表值本身正确。

## 五、每项实现的位置

| 任务项 | 位置 |
|---|---|
| 1.1 `nm install`（默认 host 名、校验、清单、HKCU 注册表、幂等） | `crates/idm-cli/src/main.rs` `mod nm`：`validate_host_name`、`validate_ext_id`、`data_dir`、`manifest_path`、`json_escape`、`install` |
| 1.2 `nm uninstall` | `mod nm::uninstall`（`reg delete /f` + 删除清单） |
| 1.3 `nm status` | `mod nm::status`（`reg query /ve` + 从清单提取 `allowed_extensions`） |
| 1.4 扩展 ID 校验 | `mod nm::validate_ext_id` |
| 注册表读写 | 全部 `mod nm::reg` 包装 `reg.exe`，无新依赖、无 `unsafe`、绝不写 HKLM |
| 命令分发 | `main()` 中 `if sub == "nm"`（在打开数据库之前，注册命令不依赖 DB） |
| 2.1 扩展清单 | `extensions/firefox/manifest.json` |
| 2.2 拦截逻辑 | `extensions/firefox/background.js`（`handoff`、`withTimeout`、`cookieHeader`、`basename`） |
| 2.3 工具栏开关 / 徽标 | `extensions/firefox/background.js`（`onClicked`、`paint`、`badge`） |
| 2.4 不记录 Cookie 值 | `background.js` 的 catch 只输出固定文本，不打印 URL / Cookie / 请求头 |
| 2.5 加载说明 | `extensions/firefox/README.md` |
| 3.1–3.3 自动测试 | `crates/idm-cli/tests/phase4b.rs`（7 个测试 + `RegGuard` 清理） |
| 3.4 / 3.5 扩展静态检查 | `scripts/phase4b-acceptance.ps1` 的 5 个 `Test-*` 函数；`scripts/verify-workspace.ps1` STEP 10 保留原有 3 项检查并新增 `extensions/` 下的 `package.json` 检查 |
| 验收脚本 | `scripts/phase4b-acceptance.ps1` |

## 六、与任务原文不一致之处（如实说明）

1. **host 名比任务原文多两条约束**：任务说「只允许小写字母、数字、下划线、点，
   符合 Firefox 的命名规则」；MDN 的 `^\w+(\.\w+)*$` 还要求不得以点开头/结尾、
   不得有连续点。实现按 MDN 补上这两条（更严格，不会拒绝任务允许的名字中
   合法的那部分）。MDN 本身允许大写字母，按任务要求仍只允许小写。
2. **扩展 ID 校验上限 128 与 MDN 建议 80**：按任务原文实现 ≤128；实际使用的
   `idm-rs@local`（11 字符）符合 MDN 的 `^[a-zA-Z0-9-._]*@[a-zA-Z0-9-._]+$`。
   MDN 的 80 是「recommended」而非强制。
3. **`--db` 是新增的可选参数**：任务原文的命令格式未列出 `--db`。为让测试把
   清单写进临时数据目录（不污染真实 `%LOCALAPPDATA%\idm-rs`），`nm` 支持
   `--db <path>`；未指定时依次回退 `IDM_RS_DB` 环境变量、`default_db_path()` 的
   父目录。这与工作区其它子命令的 `--db` 惯例一致。
4. **测试 fixture 复制 CLI 与占位 host 二进制**：`idm-nmhost.exe` 的绝对路径按
   「与 `idm-cli.exe` 同目录」解析。测试把 CLI 复制到临时 `bin/` 目录，并按需
   放置同名占位文件，从而确定性地覆盖「host 存在」与「host 不存在」两种分支，
   不改动 `target/debug/` 里的真实产物。
5. **重复触发去重用时间窗口**：任务只说「同一个 URL 重复触发时去重」，`background.js`
   实现为 10 秒窗口（`DEDUP_WINDOW_MS`）内同一 URL 只处理一次；`ping`/`enqueue`
   另有 5s/8s 超时保护。这些常量是任务未规定的实现细节。
6. **接管开关默认开启**：任务未规定初始值，`background.js` 默认 `enabled = true`
   （徽标 `ON`），状态持久化在 `storage.local` 的 `idmRsEnabled`。
7. **`registry_value` 行含中文默认值的控制台乱码**：`nm status` 把 `reg query`
   原始输出整行打印；Windows 中文版 reg.exe 输出「(默认)」在 PS 5.1 控制台
   会乱码。字段本身可判定，未做转码，避免引入编码依赖。
8. **`verify-workspace.ps1` STEP 10 被扩展**：按任务 3.5 要求，原有 3 项检查
   位置不变，新增 `extensions/` 下不得有 `package.json`；`node_modules` 检查
   放在 `phase4b-acceptance.ps1`（不改动既有 STEP 10 的其它语义）。
9. **真实 Firefox 端到端验收由用户手动完成**：任务明确说明，本次未启动 Firefox、
   未临时载入扩展、未向默认 `idm_rs` 名安装。注册命令写的是 HKCU 真实注册表，
   但所有测试与演示都用随机专用名并在结束时清理。

## 七、验证结果

- `scripts/phase4b-acceptance.ps1 -Section all -Repeat 10`：`SUMMARY: all 12 items reached 10/10 : PASS`，`ACCEPT_EXIT=0`
- `scripts/verify-workspace.ps1`：`SUMMARY: all 16 steps executed; final result: PASS`，`SCRIPT_EXIT=0`（日志 `target/vw-4b-final.out`）
- 全量回归 `cargo test --workspace --offline`：**253 passed / 0 failed**，`REG_EXIT=0`（日志 `target/reg-4b.out`）
- 两个崩溃测试 `cargo test -p idm-cli --offline --test crash_resume -- --ignored --test-threads=1`：
  **2 passed / 0 failed**，`CRASH_EXIT=0`（`crash_resume_twenty_rounds`、
  `crash_resume_with_split_twenty_rounds`，90.55s，日志 `target/crash-4b.out`）
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --offline -- -D warnings`：exit 0
- 演示脚本 `target/demo-4b.ps1`：`DEMO_EXIT=0`，install / reg query / status / uninstall 全链路实测
- 工作区干净，未 push、未打 tag
