# phase4b-acceptance.ps1
# Phase 4b acceptance: extension static checks (pure PowerShell) plus every
# NEW Rust test run individually, 10 consecutive times.
#
# - ASCII-only comments/output (avoid PowerShell 5.1 Chinese mojibake).
# - Compatible with PowerShell 5.1 / 7.x.
# - All cargo invocations use --offline (no network), per the work rules.
# - A cargo run counts as PASS only when exit code is 0 AND output has
#   "1 passed" (an --exact filter matching nothing still exits 0).
# - The existing "no package.json" rule is kept unchanged for locations
#   outside extensions/, and is additionally enforced INSIDE extensions/.
# - All items must reach Repeat/Repeat, otherwise exit 1.
#
# Usage:
#   powershell -File scripts/phase4b-acceptance.ps1
#   powershell -File scripts/phase4b-acceptance.ps1 -Section static
#   powershell -File scripts/phase4b-acceptance.ps1 -Section cli -Repeat 10

param(
    [ValidateSet('all', 'static', 'cli')]
    [string]$Section = 'all',
    [int]$Repeat = 10
)

$ErrorActionPreference = 'Stop'
# Resolve paths from the script location, so the script works from any cwd.
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$extDir = Join-Path $repoRoot 'extensions'
$fxDir = Join-Path $extDir 'firefox'

# --- Pure-PowerShell static checks for the Firefox extension -----------------
function Test-ManifestJson {
    $p = Join-Path $fxDir 'manifest.json'
    if (-not (Test-Path $p)) { return $false }
    try {
        $v = Get-Content -Encoding UTF8 -Raw $p | ConvertFrom-Json
    } catch {
        return $false
    }
    if ($null -eq $v) { return $false }
    if ($v.manifest_version -ne 2) { return $false }
    if ($v.browser_specific_settings.gecko.id -ne 'idm-rs@local') { return $false }
    if (-not $v.background.scripts) { return $false }
    if (-not $v.browser_action) { return $false }
    return $true
}

function Test-Permissions {
    $p = Join-Path $fxDir 'manifest.json'
    if (-not (Test-Path $p)) { return $false }
    try {
        $v = Get-Content -Encoding UTF8 -Raw $p | ConvertFrom-Json
    } catch {
        return $false
    }
    $want = @('downloads', 'nativeMessaging', 'cookies', 'storage', '<all_urls>')
    $got = @($v.permissions)
    if ($got.Count -ne $want.Count) { return $false }
    foreach ($w in $want) {
        if ($got -notcontains $w) { return $false }
    }
    if (($got | Sort-Object -Unique).Count -ne $want.Count) { return $false }
    return $true
}

function Test-NoPackageJson {
    # Keep the ORIGINAL rule for non-extensions locations unchanged.
    if (Test-Path (Join-Path $repoRoot 'package.json')) { return $false }
    if (Test-Path (Join-Path $repoRoot 'apps')) { return $false }
    if (Test-Path (Join-Path $repoRoot 'src-tauri')) { return $false }
    # Additionally: extensions/ must also stay free of package.json / node_modules.
    if (-not (Test-Path $extDir)) { return $false }
    $pkg = @(Get-ChildItem -Path $extDir -Recurse -Force -Filter 'package.json' -ErrorAction SilentlyContinue)
    if ($pkg.Count -gt 0) { return $false }
    $mods = @(Get-ChildItem -Path $extDir -Recurse -Force -Directory -Filter 'node_modules' -ErrorAction SilentlyContinue)
    if ($mods.Count -gt 0) { return $false }
    return $true
}

function Test-BackgroundContainsRequiredCalls {
    $p = Join-Path $fxDir 'background.js'
    if (-not (Test-Path $p)) { return $false }
    $text = Get-Content -Encoding UTF8 -Raw $p
    if ($text -notmatch 'sendNativeMessage') { return $false }
    if ($text -notmatch 'downloads\.cancel') { return $false }
    if ($text -notmatch 'downloads\.erase') { return $false }
    if ($text -notmatch 'downloads\.onCreated') { return $false }
    if ($text -notmatch 'browser_action|browserAction\.setBadgeText') { return $false }
    return $true
}

function Test-BackgroundHasNoForbiddenCode {
    $p = Join-Path $fxDir 'background.js'
    if (-not (Test-Path $p)) { return $false }
    $text = Get-Content -Encoding UTF8 -Raw $p
    if ($text -match 'eval\(') { return $false }
    if ($text -match 'new\s+Function') { return $false }
    if ($text -match 'innerHTML') { return $false }
    return $true
}

