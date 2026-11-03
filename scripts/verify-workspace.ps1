# verify-workspace.ps1
# Phase 0 workspace acceptance script for idm-rs.
#
# - Output and comments are ASCII-only to avoid PowerShell 5.1 Chinese mojibake.
# - Compatible with PowerShell 5.1 and 7.x.
# - Every step prints "[STEP n] Name" and "PASS"/"FAIL".
# - Any failing step calls "exit 1" immediately and stops subsequent steps.

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Invoke-Step: wraps an external program (rustc / cargo / git).
#
# Contract (per spec):
#   1. Save current $ErrorActionPreference, temporarily set to 'Continue'.
#   2. If $PSNativeCommandUseErrorActionPreference exists (PS 7.3+), set $false
#      during execution so native failures are NOT turned into exceptions.
#   3. Immediately capture $LASTEXITCODE after the program ends.
#   4. Use try/finally to restore all modified settings.
#   5. A non-zero exit code is the ONLY failure criterion. stderr output is NOT
#      a failure (cargo prints normal progress to stderr).
#   6. Captures merged stdout + stderr text and returns it for later checks.
# ---------------------------------------------------------------------------
function Invoke-Step {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [scriptblock]$ScriptBlock
    )

    $prevErrorPref = $ErrorActionPreference

    $hasNativeVar = $false
    $prevNativePref = $null
    if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
        $hasNativeVar = $true
        $prevNativePref = $PSNativeCommandUseErrorActionPreference
    }

    $ErrorActionPreference = 'Continue'
    if ($hasNativeVar) { $PSNativeCommandUseErrorActionPreference = $false }

    try {
        $captured = & $ScriptBlock 2>&1
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $prevErrorPref
        if ($hasNativeVar) { $PSNativeCommandUseErrorActionPreference = $prevNativePref }
    }

    $text = $captured | Out-String
    return [PSCustomObject]@{
        Name     = $Name
        ExitCode = $exitCode
        Output   = $text
    }
}

# Step runner for a pure external-command step (pass/fail by exit code only).
function Step-Command {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [scriptblock]$ScriptBlock
    )

    $global:stepNumber++
    Write-Host "[STEP $global:stepNumber] $Name"
    $result = Invoke-Step -Name $Name -ScriptBlock $ScriptBlock
    if ($result.ExitCode -ne 0) {
        Write-Host "FAIL (exit=$($result.ExitCode))"
        exit 1
    }
    Write-Host "PASS"
    return $result
}

# Step runner for an inspection step whose scriptblock returns $true/$false.
function Step-Check {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [scriptblock]$ScriptBlock
    )

    $global:stepNumber++
    Write-Host "[STEP $global:stepNumber] $Name"
    $ok = & $ScriptBlock
    if (-not $ok) {
        Write-Host "FAIL"
        exit 1
    }
    Write-Host "PASS"
}

$stepNumber = 0

# ---------------------------------------------------------------------------
# STEP 1: rustc / cargo present + MSVC host
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] Verify rustc/cargo exist and host is x86_64-pc-windows-msvc"
$rustc = Invoke-Step -Name "rustc -vV" -ScriptBlock { rustc -vV }
if ($rustc.ExitCode -ne 0) {
    Write-Host "FAIL (rustc not found)"
    exit 1
}
if ($rustc.Output -notmatch 'host:\s*x86_64-pc-windows-msvc') {
    Write-Host "FAIL (host is not x86_64-pc-windows-msvc)"
    Write-Host $rustc.Output
    exit 1
}
$cargo = Invoke-Step -Name "cargo -V" -ScriptBlock { cargo -V }
if ($cargo.ExitCode -ne 0) {
    Write-Host "FAIL (cargo not found)"
    exit 1
}
Write-Host $rustc.Output.Trim()
Write-Host $cargo.Output.Trim()
Write-Host "PASS"

# ---------------------------------------------------------------------------
# STEP 2: Cargo.lock exists
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] Verify Cargo.lock exists"
if (-not (Test-Path Cargo.lock)) {
    Write-Host "FAIL (Cargo.lock not found; run 'cargo generate-lockfile' first)"
    exit 1
}
Write-Host "PASS"

# ---------------------------------------------------------------------------
# STEP 3: cargo fmt --all -- --check
# ---------------------------------------------------------------------------
Step-Command -Name "cargo fmt --all -- --check" -ScriptBlock { cargo fmt --all -- --check }

