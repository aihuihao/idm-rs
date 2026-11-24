# phase4a-acceptance.ps1
# Phase 4a acceptance: run every NEW test individually, 10 consecutive times.
#
# - ASCII-only comments/output (avoid PowerShell 5.1 Chinese mojibake).
# - Compatible with PowerShell 5.1 / 7.x.
# - All cargo invocations use --offline (no network), per the Phase 4a rules.
# - A run counts as PASS only when exit code is 0 AND output has "1 passed".
#   (cargo test with an --exact filter that matches nothing still exits 0, so
#    we must also check that exactly one test ran.)
# - All items must reach 10/10, otherwise exit 1.
# - validate_url tests now live in idm-core (net::url_check::url_tests), not the
#   idm-nmhost binary; host_spawns_runner_once was removed because the host no
#   longer spawns the runner (Phase 4c+ uses an independent queue run --watch).
#
# Usage:
#   powershell -File scripts/phase4a-acceptance.ps1                 # all sections
#   powershell -File scripts/phase4a-acceptance.ps1 -Section frame # one section
#   powershell -File scripts/phase4a-acceptance.ps1 -Repeat 10

param(
    [ValidateSet('all', 'frame', 'ipc', 'core', 'cli')]
    [string]$Section = 'all',
    [int]$Repeat = 10
)

$ErrorActionPreference = 'Stop'

# Id = Phase 4a spec section 6 item number.
# Pkg / Target / Name = how to run exactly one test.
$tests = @(
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::frame_roundtrip_normal' }
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::frame_zero_length' }
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::frame_over_limit' }
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::frame_truncated_then_eof' }
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::frame_multiple' }
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::frame_write_then_read' }
    [pscustomobject]@{ Id = 1;  Section = 'frame'; Pkg = 'idm-ipc';  Target = @('--lib');              Name = 'frame::tests::write_frame_failure_emits_no_bytes' }

    [pscustomobject]@{ Id = 2;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'host_ping_roundtrip' }
    [pscustomobject]@{ Id = 3;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'host_enqueue_creates_queued_task' }
    [pscustomobject]@{ Id = 4;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'host_rejects_bad_input' }
    [pscustomobject]@{ Id = 4;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'host_rejects_oversized_message' }
    [pscustomobject]@{ Id = 4;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'host_url_validation_cases' }
    [pscustomobject]@{ Id = 4;  Section = 'ipc';   Pkg = 'idm-core';  Target = @('--lib');              Name = 'net::url_check::url_tests::validate_url_returns_for_three_inputs' }
    [pscustomobject]@{ Id = 4;  Section = 'ipc';   Pkg = 'idm-core';  Target = @('--lib');              Name = 'net::url_check::url_tests::validate_url_rejects_all_userinfo_forms' }
    [pscustomobject]@{ Id = 4;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'host_enqueue_rejects_url_credentials' }
    [pscustomobject]@{ Id = 5;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Feature = 'test-hooks'; Name = 'host_stdout_contains_only_frames' }
    [pscustomobject]@{ Id = 5;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Feature = 'test-hooks'; Name = 'host_stdout_single_ping_frame_then_panic' }
    [pscustomobject]@{ Id = 5;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'default_build_ignores_panic_env' }
    [pscustomobject]@{ Id = 6;  Section = 'ipc';   Pkg = 'idm-ipc';  Target = @('--test', 'nmhost');    Name = 'filename_is_sanitized' }
    # Id 13 (host_spawns_runner_once) removed: the host no longer spawns the runner
    # (Phase 4c+ moved the runner to an independent `idm-cli queue run --watch`
    # process); the corresponding test no longer exists in the codebase.

    [pscustomobject]@{ Id = 7;  Section = 'core';  Pkg = 'idm-core'; Target = @('--test', 'phase4a');   Name = 'headers_reach_server_including_retry' }
    [pscustomobject]@{ Id = 7;  Section = 'core';  Pkg = 'idm-core'; Target = @('--test', 'phase4a');   Name = 'headers_reach_server_on_resume' }
    [pscustomobject]@{ Id = 8;  Section = 'core';  Pkg = 'idm-core'; Target = @('--test', 'phase4a');   Name = 'headers_cleared_on_completion' }
    [pscustomobject]@{ Id = 9;  Section = 'core';  Pkg = 'idm-core'; Target = @('--test', 'phase4a');   Name = 'cookie_not_in_logs' }
    [pscustomobject]@{ Id = 10; Section = 'core';  Pkg = 'idm-core'; Target = @('--test', 'phase4a');   Name = 'invalid_headers_rejected_in_core' }
    [pscustomobject]@{ Id = 14; Section = 'core';  Pkg = 'idm-core'; Target = @('--test', 'phase4a');   Name = 'migration_v4_creates_tables_and_is_idempotent' }

    [pscustomobject]@{ Id = 11; Section = 'cli';   Pkg = 'idm-cli';  Target = @('--test', 'phase4a');   Name = 'watch_runner_picks_up_new_tasks' }
    [pscustomobject]@{ Id = 12; Section = 'cli';   Pkg = 'idm-cli';  Target = @('--test', 'phase4a');   Name = 'watch_runner_single_instance' }
    [pscustomobject]@{ Id = 4;  Section = 'cli';   Pkg = 'idm-cli';  Target = @('--test', 'phase4a');   Name = 'config_set_get_download_dir' }
)