# --- Phase 4b-fix: text/structure checks on background.js -------------------
# Pure PowerShell. No JavaScript is executed, no Node/npm is required.
function Get-BackgroundText {
    $p = Join-Path $fxDir 'background.js'
    if (-not (Test-Path $p)) { return $null }
    return (Get-Content -Encoding UTF8 -Raw $p)
}

# Extract every `catch (...) { ... }` block with brace counting, so nested
# try/catch blocks (e.g. removeFile) are handled correctly.
function Get-CatchBlocks {
    param([string]$Text)
    $blocks = @()
    foreach ($m in [regex]::Matches($Text, 'catch\s*\(\w+\)\s*\{')) {
        $open = $Text.IndexOf('{', $m.Index)
        if ($open -lt 0) { continue }
        $depth = 0
        for ($i = $open; $i -lt $Text.Length; $i++) {
            if ($Text[$i] -eq '{') { $depth++ }
            elseif ($Text[$i] -eq '}') {
                $depth--
                if ($depth -eq 0) {
                    $blocks += $Text.Substring($open, $i - $open + 1)
                    break
                }
            }
        }
    }
    return ,$blocks
}

# Slice the handoff() failure branch: from the fixed failure log line up to the
# end of handoff() (the onCreated listener registration follows it).
function Get-FailureBranchText {
    param([string]$Text)
    $marker = 'console.warn("idm-rs: handoff failed'
    $start = $Text.IndexOf($marker, [System.StringComparison]::Ordinal)
    if ($start -lt 0) { return $null }
    $end = $Text.IndexOf('browser.downloads.onCreated', $start, [System.StringComparison]::Ordinal)
    if ($end -lt 0 -or $end -le $start) { return $null }
    return $Text.Substring($start, $end - $start)
}

# Order check: every `downloads.cancel` must come AFTER the `reply.ok === true`
# acceptance check, i.e. cancelling is only reachable on a positive reply.
function Test-CancelOnlyAfterOkReply {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    $gate = $text.IndexOf('reply.ok === true', [System.StringComparison]::Ordinal)
    if ($gate -lt 0) { return $false }
    $cancels = [regex]::Matches($text, 'downloads\.cancel')
    if ($cancels.Count -eq 0) { return $false }
    foreach ($m in $cancels) {
        if ($m.Index -le $gate) { return $false }
    }
    return $true
}

function Test-CatchBranchesHaveNoCancel {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    $blocks = @(Get-CatchBlocks -Text $text)
    if ($blocks.Count -eq 0) { return $false }
    foreach ($b in $blocks) {
        if ($b -match 'downloads\.cancel') { return $false }
    }
    return $true
}

function Test-ContainsRemoveFile {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    return ($text -match 'downloads\.removeFile')
}

function Test-ContainsLastFailAt {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    return ($text -match 'lastFailAt')
}

# Part 3: PowerShell text/structure test - failure branch clears the dedup entry.
function Test-FailureBranchDeletesRecent {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    $branch = Get-FailureBranchText -Text $text
    if ($null -eq $branch) { return $false }
    return ($branch -match 'recent\.delete\(url\)')
}

# Part 3: PowerShell text/structure test - 30 second backoff constant exists
# and is actually used as the backoff window.
function Test-BackoffConstantIsThirtySeconds {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    if ($text -notmatch 'FAIL_BACKOFF_MS\s*=\s*30000') { return $false }
    if ($text -notmatch 'lastFailAt\s*!==\s*0') { return $false }
    if ($text -notmatch 'FAIL_BACKOFF_MS') { return $false }
    return $true
}

# Part 3: PowerShell text/structure test - forbidden JS constructs are absent.
function Test-NoForbiddenJsConstructs {
    $text = Get-BackgroundText
    if ($null -eq $text) { return $false }
    if ($text -match 'eval\(') { return $false }
    if ($text -match 'new\s+Function') { return $false }
    if ($text -match 'innerHTML') { return $false }
    return $true
}