# ---------------------------------------------------------------------------
# STEP 4: cargo check --workspace --all-targets --locked
# ---------------------------------------------------------------------------
Step-Command -Name "cargo check --workspace --all-targets --locked" -ScriptBlock { cargo check --workspace --all-targets --locked }

# ---------------------------------------------------------------------------
# STEP 5: cargo test --workspace --locked (includes both toolchain smoke tests)
# ---------------------------------------------------------------------------
Step-Command -Name "cargo test --workspace --locked" -ScriptBlock { cargo test --workspace --locked }

# ---------------------------------------------------------------------------
# STEP 6: cargo clippy --workspace --all-targets --locked -- -D warnings
# ---------------------------------------------------------------------------
Step-Command -Name "cargo clippy --workspace --all-targets --locked -- -D warnings" -ScriptBlock { cargo clippy --workspace --all-targets --locked -- -D warnings }

# ---------------------------------------------------------------------------
# STEP 7: cargo run -p idm-cli --locked; verify exit 0 and output has core_version + version
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] Run idm-cli and verify core_version field + version in output"
$run = Invoke-Step -Name "cargo run -p idm-cli --locked" -ScriptBlock { cargo run -p idm-cli --locked }
if ($run.ExitCode -ne 0) {
    Write-Host "FAIL (exit=$($run.ExitCode))"
    Write-Host $run.Output
    exit 1
}
if ($run.Output -notmatch 'core_version[=:]\S+') {
    Write-Host "FAIL (core_version field with version not found in output)"
    Write-Host $run.Output
    exit 1
}
Write-Host $run.Output.Trim()
Write-Host "PASS"

# ---------------------------------------------------------------------------
# STEP 8: forbidden transitive deps via cargo tree
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] Check cargo tree for forbidden dependencies"
$tree = Invoke-Step -Name "cargo tree" -ScriptBlock { cargo tree --workspace --target all -e normal,build,dev --prefix none }
if ($tree.ExitCode -ne 0) {
    Write-Host "FAIL (cargo tree failed)"
    Write-Host $tree.Output
    exit 1
}
$forbiddenPattern = '^(tauri(-[\w-]+)?|wry|tao|async-std|smol|sqlx(-[\w-]+)?|diesel(-[\w-]+)?) v'
$lines = $tree.Output -split "`n"
$bad = $lines | Where-Object { $_ -match $forbiddenPattern }
if ($bad) {
    Write-Host "FAIL (forbidden dependency detected):"
    $bad | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "PASS"

# ---------------------------------------------------------------------------
# STEP 9: static check of crate Cargo.toml for forbidden direct dependencies
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: no forbidden direct deps in crate Cargo.toml" -ScriptBlock {
    $files = @(
        "crates/idm-core/Cargo.toml",
        "crates/idm-cli/Cargo.toml",
        "crates/idm-ipc/Cargo.toml"
    )
    $forbidden = @('hyper', 'clap', 'actix-web', 'axum', 'sea-orm')
    $ok = $true
    foreach ($f in $files) {
        if (-not (Test-Path $f)) {
            Write-Host "  missing: $f"
            $ok = $false
            continue
        }
        $content = Get-Content -Encoding utf8 $f -Raw
        if ($content -match 'tauri') {
            Write-Host "  $f contains forbidden token: tauri"
            $ok = $false
        }
        foreach ($dep in $forbidden) {
            if ($content -match "(?m)^\s*$dep\s*=") {
                Write-Host "  $f directly declares forbidden dep: $dep"
                $ok = $false
            }
        }
    }
    return $ok
}

# ---------------------------------------------------------------------------
# STEP 10: forbidden paths must not exist
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: no apps/ src-tauri/ package.json" -ScriptBlock {
    $bad = @()
    if (Test-Path apps) { $bad += "apps/" }
    if (Test-Path src-tauri) { $bad += "src-tauri/" }
    if (Test-Path package.json) { $bad += "package.json" }
    if ($bad.Count -gt 0) {
        Write-Host "  forbidden paths found: $($bad -join ', ')"
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# STEP 11: no TODO/FIXME/todo!()/unimplemented!() in crates/ .rs files
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: no TODO/FIXME/todo!()/unimplemented!() in .rs" -ScriptBlock {
    $files = Get-ChildItem -Path crates -Recurse -Filter *.rs
    $ok = $true
    foreach ($f in $files) {
        $lines = Get-Content -Encoding utf8 $f.FullName
        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i] -match 'TODO|FIXME|todo!\(|unimplemented!\(') {
                Write-Host "  $($f.FullName):$($i + 1): $($lines[$i].Trim())"
                $ok = $false
            }
        }
    }
    return $ok
}

