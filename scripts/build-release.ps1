# build-release.ps1
# Phase 5c-1: build release binaries, frontend, NSIS installer, then print artifact path/size.
#
# - ASCII-only output/comments for PowerShell 5.1/7.x compatibility.
# - Any failing step exits non-zero immediately.
# - Does NOT execute the installer, does NOT touch registry or user data.
# - v0.3.0a: default build is UNSIGNED (createUpdaterArtifacts overridden to false).
#   Use -Sign only when the user has supplied the private key path and password.

param(
    [switch] $Sign
)

$ErrorActionPreference = 'Stop'

function Clear-SignEnvironment {
    # Password is only ever held in process environment for the duration of the build.
    Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY -ErrorAction SilentlyContinue
    Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
}

function Invoke-Step {
    param(
        [Parameter(Mandatory = $true)] [string] $Name,
        [Parameter(Mandatory = $true)] [scriptblock] $ScriptBlock
    )
    Write-Output "[STEP] $Name"
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $ScriptBlock
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    if ($null -eq $code) { $code = 0 }
    if ($code -ne 0) {
        Write-Output "[FAIL] $Name (exit $code)"
        Clear-SignEnvironment
        exit $code
    }
    Write-Output "[PASS] $Name"
}

# Resolve repo root from this script's location: <root>/scripts/build-release.ps1
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $RepoRoot

$Triple = 'x86_64-pc-windows-msvc'
$ReleaseDir = Join-Path $RepoRoot 'target\release'
$TauriDir = Join-Path $RepoRoot 'apps\idm-desktop\src-tauri'
$BinariesDir = Join-Path $TauriDir 'binaries'
$SetupDir = Join-Path $ReleaseDir 'bundle\nsis'

# Signed Firefox extension bundled into the installer.
# Single source of truth for the expected hash: update here when shipping a new extension.
# docs/maintenance.md documents the same value and the update procedure.
$ExtensionSha256 = 'D918224C3DF5800289A8311245D555A4328053C2B8AD980CDD229729C60F3227'
$ExtensionSource = Join-Path $RepoRoot 'dist\idm-rs-firefox-0.1.0-signed.xpi'
$ExtensionStageDir = Join-Path $TauriDir 'resources'
$ExtensionStaged = Join-Path $ExtensionStageDir 'idm-rs-firefox.xpi'

# 1) release binaries: idm-cli and idm-nmhost (idm-ipc crate)
Invoke-Step 'cargo build --release idm-cli + idm-ipc(nmhost)' {
    cargo build --release -p idm-cli -p idm-ipc --bins
}

# 2) frontend
Invoke-Step 'npm run build (frontend)' {
    Push-Location (Join-Path $RepoRoot 'apps\idm-desktop')
    try { npm run build } finally { Pop-Location }
}

# 3) stage externalBin files with target-triple suffix expected by Tauri
Invoke-Step 'stage external binaries (binaries/*-<triple>.exe)' {
    New-Item -ItemType Directory -Force -Path $BinariesDir | Out-Null
    $cliSrc = Join-Path $ReleaseDir 'idm-cli.exe'
    $nmSrc = Join-Path $ReleaseDir 'idm-nmhost.exe'
    if (-not (Test-Path $cliSrc)) { throw "missing $cliSrc" }
    if (-not (Test-Path $nmSrc)) { throw "missing $nmSrc" }
    Copy-Item -Force $cliSrc (Join-Path $BinariesDir "idm-cli-$Triple.exe")
    Copy-Item -Force $nmSrc (Join-Path $BinariesDir "idm-nmhost-$Triple.exe")
}

# 4) verify and stage signed Firefox extension as a bundled resource
Invoke-Step 'verify + stage signed Firefox extension' {
    if (-not (Test-Path $ExtensionSource)) { throw "missing signed extension: $ExtensionSource" }
    $actual = (Get-FileHash -Algorithm SHA256 $ExtensionSource).Hash
    if ($actual -ne $ExtensionSha256) {
        throw "extension SHA256 mismatch: expected $ExtensionSha256, got $actual"
    }
    New-Item -ItemType Directory -Force -Path $ExtensionStageDir | Out-Null
    Copy-Item -Force $ExtensionSource $ExtensionStaged
    $stagedHash = (Get-FileHash -Algorithm SHA256 $ExtensionStaged).Hash
    if ($stagedHash -ne $ExtensionSha256) { throw "staged extension SHA256 mismatch: $stagedHash" }
    Write-Output "EXTENSION: $ExtensionStaged ($((Get-Item $ExtensionStaged).Length) bytes)"
    Write-Output "SHA256: $stagedHash"
}

# 5) signing setup (signed mode only; my verification builds never use -Sign)
# https://v2.tauri.app/plugin/updater/#signing-updates :
# tauri build reads TAURI_SIGNING_PRIVATE_KEY (path or content) and
# TAURI_SIGNING_PRIVATE_KEY_PASSWORD. The user-facing path variable is mapped
# to the build variable only for this process.
if ($Sign) {
    $keyPath = $env:TAURI_SIGNING_PRIVATE_KEY_PATH
    if ([string]::IsNullOrWhiteSpace($keyPath) -or -not (Test-Path $keyPath)) {
        Write-Output '[FAIL] TAURI_SIGNING_PRIVATE_KEY_PATH is not set or the file does not exist'
        Write-Output '  Example: $env:TAURI_SIGNING_PRIVATE_KEY_PATH = "C:\path\to\updater.key"'
        exit 1
    }
    $securePassword = Read-Host -AsSecureString -Prompt 'Updater signing password'
    $plainPassword = [System.Net.NetworkCredential]::new('', $securePassword).Password
    $env:TAURI_SIGNING_PRIVATE_KEY = $keyPath
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $plainPassword
    Remove-Variable plainPassword -ErrorAction SilentlyContinue
    Write-Output 'SIGNING: enabled (password in process environment only)'
} else {
    Clear-SignEnvironment
    Write-Output 'SIGNING: disabled (createUpdaterArtifacts=false override)'
}

# https://v2.tauri.app/reference/cli/ : --config accepts inline JSON merged
# over tauri.conf.json, with later values overriding earlier ones.
$UnsignedConfig = '{\"bundle\":{\"createUpdaterArtifacts\":false}}'

# 6) desktop release build + NSIS installer
Invoke-Step 'tauri build (nsis)' {
    Push-Location (Join-Path $RepoRoot 'apps\idm-desktop')
    try {
        if ($Sign) { npx tauri build --bundles nsis }
        else { npx tauri build --bundles nsis --config $UnsignedConfig }
    } finally { Pop-Location }
}

# 7) locate installer and report
$setup = Get-ChildItem -Path $SetupDir -Filter '*-setup.exe' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($null -eq $setup) {
    Write-Output '[FAIL] installer not found'
    Clear-SignEnvironment
    exit 1
}
$sizeMiB = [math]::Round($setup.Length / 1MB, 2)
Write-Output '=============================================='
Write-Output "INSTALLER: $($setup.FullName)"
Write-Output "SIZE: $($setup.Length) bytes ($sizeMiB MiB)"
Write-Output '=============================================='

if ($Sign) {
    Invoke-Step 'make-latest-json.ps1' {
        $version = (Get-Content (Join-Path $TauriDir 'tauri.conf.json') -Raw | ConvertFrom-Json).version
        & (Join-Path $PSScriptRoot 'make-latest-json.ps1') -Version $version
    }
}

Clear-SignEnvironment
exit 0