$selected = $tests | Where-Object { $Section -eq 'all' -or $_.Section -eq $Section }
if (-not $selected -or @($selected).Count -eq 0) {
    Write-Host "FAIL: no tests for section '$Section'"
    exit 1
}

Write-Host "=============================================="
Write-Host "Phase 4a acceptance | section=$Section | repeat=$Repeat | tests=$(@($selected).Count)"
Write-Host "=============================================="

# Each cargo invocation is wrapped in a temporary .bat that redirects output to
# a file and writes a PASS marker ONLY if cargo exits 0 AND the output contains
# "1 passed". A PowerShell pipeline (`2>&1 | Out-String`) is deliberately
# avoided: it blocks until every writer of the pipe closes, and a detached
# child that inherits the handle can keep it open forever. `Process.ExitCode`
# from Start-Process -PassThru is unreliable on PS 5.1; the marker file is not.
$work = Join-Path $env:TEMP ("phase4a-run-{0}" -f $PID)
New-Item -ItemType Directory -Path $work -Force | Out-Null
$outFile = Join-Path $work 'out.txt'
$markFile = Join-Path $work 'PASS'
$batFile = Join-Path $work 'run.bat'

$failures = @()
$idx = 0
foreach ($t in $selected) {
    $idx++
    Write-Host "[$idx/$(@($selected).Count)] START item $($t.Id) $($t.Name)"
    $passed = 0
    for ($i = 1; $i -le $Repeat; $i++) {
        Remove-Item $outFile, $markFile -Force -ErrorAction SilentlyContinue
        $featArg = ''
        if ($t.Feature) { $featArg = '--features ' + $t.Feature + ' ' }
        $cargoLine = 'cargo test -p ' + $t.Pkg + ' --offline ' + $featArg +
                     (@($t.Target) -join ' ') + ' ' + $t.Name +
                     ' -- --exact --test-threads=1'
        # cmd: run cargo, on success grep for "1 passed", only then write marker.
        $bat = @(
            '@echo off'
            "$cargoLine > `"$outFile`" 2>&1"
            'if errorlevel 1 exit /b 1'
            "findstr /C:`"1 passed`" `"$outFile`" >nul"
            'if errorlevel 1 exit /b 1'
            "echo ok > `"$markFile`""
        ) -join "`r`n"
        Set-Content -Path $batFile -Value $bat -Encoding ASCII
        $proc = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', "`"$batFile`"" `
            -PassThru -NoNewWindow
        # Hard cap per run (ms): guard against a wedged child.
        if (-not $proc.WaitForExit(120000)) {
            $proc.Kill()
            Write-Host "  [$($t.Name)] run $i TIMEOUT"
            continue
        }
        $out = Get-Content $outFile -Raw -ErrorAction SilentlyContinue
        if ((Test-Path $markFile) -and $out -match '1 passed') {
            $passed++
            Write-Host "  run $i/$Repeat ok"
        }
        else {
            Write-Host "  [$($t.Name)] run $i FAILED"
            Write-Host ($out.Trim())
        }
    }
    $tag = if ($passed -eq $Repeat) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] item {1,2}  {2}/{3}  {4}" -f $tag, $t.Id, $passed, $Repeat, $t.Name)
    if ($passed -ne $Repeat) {
        $failures += "$($t.Name): $passed/$Repeat"
    }
}
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "=============================================="
if ($failures.Count -gt 0) {
    Write-Host "SUMMARY: FAIL"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "SUMMARY: all $(@($selected).Count) tests reached $Repeat/$Repeat : PASS"
exit 0
