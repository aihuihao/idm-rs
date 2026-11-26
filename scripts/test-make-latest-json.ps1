# test-make-latest-json.ps1
# v0.3.0a: offline test for scripts/make-latest-json.ps1.
# Uses a fake .sig file and a fake release-notes file; no private key, no network.
#
# - ASCII-only comments/output where possible for PowerShell 5.1/7.x.
# - Creates and removes only its own temporary directory.

$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Script = Join-Path $RepoRoot 'scripts/make-latest-json.ps1'

function Assert-True {
    param([string] $Name, [bool] $Ok)
    if (-not $Ok) {
        Write-Output "[FAIL] $Name"
        exit 1
    }
    Write-Output "[PASS] $Name"
}

if (-not (Test-Path $Script)) {
    Write-Output "[FAIL] missing script: $Script"
    exit 1
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("idm-latest-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    $sigContent = 'dummy-signature-content-for-offline-test'
    $sigFile = Join-Path $tmp 'fake-setup.exe.sig'
    [System.IO.File]::WriteAllText($sigFile, "$sigContent`r`n")

    $notesLines = @('# Release Notes 9.8.7')
    for ($i = 1; $i -le 24; $i++) { $notesLines += "line-$i" }
    $notesFile = Join-Path $tmp 'release-notes-9.8.7.md'
    [System.IO.File]::WriteAllLines($notesFile, $notesLines)

    $output = Join-Path $tmp 'latest.json'
    $pubDate = '2026-11-25T00:00:00Z'
    & $Script -Version '9.8.7' -SigFile $sigFile -NotesFile $notesFile -Output $output -PubDate $pubDate
    Assert-True 'script exit code' ($LASTEXITCODE -eq 0)
    Assert-True 'latest.json created' (Test-Path $output)

    $json = Get-Content $output -Raw | ConvertFrom-Json
    Assert-True 'version field' ($json.version -eq '9.8.7')
    Assert-True 'pub_date field' ($json.pub_date -eq $pubDate)
    Assert-True 'download url' ($json.platforms.'windows-x86_64'.url -eq 'https://github.com/aihuihao/idm-rs/releases/download/v9.8.7/idm-rs_9.8.7_x64-setup.exe')
    Assert-True 'signature field' ($json.platforms.'windows-x86_64'.signature -eq $sigContent)
    Assert-True 'notes include heading' ($json.notes -match 'Release Notes 9.8.7')
    Assert-True 'notes truncated to first 20 lines' (-not ($json.notes -match 'line-20'))
    Assert-True 'json has no extra platforms' (@($json.platforms.PSObject.Properties.Name).Count -eq 1)

    Write-Output '[PASS] make-latest-json tests'
    exit 0
} finally {
    if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
}