# ---------------------------------------------------------------------------
# STEP 12: unsafe keyword may only appear in crates/idm-core/src/disk/win32.rs,
#          and there each `unsafe` block must be preceded by a SAFETY: comment.
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: unsafe only in win32.rs with SAFETY notes" -ScriptBlock {
    $allowed = "crates/idm-core/src/disk/win32.rs".ToLower()
    $files = Get-ChildItem -Path crates -Recurse -Filter *.rs
    $ok = $true
    foreach ($f in $files) {
        # Normalize the absolute path to forward slashes + lowercase so the
        # forward-slash allow pattern matches on Windows (where FullName uses `\`).
        $norm = $f.FullName.Replace('\', '/').ToLower()
        $lines = Get-Content -Encoding utf8 $f.FullName
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $trimmed = $lines[$i].Trim()
            # Skip comment lines: a real `unsafe` usage (block, fn, impl, extern,
            # trait) is never prefixed with `//`. Doc comments may *mention*
            # `unsafe` (e.g. "this module allows `unsafe`"), which must not count.
            if ($trimmed -like '//*') { continue }
            # Case-sensitive: match every `unsafe` usage, not just `unsafe {`.
            if ($trimmed -cmatch '\bunsafe\b') {
                if ($norm -notmatch [regex]::Escape($allowed)) {
                    Write-Host "  $($f.FullName):$($i + 1): unsafe outside win32.rs: $trimmed"
                    $ok = $false
                }
                else {
                    # Walk backwards over blank lines and comment lines (a SAFETY note
                    # may span several `//` lines; only the first carries `SAFETY:`).
                    # Stop at the first non-comment, non-blank line.
                    $j = $i - 1
                    $found = $false
                    while ($j -ge 0) {
                        $t = $lines[$j].Trim()
                        if ($t -eq '') { $j--; continue }
                        if ($t -like '//*') {
                            if ($t -match 'SAFETY:') { $found = $true }
                            $j--
                            continue
                        }
                        break
                    }
                    if (-not $found) {
                        Write-Host "  $($f.FullName):$($i + 1): unsafe block missing preceding SAFETY: comment"
                        $ok = $false
                    }
                }
            }
        }
    }
    return $ok
}

# ---------------------------------------------------------------------------
# STEP 13: #[allow( / #![allow( only allowed as the top-level
#          `#![allow(unsafe_code)]` in win32.rs.
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: allow only in win32.rs as #![allow(unsafe_code)]" -ScriptBlock {
    $allowed = "crates/idm-core/src/disk/win32.rs".ToLower()
    $files = Get-ChildItem -Path crates -Recurse -Filter *.rs
    $ok = $true
    foreach ($f in $files) {
        # Normalize the absolute path to forward slashes + lowercase so the
        # forward-slash allow pattern matches on Windows (where FullName uses `\`).
        $norm = $f.FullName.Replace('\', '/').ToLower()
        $lines = Get-Content -Encoding utf8 $f.FullName
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $trimmed = $lines[$i].Trim()
            # Skip comment lines: a real `#[allow]`/`#![allow]` attribute is never
            # prefixed with `//`, while doc comments may *mention* the attribute.
            if ($trimmed -like '//*') { continue }
            if ($trimmed -match '#!?\[allow\(') {
                $isAllowed = ($norm -match [regex]::Escape($allowed)) -and ($trimmed -eq '#![allow(unsafe_code)]')
                if (-not $isAllowed) {
                    Write-Host "  $($f.FullName):$($i + 1): forbidden allow: $trimmed"
                    $ok = $false
                }
            }
        }
    }
    return $ok
}

# ---------------------------------------------------------------------------
# STEP 14: duplicate dependencies (informational only, never fails)
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] Show duplicate dependencies (informational)"
$dups = Invoke-Step -Name "cargo tree -d" -ScriptBlock { cargo tree --workspace -d }
Write-Host $dups.Output.Trim()

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host "=============================================="
Write-Host "SUMMARY: all $stepNumber steps executed; final result: PASS"
Write-Host "=============================================="
exit 0
