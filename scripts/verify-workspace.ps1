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
# STEP 4/5: desktop frontend build + lockfile guard.
#           Runs before the workspace cargo steps because
#           tauri::generate_context!() reads apps/idm-desktop/dist at compile
#           time; a fresh checkout has no dist, so cargo check/test/clippy
#           --workspace would fail without it. (Per task Phase 5a-2 step 4.6:
#           moved ahead of the former STEP 4.)
#
#           STEP 4 avoids bulk-delete of an existing node_modules (which trips
#           the safe-delete guard at >500 files): when node_modules is absent
#           run `npm ci`; when present run `npm install` (no bulk delete).
#           Either way package-lock.json must remain unchanged (git diff guard).
# ---------------------------------------------------------------------------
$env:Path = "C:\Program Files\nodejs;$env:Path"
$desktopDir = Join-Path (Get-Location) 'apps/idm-desktop'
Push-Location $desktopDir
try {
    $nodeModulesDir = Join-Path $desktopDir 'node_modules'
    if (Test-Path $nodeModulesDir) {
        Step-Command -Name "npm install --prefer-offline --no-audit --no-fund" -ScriptBlock { npm install --prefer-offline --no-audit --no-fund }
    }
    else {
        Step-Command -Name "npm ci --prefer-offline" -ScriptBlock { npm ci --prefer-offline }
    }
    Step-Command -Name "npm run build" -ScriptBlock { npm run build }
}
finally {
    Pop-Location
}
# git pathspec is relative to the repo root, so run this after leaving $desktopDir.
Step-Command -Name "git diff --exit-code apps/idm-desktop/package-lock.json" -ScriptBlock { git diff --exit-code apps/idm-desktop/package-lock.json }

