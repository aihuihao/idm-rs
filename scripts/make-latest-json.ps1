# make-latest-json.ps1
# v0.3.0a: build the static update manifest consumed by the Tauri updater.
#
# Official field format (Tauri v2 updater, "Static JSON File"):
#   https://v2.tauri.app/plugin/updater/
# Required: version, platforms.<OS-ARCH>.url, platforms.<OS-ARCH>.signature.
# Optional: notes, pub_date (RFC 3339).
#
# - ASCII-only output/comments for PowerShell 5.1/7.x compatibility.
# - Reads a .sig file produced by `tauri build`; never reads a private key.
# - Does not touch the registry or user data directories.

param(
    [Parameter(Mandatory = $true)] [string] $Version,
    [string] $SigFile,
    [string] $NotesFile,
    [string] $Output,
    [string] $PubDate
)

$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    Write-Output "[FAIL] Version must be plain SemVer (e.g. 0.3.0): $Version"
    exit 1
}
if ([string]::IsNullOrWhiteSpace($SigFile)) {
    $SigFile = Join-Path $RepoRoot "target\release\bundle\nsis\idm-rs_${Version}_x64-setup.exe.sig"
}
if ([string]::IsNullOrWhiteSpace($NotesFile)) {
    $NotesFile = Join-Path $RepoRoot "docs\release-notes-${Version}.md"
}
if ([string]::IsNullOrWhiteSpace($Output)) {
    $Output = Join-Path $RepoRoot 'dist\latest.json'
}
if ([string]::IsNullOrWhiteSpace($PubDate)) {
    $PubDate = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
}

if (-not (Test-Path $SigFile)) {
    Write-Output "[FAIL] signature file not found: $SigFile"
    exit 1
}
if (-not (Test-Path $NotesFile)) {
    Write-Output "[FAIL] release notes not found: $NotesFile"
    exit 1
}
if ($PubDate -notmatch '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$') {
    Write-Output "[FAIL] PubDate must be RFC 3339 UTC (yyyy-MM-ddTHH:mm:ssZ): $PubDate"
    exit 1
}

$signature = (Get-Content $SigFile -Raw).Trim()
if ($signature.Length -eq 0) {
    Write-Output '[FAIL] signature file is empty'
    exit 1
}

$allNotes = @(Get-Content $NotesFile)
$take = [Math]::Min(20, $allNotes.Count)
$notesLines = @($allNotes | Select-Object -First $take)
while ($notesLines.Count -gt 0 -and [string]::IsNullOrWhiteSpace($notesLines[$notesLines.Count - 1])) {
    $notesLines = @($notesLines | Select-Object -First ($notesLines.Count - 1))
}
$notes = ($notesLines -join "`n").Trim()

$downloadUrl = "https://github.com/aihuihao/idm-rs/releases/download/v${Version}/idm-rs_${Version}_x64-setup.exe"

$manifest = [ordered]@{
    version  = $Version
    notes    = $notes
    pub_date = $PubDate
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = $signature
            url       = $downloadUrl
        }
    }
}

$json = $manifest | ConvertTo-Json -Depth 6
$outDir = Split-Path -Parent $Output
if ($outDir -and -not (Test-Path $outDir)) {
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
}
[System.IO.File]::WriteAllText($Output, $json + "`n")

Write-Output "LATEST_JSON: $Output"
Write-Output "VERSION: $Version"
Write-Output "URL: $downloadUrl"
exit 0
