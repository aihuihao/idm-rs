; Phase 5c-1 卸载钩子：在 Tauri 删除任何文件/注册表项之前执行（NSIS_HOOK_PREUNINSTALL）。
;
; 1) 调用安装目录中的 idm-cli.exe 注销浏览器 Native Messaging host。
;    nm uninstall 默认 host 名为 idm_rs，只删除 HKCU 下的 host 注册表项与清单
;    （清单位于数据目录），不删除数据库、设置或下载记录。
;    调用失败（文件缺失/返回非零）一律忽略，不阻断卸载。
; 2) 删除 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 中的
;    idm-rs-desktop 与 idm-rs-runner 两个开机启动值；不存在时忽略。
;    注意：Tauri 默认模板只删除名为 ${PRODUCTNAME}(idm-rs) 的 Run 值，
;    这里补删实际使用的这两个值名。
; 3) 不删除数据目录：%LOCALAPPDATA%\idm-rs 中的下载记录、设置、日志全部保留。
;    本钩子不含任何针对数据目录的 RmDir/Delete 操作。

!macro NSIS_HOOK_PREUNINSTALL
  ; --- 1) 注销 Native Messaging host（默认 idm_rs）---------------------------
  IfFileExists "$INSTDIR\idm-cli.exe" 0 idmrs_nm_uninstall_done
    ExecWait '"$INSTDIR\idm-cli.exe" nm uninstall' $0
  idmrs_nm_uninstall_done:

  ; --- 2) 删除两个开机启动值（不存在时 DeleteRegValue 本身即忽略）----------
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "idm-rs-desktop"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "idm-rs-runner"

  ; --- 3) 数据目录保留：不删除 %LOCALAPPDATA%\idm-rs 下任何内容 --------------
!macroend