# ---------------------------------------------------------------------------
# STEP 5: cargo check --workspace --all-targets --locked
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
#         Phase 5a-2: check only idm-core / idm-cli / idm-ipc, each with
#         `cargo tree -p <crate>`. idm-desktop is intentionally not checked
#         because it is allowed to depend on tauri/wry/tao.
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] Check cargo tree of core crates for tauri/wry/tao"
$bad = @()
foreach ($crate in @('idm-core', 'idm-cli', 'idm-ipc')) {
    $tree = Invoke-Step -Name "cargo tree -p $crate" -ScriptBlock { cargo tree -p $crate --target all -e normal,build,dev --prefix none }
    if ($tree.ExitCode -ne 0) {
        Write-Host "FAIL (cargo tree -p $crate failed)"
        Write-Host $tree.Output
        exit 1
    }
    $lines = $tree.Output -split "`n"
    $hits = $lines | Where-Object { $_ -match '^(tauri(-[\w-]+)?|wry|tao) v' }
    foreach ($h in $hits) { $bad += "$crate : $h" }
}
if ($bad.Count -gt 0) {
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
# STEP 10: package.json / node_modules placement
#          Phase 5a-2: the only allowed package.json in the whole repository
#          is apps/idm-desktop/package.json. Any other package.json (including
#          under extensions/) fails, and any git-tracked node_modules fails.
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: single package.json + no tracked node_modules" -ScriptBlock {
    $bad = @()
    $allowed = 'apps/idm-desktop/package.json'
    $trackedFiles = @(git ls-files)
    $trackedPkg = @($trackedFiles | Where-Object { $_ -match '(^|/)package\.json$' })
    foreach ($p in $trackedPkg) {
        if ($p -ne $allowed) { $bad += "unexpected tracked package.json: $p" }
    }
    $trackedNodeModules = @($trackedFiles | Where-Object { $_ -match '(^|/)node_modules(/|$)' })
    foreach ($t in $trackedNodeModules) { $bad += "tracked node_modules: $t" }
    if ($bad.Count -gt 0) {
        $bad | ForEach-Object { Write-Host "  $_" }
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
# STEP 12: unsafe keyword may only appear in crates/idm-core/src/disk/win32.rs.
#          (The "every `unsafe` block needs a SAFETY note" rule is now enforced
#           by clippy's `undocumented_unsafe_blocks`, see STEP 6.)
# ---------------------------------------------------------------------------
Step-Check -Name "Static check: unsafe only in win32.rs" -ScriptBlock {
    $allowed = "crates/idm-core/src/disk/win32.rs".ToLower()
    $files = @()
    $files += Get-ChildItem -Path crates -Recurse -Filter *.rs
    $desktopSrc = "apps/idm-desktop/src-tauri/src"
    if (Test-Path $desktopSrc) { $files += Get-ChildItem -Path $desktopSrc -Recurse -Filter *.rs }
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
    $files = @()
    $files += Get-ChildItem -Path crates -Recurse -Filter *.rs
    $desktopSrc = "apps/idm-desktop/src-tauri/src"
    if (Test-Path $desktopSrc) { $files += Get-ChildItem -Path $desktopSrc -Recurse -Filter *.rs }
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
# STEP 15: rustdoc comment/link check (-D warnings)
# ---------------------------------------------------------------------------
$stepNumber++
Write-Host "[STEP $stepNumber] cargo doc --workspace --no-deps --locked (RUSTDOCFLAGS=-D warnings)"
$env:RUSTDOCFLAGS = "-D warnings"
$doc = Invoke-Step -Name "cargo doc" -ScriptBlock { cargo doc --workspace --no-deps --locked }
if ($doc.ExitCode -ne 0) {
    Write-Host "FAIL (exit=$($doc.ExitCode))"
    Write-Host $doc.Output
    exit 1
}
Write-Host "PASS"

# ---------------------------------------------------------------------------
# STEP 16: crash_resume test runs on its own (--ignored --test-threads=1).
#          It spawns 20 killed CLI subprocesses and is timing sensitive, so it
#          must not run concurrently with the rest of the suite (STEP 5).
# ---------------------------------------------------------------------------
Step-Command -Name "cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1" -ScriptBlock {
    cargo test -p idm-cli --test crash_resume --locked -- --ignored --test-threads=1
}

# ---------------------------------------------------------------------------
# STEP 17: cargo build -p idm-desktop --locked (needs dist from STEP 4,
#          which was moved ahead of the workspace cargo steps).
# ---------------------------------------------------------------------------
Step-Command -Name "cargo build -p idm-desktop --locked" -ScriptBlock { cargo build -p idm-desktop --locked }

# ---------------------------------------------------------------------------
# STEP 18 (Phase 5a-4): frontend must not contain dangerous patterns
#   (v-html / innerHTML / eval( / new Function) in .vue and .ts files.
# ---------------------------------------------------------------------------
Step-Check -Name "static: frontend has no dangerous patterns" -ScriptBlock {
    $root = Join-Path (Get-Location) 'apps/idm-desktop/src'
    $files = @(Get-ChildItem -Path $root -Recurse -File | Where-Object { $_.Extension -in '.vue', '.ts' })
    $patterns = @('v-html', 'innerHTML', 'eval\(', 'new Function')
    foreach ($file in $files) {
        foreach ($pattern in $patterns) {
            $hit = Select-String -Path $file.FullName -Pattern $pattern -SimpleMatch:$false -Quiet
            if ($hit) {
                Write-Host "  forbidden pattern '$pattern' in $($file.FullName)"
                return $false
            }
        }
    }
    return $true
}

# ---------------------------------------------------------------------------
# STEP 18b (v0.3.0a): frontend must surface the `Probing` task state.
#   The scheduler now writes Probing before probe; the UI must show a Chinese
#   label and include it in pause/downloading/incomplete filters and counts.
# ---------------------------------------------------------------------------
Step-Check -Name "static: frontend handles Probing state" -ScriptBlock {
    $app = Join-Path (Get-Location) 'apps/idm-desktop/src/App.vue'
    # App.vue is UTF-8 without BOM; PS 5.1 Get-Content -Raw would decode it as ANSI
    # and mangle Chinese labels, so read it explicitly as UTF-8.
    $text = Get-Content $app -Encoding UTF8 -Raw
    $checks = @(
        'function stateText(task: TaskView)',
        '{{ stateText(task) }}',
        'function statusMatchKey(t: TaskView, key: string)',
        "t.state === 'Probing' || t.state === 'Downloading'"
    )
    foreach ($c in $checks) {
        if ($text.IndexOf($c) -lt 0) {
            Write-Host "  missing Probing handling: $c"
            return $false
        }
    }
    # Check the real behavior: stateText maps Probing to the exact Chinese label
    # "连接中" (code points 8FDE 63A5 4E2D), and that function feeds the table cell.
    $expectedLabel = -join @([char]0x8FDE, [char]0x63A5, [char]0x4E2D)
    $mapping = [regex]::Match($text, "case 'Probing':\s*return '([^']*)'")
    if (-not $mapping.Success) {
        Write-Host '  missing Chinese label for Probing'
        return $false
    }
    if ($mapping.Groups[1].Value -ne $expectedLabel) {
        $codes = ($mapping.Groups[1].Value.ToCharArray() | ForEach-Object { ([int]$_).ToString('X4') }) -join ' '
        Write-Host "  Probing label mismatch (code points: $codes)"
        return $false
    }
    $probingMentions = ([regex]::Matches($text, "t\.state === 'Probing'")).Count
    if ($probingMentions -lt 4) {
        Write-Host "  expected at least 4 Probing state checks in App.vue, got $probingMentions"
        return $false
    }
    # The pause button must actually act on Probing tasks, not only be enabled.
    $pauseFilters = ([regex]::Matches($text, [regex]::Escape("t.state === 'Queued' || t.state === 'Probing' || t.state === 'Downloading'"))).Count
    if ($pauseFilters -lt 2) {
        Write-Host "  expected Probing in both canPauseSelection and onPauseSelected, got $pauseFilters"
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# STEP 19 (Phase 5a-4): tauri.conf.json CSP starts with "default-src 'self'".
# ---------------------------------------------------------------------------
Step-Check -Name "static: tauri CSP starts with default-src 'self'" -ScriptBlock {
    $confPath = Join-Path (Get-Location) 'apps/idm-desktop/src-tauri/tauri.conf.json'
    $conf = Get-Content -Raw $confPath | ConvertFrom-Json
    $csp = $conf.app.security.csp
    if ($null -eq $csp) {
        Write-Host "  csp is missing"
        return $false
    }
    if (-not $csp.StartsWith("default-src 'self'")) {
        Write-Host "  csp does not start with default-src 'self': $csp"
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# STEP 20 (Phase 5a-4): every capabilities json grants only core:default.
# ---------------------------------------------------------------------------
Step-Check -Name "static: capabilities grant only core:default" -ScriptBlock {
    $capDir = Join-Path (Get-Location) 'apps/idm-desktop/src-tauri/capabilities'
    $files = @(Get-ChildItem -Path $capDir -File -Filter '*.json')
    foreach ($file in $files) {
        $json = Get-Content -Raw $file.FullName | ConvertFrom-Json
        $perms = @($json.permissions)
        foreach ($perm in $perms) {
            if ($perm -ne 'core:default') {
                Write-Host "  unexpected permission '$perm' in $($file.Name)"
                return $false
            }
        }
    }
    return $true
}

# ---------------------------------------------------------------------------
# STEP 21 (phase5a-4-fix): invoke() argument objects must use camelCase keys.
#   Tauri 2 converts Rust snake_case command params to camelCase, so a JS key
#   with an underscore is silently rejected. Scan only the { ... } argument
#   object on lines that call invoke(); command names and nested payload fields
#   (e.g. Settings.download_dir) are not command params and are left alone.
# ---------------------------------------------------------------------------
Step-Check -Name "static: invoke args use camelCase (no snake_case keys)" -ScriptBlock {
    $root = Join-Path (Get-Location) 'apps/idm-desktop/src'
    $files = @(Get-ChildItem -Path $root -Recurse -File | Where-Object { $_.Extension -in '.vue', '.ts' })
    $bad = @()
    foreach ($file in $files) {
        $lineNo = 0
        foreach ($line in (Get-Content $file.FullName)) {
            $lineNo++
            if ($line -match 'invoke') {
                $braceIdx = $line.IndexOf('{')
                if ($braceIdx -ge 0) {
                    $argPart = $line.Substring($braceIdx)
                    if ($argPart -match '\b[a-zA-Z_][a-zA-Z0-9_]*_[a-zA-Z0-9_]*\s*:') {
                        $bad += "$($file.Name):$lineNo key='$($Matches[0])'"
                    }
                }
            }
        }
    }
    if ($bad.Count -gt 0) {
        $bad | ForEach-Object { Write-Host "  snake_case invoke arg $_" }
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# STEP 21b (v0.3.0a): the updater plugin must be called from Rust only, and
#   the config/settings/UI pieces required by the auto-update flow must exist.
# ---------------------------------------------------------------------------
Step-Check -Name "static: updater plugin only in Rust" -ScriptBlock {
    $root = Join-Path (Get-Location) 'apps/idm-desktop/src'
    $files = @(Get-ChildItem -Path $root -Recurse -File | Where-Object { $_.Extension -in '.vue', '.ts' })
    foreach ($file in $files) {
        $text = Get-Content $file.FullName -Raw
        if ($text -match '@tauri-apps/plugin-updater|plugin-updater|updaterBuilder') {
            Write-Host "  frontend must not call the updater plugin: $($file.Name)"
            return $false
        }
    }
    $commands = Get-Content (Join-Path (Get-Location) 'apps/idm-desktop/src-tauri/src/commands.rs') -Raw
    if ($commands -notmatch 'tauri_plugin_updater') {
        Write-Host '  Rust commands do not use tauri_plugin_updater'
        return $false
    }
    return $true
}

Step-Check -Name "static: auto-update config, settings and UI" -ScriptBlock {
    $confPath = Join-Path (Get-Location) 'apps/idm-desktop/src-tauri/tauri.conf.json'
    $conf = Get-Content $confPath -Raw | ConvertFrom-Json
    if (-not $conf.bundle.createUpdaterArtifacts) {
        Write-Host '  bundle.createUpdaterArtifacts must be true'
        return $false
    }
    if ([string]::IsNullOrWhiteSpace($conf.plugins.updater.pubkey)) {
        Write-Host '  plugins.updater.pubkey is missing'
        return $false
    }
    if ($conf.plugins.updater.endpoints[0] -ne 'https://github.com/aihuihao/idm-rs/releases/latest/download/latest.json') {
        Write-Host '  updater endpoint mismatch'
        return $false
    }
    if ($conf.plugins.updater.windows.installMode -ne 'passive') {
        Write-Host '  updater installMode must be passive'
        return $false
    }
    $service = Get-Content (Join-Path (Get-Location) 'apps/idm-desktop/src-tauri/src/service.rs') -Raw
    $commands = Get-Content (Join-Path (Get-Location) 'apps/idm-desktop/src-tauri/src/commands.rs') -Raw
    $app = Get-Content (Join-Path (Get-Location) 'apps/idm-desktop/src/App.vue') -Encoding UTF8 -Raw
    $checks = @(
        @($service, '"auto-check-updates"', 'Rust settings key'),
        @($commands, 'run_install_flow', 'install orchestration'),
        @($app, 'auto_check_updates', 'settings checkbox'),
        @($app, "invoke<UpdateCheckView>('check_for_update')", 'manual check command'),
        @($app, "invoke('install_update')", 'install command'),
        @($app, "'update-progress'", 'progress event listener')
    )
    foreach ($c in $checks) {
        if ($c[0].IndexOf($c[1]) -lt 0) {
            Write-Host "  missing auto-update piece: $($c[2])"
            return $false
        }
    }
    # Chinese UI text via Unicode regex so this script stays ASCII-only.
    $regexChecks = @(
        @($app, '\u53d1\u73b0\u65b0\u7248\u672c', 'update banner'),
        @($app, '\u66f4\u65b0\u524d\u4f1a\u6682\u505c\u6240\u6709\u6b63\u5728\u8fdb\u884c\u7684\u4e0b\u8f7d', 'install confirmation text')
    )
    foreach ($c in $regexChecks) {
        if ($c[0] -notmatch $c[1]) {
            Write-Host "  missing auto-update piece: $($c[2])"
            return $false
        }
    }
    return $true
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host "=============================================="
Write-Host "SUMMARY: all $stepNumber steps executed; final result: PASS"
Write-Host "=============================================="
exit 0