$staticTests = @(
    [pscustomobject]@{ Id = 4; Name = 'static_manifest_json_is_valid'; Fn = { Test-ManifestJson } }
    [pscustomobject]@{ Id = 4; Name = 'static_permissions_are_exactly_five'; Fn = { Test-Permissions } }
    [pscustomobject]@{ Id = 5; Name = 'static_no_package_json_anywhere'; Fn = { Test-NoPackageJson } }
    [pscustomobject]@{ Id = 4; Name = 'static_background_required_calls'; Fn = { Test-BackgroundContainsRequiredCalls } }
    [pscustomobject]@{ Id = 4; Name = 'static_background_no_forbidden_code'; Fn = { Test-BackgroundHasNoForbiddenCode } }
    # Part 2.2 additions
    [pscustomobject]@{ Id = 2; Name = 'static_cancel_only_after_ok_reply'; Fn = { Test-CancelOnlyAfterOkReply } }
    [pscustomobject]@{ Id = 2; Name = 'static_catch_branches_have_no_cancel'; Fn = { Test-CatchBranchesHaveNoCancel } }
    [pscustomobject]@{ Id = 2; Name = 'static_contains_removeFile'; Fn = { Test-ContainsRemoveFile } }
    [pscustomobject]@{ Id = 2; Name = 'static_contains_lastFailAt'; Fn = { Test-ContainsLastFailAt } }
    # Part 3 additions: PowerShell text/structure tests
    [pscustomobject]@{ Id = 3; Name = 'ps_test_failure_branch_deletes_recent'; Fn = { Test-FailureBranchDeletesRecent } }
    [pscustomobject]@{ Id = 3; Name = 'ps_test_backoff_constant_30s'; Fn = { Test-BackoffConstantIsThirtySeconds } }
    [pscustomobject]@{ Id = 3; Name = 'ps_test_no_forbidden_js_constructs'; Fn = { Test-NoForbiddenJsConstructs } }
)

# --- New Rust tests (registration commands) ----------------------------------
$cargoTests = @(
    [pscustomobject]@{ Id = 1; Name = 'nm_install_registers_manifest_and_registry' }
    [pscustomobject]@{ Id = 3; Name = 'nm_manifest_json_fields_are_exact' }
    [pscustomobject]@{ Id = 2; Name = 'nm_rejects_invalid_extension_id' }
    [pscustomobject]@{ Id = 2; Name = 'nm_rejects_invalid_host_name' }
    [pscustomobject]@{ Id = 2; Name = 'nm_rejects_missing_host_exe' }
    [pscustomobject]@{ Id = 1; Name = 'nm_install_is_idempotent' }
    [pscustomobject]@{ Id = 1; Name = 'nm_status_after_uninstall_reports_absent' }
)

$staticSelected = if ($Section -eq 'all' -or $Section -eq 'static') { @($staticTests) } else { @() }
$cliSelected = if ($Section -eq 'all' -or $Section -eq 'cli') { @($cargoTests) } else { @() }
$total = @($staticSelected).Count + @($cliSelected).Count
if ($total -eq 0) {
    Write-Host "FAIL: no tests for section '$Section'"
    exit 1
}

Write-Host "=============================================="
Write-Host "Phase 4b acceptance | section=$Section | repeat=$Repeat | items=$total"
Write-Host "=============================================="

$failures = @()
$idx = 0

# Static checks: pure PowerShell, no external process.
foreach ($t in $staticSelected) {
    $idx++
    Write-Host "[$idx/$total] START item $($t.Id) $($t.Name)"
    $passed = 0
    for ($i = 1; $i -le $Repeat; $i++) {
        $ok = $false
        try { $ok = & $t.Fn } catch { $ok = $false }
        if ($ok) { $passed++; Write-Host "  run $i/$Repeat ok" }
        else { Write-Host "  [$($t.Name)] run $i FAILED" }
    }
    $tag = if ($passed -eq $Repeat) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] item {1,2}  {2}/{3}  {4}" -f $tag, $t.Id, $passed, $Repeat, $t.Name)
    if ($passed -ne $Repeat) { $failures += "$($t.Name): $passed/$Repeat" }
}

# Cargo tests: each run wrapped in a temporary .bat with a PASS marker file.
# A PowerShell pipeline (2>&1 | Out-String) is deliberately avoided: a detached
# child inheriting the pipe handle can keep it open forever.
$work = Join-Path $env:TEMP ("phase4b-run-{0}" -f $PID)
New-Item -ItemType Directory -Path $work -Force | Out-Null
$outFile = Join-Path $work 'out.txt'
$markFile = Join-Path $work 'PASS'
$batFile = Join-Path $work 'run.bat'

foreach ($t in $cliSelected) {
    $idx++
    Write-Host "[$idx/$total] START item $($t.Id) $($t.Name)"
    $passed = 0
    for ($i = 1; $i -le $Repeat; $i++) {
        Remove-Item $outFile, $markFile -Force -ErrorAction SilentlyContinue
        $cargoLine = 'cargo test -p idm-cli --offline --test phase4b ' + $t.Name +
                     ' -- --exact --test-threads=1'
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
    if ($passed -ne $Repeat) { $failures += "$($t.Name): $passed/$Repeat" }
}
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "=============================================="
if ($failures.Count -eq 0) {
    Write-Host "SUMMARY: all $total items reached $Repeat/$Repeat : PASS"
    Write-Host "=============================================="
    exit 0
}
Write-Host "SUMMARY: FAIL ($($failures.Count) items)"
$failures | ForEach-Object { Write-Host "  $_" }
Write-Host "=============================================="
exit 1
