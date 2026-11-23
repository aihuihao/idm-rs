# package-extension.ps1
# Phase 5c-3a: package the Firefox extension (runtime files only, no README)
# into dist/idm-rs-firefox-0.1.0.zip with files at the ZIP root.
#
# - ASCII-only output/comments for PowerShell 5.1/7.x compatibility.
# - No network, no npm, no new dependencies (uses built-in Compress-Archive).
# - Self-checks after packaging: manifest at ZIP root, version matches file
#   name, and lists every entry. Exits non-zero on any failure.

$ErrorActionPreference = 'Stop'

# Resolve repo root from this script's location: <root>/scripts/package-extension.ps1
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$ExtDir = Join-Path $RepoRoot 'extensions\firefox'
$DistDir = Join-Path $RepoRoot 'dist'
$Version = '0.1.0'
$ZipName = "idm-rs-firefox-$Version.zip"
$ZipPath = Join-Path $DistDir $ZipName

# Runtime files shipped in the extension (README is intentionally excluded).
$RuntimeFiles = @('manifest.json', 'background.js')

function Fail([string]$Message) {
    Write-Output "[FAIL] $Message"
    exit 1
}

if (-not (Test-Path (Join-Path $ExtDir 'manifest.json'))) {
    Fail "manifest.json missing in $ExtDir"
}
foreach ($f in $RuntimeFiles) {
    if (-not (Test-Path (Join-Path $ExtDir $f))) {
        Fail "required runtime file missing: $f"
    }
}

# Stage runtime files in an isolated temp dir so the ZIP has a flat root.
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ("idm-rs-ext-{0}" -f [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Stage -Force | Out-Null
try {
    foreach ($f in $RuntimeFiles) {
        Copy-Item -Path (Join-Path $ExtDir $f) -Destination (Join-Path $Stage $f) -Force
    }
    New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
    if (Test-Path $ZipPath) { Remove-Item -Path $ZipPath -Force }
    Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $ZipPath -CompressionLevel Optimal
} finally {
    Remove-Item -Path $Stage -Recurse -Force -ErrorAction SilentlyContinue
}

if (-not (Test-Path $ZipPath)) { Fail "ZIP was not created: $ZipPath" }

# --- Self-checks -------------------------------------------------------------
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [System.IO.Compression.ZipFile]::OpenRead($ZipPath)
try {
    $entries = @($zip.Entries)
    $names = @($entries | ForEach-Object { $_.FullName })

    # 1) manifest.json must be at the ZIP root (no extra folder level).
    if ($names -notcontains 'manifest.json') {
        Fail "manifest.json not found at ZIP root; entries: $($names -join ', ')"
    }
    foreach ($n in $names) {
        if ($n -match '/') { Fail "entry is not at ZIP root: $n" }
    }

    # 2) manifest version must match the file name (0.1.0).
    $mEntry = $zip.GetEntry('manifest.json')
    $reader = New-Object System.IO.StreamReader($mEntry.Open())
    try { $manifestJson = $reader.ReadToEnd() } finally { $reader.Dispose() }
    $manifest = $manifestJson | ConvertFrom-Json
    if ($manifest.version -ne $Version) {
        Fail "manifest version '$($manifest.version)' does not match file name version '$Version'"
    }

    # 3) list every entry.
    Write-Output '=============================================='
    Write-Output "ZIP: $ZipPath"
    $size = (Get-Item $ZipPath).Length
    Write-Output "SIZE: $size bytes ($([math]::Round($size/1KB,2)) KiB)"
    Write-Output 'ENTRIES:'
    foreach ($e in ($entries | Sort-Object FullName)) {
        Write-Output ("  {0}  ({1} bytes)" -f $e.FullName, $e.Length)
    }
    Write-Output '=============================================='
    Write-Output "SELF-CHECK: PASS (version=$Version, root manifest OK)"
} finally {
    $zip.Dispose()
}
exit 0
