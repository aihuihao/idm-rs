# network-acceptance.ps1
# Phase 1b.1 real-network acceptance automation (items A-F of phase1b1.md part two).
# ASCII-only, compatible with PowerShell 5.1.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File .\scripts\network-acceptance.ps1 `
#       -Url "<ISO_URL>" -Sha256 "<OFFICIAL_SHA256>" -MaxSpeed 3M
#
# Parameters:
#   -Url        (required) ISO URL to download.
#   -Sha256     (required) Official SHA256 of the ISO (case-insensitive compare).
#   -MaxSpeed   (required, no unlimited downloads allowed) Total speed cap, e.g.
#               3M or 500K (K=1024, M=1024*1024), or a plain byte count.
#               A/B/D use it directly; E gives HALF to each of its two processes.
#   -Dir        (default D:\dl-test) Working directory. Must be empty or absent.
#   -Exe        (default .\target\release\idm-cli.exe) Path to the release CLI.
#   -KeepFiles  (switch) If set, downloaded files are kept; otherwise deleted at end.

param(
    [Parameter(Mandatory = $true)]
    [string]$Url,

    [Parameter(Mandatory = $true)]
    [string]$Sha256,

    [string]$MaxSpeed,

    [string]$Dir = 'D:\dl-test',

    [string]$Exe = '.\target\release\idm-cli.exe',

    [switch]$KeepFiles
)

$ErrorActionPreference = 'Continue'

# ASCII-only source: build the Chinese phrase "rate limit not effective" from
# Unicode code points, so PowerShell 5.1 (which reads BOM-less UTF-8 as ANSI)
# cannot corrupt the report text.
$limitNotEffective = ([int[]](0x9650, 0x901F, 0x672A, 0x751F, 0x6548) | ForEach-Object { [char]$_ }) -join ''
$speedViolations = @()

function Convert-SpeedToBytes {
    param([string]$Value)
    $v = $Value.Trim()
    $m = [regex]::Match($v, '^([0-9]+)([KkMm]?)$')
    if (-not $m.Success) { return $null }
    $numText = $m.Groups[1].Value
    $unit = $m.Groups[2].Value.ToUpperInvariant()
    try { $num = [long]$numText } catch { return $null }
    if ($num -le 0) { return $null }
    $mult = switch ($unit) {
        'K' { 1024 }
        'M' { 1048576 }
        default { 1 }
    }
    try {
        $bytes = [long]($num * $mult)
        if ($bytes -le 0) { return $null }
        return $bytes
    } catch { return $null }
}

function Test-SpeedCap {
    param([double]$Measured, [double]$Cap)
    if ($Cap -le 0) { return $true }
    return ($Measured -le ($Cap * 1.2))
}

# ---------------------------------------------------------------------------
# Pre-checks
# ---------------------------------------------------------------------------
# -MaxSpeed is deliberately NOT declared [Parameter(Mandatory)]: in a detached,
# non-interactive run PowerShell would prompt on stdin and hang. Explicit check
# refuses to run instead, which is the required behaviour.
if ([string]::IsNullOrWhiteSpace($MaxSpeed)) {
    Write-Host 'ERROR: -MaxSpeed is REQUIRED (e.g. 3M, 500K). Refusing to run without a speed cap.'
    exit 2
}
$limitBytes = Convert-SpeedToBytes $MaxSpeed
if ($null -eq $limitBytes) {
    Write-Host ("ERROR: invalid -MaxSpeed '{0}' (expected e.g. 3M, 500K, or a plain byte count)" -f $MaxSpeed)
    exit 2
}
$halfBytes = [long][math]::Floor($limitBytes / 2)
if ($halfBytes -lt 1) {
    Write-Host 'ERROR: -MaxSpeed is too small to split in half for item E.'
    exit 2
}
$halfSpeedText = $halfBytes.ToString()
Write-Host ("MaxSpeed: {0} ({1} B/s); E per-process: {2} B/s" -f $MaxSpeed, $limitBytes, $halfSpeedText)

if (-not (Test-Path $Exe)) {
    Write-Host "ERROR: executable not found: $Exe"
    Write-Host "Run first: cargo build --release -p idm-cli"
    exit 1
}
$ExePath = (Resolve-Path $Exe).Path

if (Test-Path $Dir) {
    $existing = @(Get-ChildItem -Path $Dir -Force | Where-Object { -not $_.PSIsContainer })
    if ($existing.Count -gt 0) {
        Write-Host "ERROR: directory already exists and contains files: $Dir"
        Write-Host "Contents:"
        foreach ($f in $existing) {
            Write-Host ("  {0} ({1} bytes)" -f $f.Name, $f.Length)
        }
        Write-Host "Clean the directory first, then re-run."
        exit 1
    }
}
else {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
}

$logs = Join-Path $Dir 'logs'
New-Item -ItemType Directory -Force -Path $logs | Out-Null

$docDir = (Resolve-Path (Join-Path $PSScriptRoot '..\docs')).Path
$docPath = Join-Path $docDir 'phase1b-network-acceptance.md'

$shaNorm = $Sha256.Trim().ToLower()
try { $uri = [uri]$Url; $mirror = $uri.Host } catch { $mirror = '(unknown)' }

# Quoted copies for Start-Process -ArgumentList (values may contain spaces).
$qUrl = '"' + $Url + '"'
$qDir = '"' + $Dir + '"'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
function Get-DataFiles {
    # Non-container files directly under $Dir, excluding the logs directory.
    $map = @{}
    foreach ($f in @(Get-ChildItem -Path $Dir -Force | Where-Object { (-not $_.PSIsContainer) -and ($_.Name -ne 'logs') })) {
        $map[$f.Name] = $f.Length
    }
    return $map
}

function Invoke-Cli {
    param(
        [Parameter(Mandatory = $true)] [string[]]$Arguments,
        [Parameter(Mandatory = $true)] [string]$LogFile
    )
    $errLog = $LogFile + '.err'
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $ExePath -ArgumentList $Arguments -Wait -PassThru `
        -RedirectStandardOutput $LogFile -RedirectStandardError $errLog
    $sw.Stop()
    $out = ''
    if (Test-Path $LogFile) { $out = (Get-Content -Path $LogFile -Raw -ErrorAction SilentlyContinue) }
    return [PSCustomObject]@{
        ExitCode  = $p.ExitCode
        ElapsedMs = $sw.Elapsed.TotalMilliseconds
        Output    = $out
    }
}

function Get-Metric {
    param([string]$Text, [string]$Key)
    if ([string]::IsNullOrEmpty($Text)) { return $null }
    $pattern = "RESULT.*?${Key}([^\s]+)"
    if ($Text -match $pattern) { return $Matches[1] }
    return $null
}

function Get-ProbeInfo {
    param([string]$Text)
    $info = @{ total = $null; range = $null }
    if ([string]::IsNullOrEmpty($Text)) { return $info }
    if ($Text -match 'total_size=Some\((\d+)\)') { $info.total = [long]$Matches[1] }
    if ($Text -match 'supports_range=(\w+)') { $info.range = $Matches[1] }
    return $info
}

function Format-Speed {
    param([double]$BytesPerSec)
    if ($BytesPerSec -le 0) { return 'n/a' }
    return ('{0:F2} MB/s' -f ($BytesPerSec / 1MB))
}

function Get-Sha256File {
    param([string]$FilePath)
    if (-not (Test-Path $FilePath)) { return $null }
    return (Get-FileHash -Path $FilePath -Algorithm SHA256).Hash.ToLower()
}

# Result collector for the summary table.
$results = @()

# ---------------------------------------------------------------------------
# Item A: single connection download
# ---------------------------------------------------------------------------
Write-Host "`n=== A: single connection (--connections 1) ==="
$beforeA = Get-DataFiles
$cmdA = "idm-cli.exe download `"$Url`" --dir $Dir --connections 1 --max-speed $MaxSpeed"
$resA = Invoke-Cli -Arguments @('download', $qUrl, '--dir', $qDir, '--connections', '1', '--max-speed', $MaxSpeed) `
    -LogFile (Join-Path $logs 'A_conn1.log')
$afterA = Get-DataFiles
$newA = @($afterA.Keys | Where-Object { -not $beforeA.ContainsKey($_) })
$aFile = if ($newA.Count -ge 1) { $newA[0] } else { $null }
$aSize = if ($aFile) { $afterA[$aFile] } else { 0 }
$aSpeedRaw = Get-Metric $resA.Output 'avg_speed='
$aElapsedRaw = Get-Metric $resA.Output 'elapsed='
$aElapsedSec = if ($aElapsedRaw) { [double]($aElapsedRaw -replace 's', '') } else { $resA.ElapsedMs / 1000.0 }
$aSpeed = if ($aSpeedRaw) { [double]$aSpeedRaw } else { if ($aElapsedSec -gt 0) { $aSize / $aElapsedSec } else { 0 } }
$hA = if ($aFile) { Get-Sha256File (Join-Path $Dir $aFile) } else { $null }
$speedOkA = Test-SpeedCap $aSpeed $limitBytes
if (-not $speedOkA) {
    $speedViolations += ('A: measured={0:N0} B/s, cap={1} B/s, 120%={2:N0} B/s' -f $aSpeed, $limitBytes, ($limitBytes * 1.2))
}
$noteA = if ($speedOkA) { ('cap={0} B/s, within_cap=yes' -f $limitBytes) } else { ('cap={0} B/s, within_cap=no, ' -f $limitBytes) + $limitNotEffective }
$passA = ($resA.ExitCode -eq 0) -and ($null -ne $aFile) -and ($aSize -gt 0) -and $speedOkA
Write-Host ("  command : {0}" -f $cmdA)
Write-Host ("  exit    : {0}" -f $resA.ExitCode)
Write-Host ("  file    : {0} ({1} bytes)" -f $aFile, $aSize)
Write-Host ("  elapsed : {0:F2}s" -f $aElapsedSec)
Write-Host ("  speed   : {0} (cap {1} B/s, within 120%: {2})" -f (Format-Speed $aSpeed), $limitBytes, $speedOkA)
Write-Host ("  result  : {0}" -f $(if ($passA) { 'PASS' } else { 'FAIL' }))
$results += [PSCustomObject]@{ Item = 'A'; Desc = 'single connection download'; Pass = $passA; Elapsed = ('{0:F2}s' -f $aElapsedSec); Speed = (Format-Speed $aSpeed); WithinCap = $(if ($speedOkA) { 'yes' } else { 'no' }); Sha = $hA; Detail = ("file={0}, size={1}, elapsed={2:F2}s, speed={3}, {4}" -f $aFile, $aSize, $aElapsedSec, (Format-Speed $aSpeed), $noteA) }

# ---------------------------------------------------------------------------
# Item B: 8 connections download to same directory (expect name (1).ext)
# ---------------------------------------------------------------------------
Write-Host "`n=== B: 8 connections (--connections 8) ==="
$beforeB = Get-DataFiles
$cmdB = "idm-cli.exe download `"$Url`" --dir $Dir --connections 8 --max-speed $MaxSpeed"
$resB = Invoke-Cli -Arguments @('download', $qUrl, '--dir', $qDir, '--connections', '8', '--max-speed', $MaxSpeed) `
    -LogFile (Join-Path $logs 'B_conn8.log')
$afterB = Get-DataFiles
$newB = @($afterB.Keys | Where-Object { -not $beforeB.ContainsKey($_) })
$bFile = if ($newB.Count -ge 1) { $newB[0] } else { $null }
$bSize = if ($bFile) { $afterB[$bFile] } else { 0 }
$bSpeedRaw = Get-Metric $resB.Output 'avg_speed='
$bElapsedRaw = Get-Metric $resB.Output 'elapsed='
$bElapsedSec = if ($bElapsedRaw) { [double]($bElapsedRaw -replace 's', '') } else { $resB.ElapsedMs / 1000.0 }
$bSpeed = if ($bSpeedRaw) { [double]$bSpeedRaw } else { if ($bElapsedSec -gt 0) { $bSize / $bElapsedSec } else { 0 } }
$hB = if ($bFile) { Get-Sha256File (Join-Path $Dir $bFile) } else { $null }
$differentName = ($null -ne $bFile) -and ($bFile -ne $aFile)
$speedOkB = Test-SpeedCap $bSpeed $limitBytes
if (-not $speedOkB) {
    $speedViolations += ('B: measured={0:N0} B/s, cap={1} B/s, 120%={2:N0} B/s' -f $bSpeed, $limitBytes, ($limitBytes * 1.2))
}
$noteB = if ($speedOkB) { ('cap={0} B/s, within_cap=yes' -f $limitBytes) } else { ('cap={0} B/s, within_cap=no, ' -f $limitBytes) + $limitNotEffective }
$passB = ($resB.ExitCode -eq 0) -and ($null -ne $bFile) -and ($bSize -gt 0) -and $differentName -and $speedOkB
Write-Host ("  command : {0}" -f $cmdB)
Write-Host ("  exit    : {0}" -f $resB.ExitCode)
Write-Host ("  file    : {0} ({1} bytes)" -f $bFile, $bSize)
Write-Host ("  elapsed : {0:F2}s" -f $bElapsedSec)
Write-Host ("  speed   : {0} (cap {1} B/s, within 120%: {2})" -f (Format-Speed $bSpeed), $limitBytes, $speedOkB)
Write-Host ("  result  : {0}" -f $(if ($passB) { 'PASS' } else { 'FAIL' }))
$results += [PSCustomObject]@{ Item = 'B'; Desc = '8-connection download (name (1).ext)'; Pass = $passB; Elapsed = ('{0:F2}s' -f $bElapsedSec); Speed = (Format-Speed $bSpeed); WithinCap = $(if ($speedOkB) { 'yes' } else { 'no' }); Sha = $hB; Detail = ("file={0}, size={1}, elapsed={2:F2}s, speed={3}, {4}" -f $bFile, $bSize, $bElapsedSec, (Format-Speed $bSpeed), $noteB) }

# ---------------------------------------------------------------------------
# Item C: SHA256 of A and B files match official
# ---------------------------------------------------------------------------
Write-Host "`n=== C: SHA256 verification ==="
$passC = ($hA -eq $shaNorm) -and ($hB -eq $shaNorm)
Write-Host ("  A hash  : {0}" -f $hA)
Write-Host ("  B hash  : {0}" -f $hB)
Write-Host ("  expect  : {0}" -f $shaNorm)
Write-Host ("  result  : {0}" -f $(if ($passC) { 'PASS' } else { 'FAIL' }))
$results += [PSCustomObject]@{ Item = 'C'; Desc = 'SHA256 of A and B match official'; Pass = $passC; Elapsed = '-'; Speed = '-'; WithinCap = '-'; Sha = $shaNorm; Detail = ("a={0}, b={1}, expect={2}" -f $hA, $hB, $shaNorm) }

# ---------------------------------------------------------------------------
# Item D: cancel via --cancel-after 15 (exit != 0, .idm-part kept, rename+delete OK)
# ---------------------------------------------------------------------------
Write-Host "`n=== D: cancel (--cancel-after 15) ==="
$cmdD = "idm-cli.exe download `"$Url`" --dir $Dir --connections 8 --cancel-after 15 --max-speed $MaxSpeed"
$resD = Invoke-Cli -Arguments @('download', $qUrl, '--dir', $qDir, '--connections', '8', '--cancel-after', '15', '--max-speed', $MaxSpeed) `
    -LogFile (Join-Path $logs 'D_cancel.log')
$part = @(Get-ChildItem -Path $Dir -Force | Where-Object { $_.Name -like '*.idm-part' }) | Select-Object -First 1
$exitNonZero = ($resD.ExitCode -ne 0)
$partPresent = ($null -ne $part) -and ($part.Length -gt 0)
$renamed = $false
$deleted = $false
if ($part) {
    $tmpName = ('_cancel_renamed_' + [guid]::NewGuid().ToString('N') + '.tmp')
    $tmpPath = Join-Path $Dir $tmpName
    try {
        Rename-Item -Path $part.FullName -NewName $tmpName -Force
        $renamed = $true
    }
    catch {
        $renamed = $false
        Write-Host ("  rename error: {0}" -f $_.Exception.Message)
    }
    if ($renamed) {
        try {
            Remove-Item -Path $tmpPath -Force
            $deleted = $true
        }
        catch {
            $deleted = $false
            Write-Host ("  delete error: {0}" -f $_.Exception.Message)
        }
    }
}
$dElapsedSec = $resD.ElapsedMs / 1000.0
$dSpeedRaw = Get-Metric $resD.Output 'avg_speed='
$dSpeed = if ($dSpeedRaw) { [double]$dSpeedRaw } else { 0 }
$speedOkD = Test-SpeedCap $dSpeed $limitBytes
if (-not $speedOkD) {
    $speedViolations += ('D: measured={0:N0} B/s, cap={1} B/s, 120%={2:N0} B/s' -f $dSpeed, $limitBytes, ($limitBytes * 1.2))
}
$noteD = if ($speedOkD) { ('cap={0} B/s, within_cap=yes' -f $limitBytes) } else { ('cap={0} B/s, within_cap=no, ' -f $limitBytes) + $limitNotEffective }
$passD = $exitNonZero -and $partPresent -and $renamed -and $deleted -and $speedOkD
Write-Host ("  command : {0}" -f $cmdD)
Write-Host ("  exit    : {0} (non-zero expected: {1})" -f $resD.ExitCode, $exitNonZero)
Write-Host ("  part    : {0} ({1} bytes, present>0: {2})" -f $(if ($part) { $part.Name } else { 'none' }), $(if ($part) { $part.Length } else { 0 }), $partPresent)
Write-Host ("  rename  : {0}" -f $renamed)
Write-Host ("  delete  : {0}" -f $deleted)
Write-Host ("  speed   : {0} (cap {1} B/s, within 120%: {2})" -f (Format-Speed $dSpeed), $limitBytes, $speedOkD)
Write-Host ("  result  : {0}" -f $(if ($passD) { 'PASS' } else { 'FAIL' }))
$results += [PSCustomObject]@{ Item = 'D'; Desc = 'cancel keeps .idm-part; rename+delete OK'; Pass = $passD; Elapsed = ('{0:F2}s' -f $dElapsedSec); Speed = (Format-Speed $dSpeed); WithinCap = $(if ($speedOkD) { 'yes' } else { 'no' }); Sha = '-'; Detail = ("exit={0}, part={1}, rename={2}, delete={3}, {4}" -f $resD.ExitCode, $(if ($part) { $part.Name } else { 'none' }), $renamed, $deleted, $noteD) }

# ---------------------------------------------------------------------------
# Item E: two concurrent processes, each --connections 4
# ---------------------------------------------------------------------------
Write-Host "`n=== E: two concurrent processes (--connections 4 each) ==="
$beforeE = Get-DataFiles
$cmdE = "idm-cli.exe download `"$Url`" --dir $Dir --connections 4 --max-speed $halfSpeedText   (x2 concurrent; combined cap $MaxSpeed)"
$swE = [System.Diagnostics.Stopwatch]::StartNew()
$p1 = Start-Process -FilePath $ExePath -ArgumentList @('download', $qUrl, '--dir', $qDir, '--connections', '4', '--max-speed', $halfSpeedText) -PassThru `
    -RedirectStandardOutput (Join-Path $logs 'E_1.log') -RedirectStandardError (Join-Path $logs 'E_1.log.err')
$p2 = Start-Process -FilePath $ExePath -ArgumentList @('download', $qUrl, '--dir', $qDir, '--connections', '4', '--max-speed', $halfSpeedText) -PassThru `
    -RedirectStandardOutput (Join-Path $logs 'E_2.log') -RedirectStandardError (Join-Path $logs 'E_2.log.err')
# PS 5.1: Start-Process -PassThru plus redirected output needs EnableRaisingEvents
# before ExitCode becomes available after WaitForExit().
$p1.EnableRaisingEvents = $true
$p2.EnableRaisingEvents = $true
$p1.WaitForExit()
$p2.WaitForExit()
$swE.Stop()
$exit1 = $p1.ExitCode
$exit2 = $p2.ExitCode
$eElapsedSec = $swE.Elapsed.TotalMilliseconds / 1000.0
$afterE = Get-DataFiles
$newE = @($afterE.Keys | Where-Object { -not $beforeE.ContainsKey($_) })
$eFiles = $newE
$eAllZero = $true
$eAllMatch = $true
$eHashes = @()
foreach ($nf in $eFiles) {
    if ($afterE[$nf] -le 0) { $eAllZero = $false }
    $hf = Get-Sha256File (Join-Path $Dir $nf)
    $eHashes += $hf
    if ($hf -ne $shaNorm) { $eAllMatch = $false }
}
# Per-process average speeds come from each RESULT line; the combined average is
# the sum of the two downloaded file sizes divided by the wall-clock time.
$e1Log = Join-Path $logs 'E_1.log'
$e2Log = Join-Path $logs 'E_2.log'
$e1Out = if (Test-Path $e1Log) { Get-Content -Path $e1Log -Raw -ErrorAction SilentlyContinue } else { '' }
$e2Out = if (Test-Path $e2Log) { Get-Content -Path $e2Log -Raw -ErrorAction SilentlyContinue } else { '' }
$e1SpeedRaw = Get-Metric $e1Out 'avg_speed='
$e2SpeedRaw = Get-Metric $e2Out 'avg_speed='
$e1Speed = if ($e1SpeedRaw) { [double]$e1SpeedRaw } else { 0 }
$e2Speed = if ($e2SpeedRaw) { [double]$e2SpeedRaw } else { 0 }
$eTotalBytes = [double]0
foreach ($nf in $eFiles) { $eTotalBytes += $afterE[$nf] }
$eSpeed = if ($eElapsedSec -gt 0) { $eTotalBytes / $eElapsedSec } else { 0 }
$speedOkE1 = Test-SpeedCap $e1Speed $halfBytes
$speedOkE2 = Test-SpeedCap $e2Speed $halfBytes
$speedOkEAll = Test-SpeedCap $eSpeed $limitBytes
$speedOkE = $speedOkE1 -and $speedOkE2 -and $speedOkEAll
if (-not $speedOkE1) {
    $speedViolations += ('E process 1: measured={0:N0} B/s, cap={1} B/s, 120%={2:N0} B/s' -f $e1Speed, $halfBytes, ($halfBytes * 1.2))
}
if (-not $speedOkE2) {
    $speedViolations += ('E process 2: measured={0:N0} B/s, cap={1} B/s, 120%={2:N0} B/s' -f $e2Speed, $halfBytes, ($halfBytes * 1.2))
}
if (-not $speedOkEAll) {
    $speedViolations += ('E combined: measured={0:N0} B/s, cap={1} B/s, 120%={2:N0} B/s' -f $eSpeed, $limitBytes, ($limitBytes * 1.2))
}
$noteE = if ($speedOkE) {
    ('per_process_cap={0} B/s, combined_cap={1} B/s, within_cap=yes' -f $halfBytes, $limitBytes)
} else {
    ('per_process_cap={0} B/s, combined_cap={1} B/s, within_cap=no, ' -f $halfBytes, $limitBytes) + $limitNotEffective
}
$passE = ($exit1 -eq 0) -and ($exit2 -eq 0) -and ($eFiles.Count -eq 2) -and $eAllZero -and $eAllMatch -and $speedOkE
Write-Host ("  command : {0}" -f $cmdE)
Write-Host ("  exit1   : {0}, exit2: {1}" -f $exit1, $exit2)
Write-Host ("  new files: {0}" -f ($eFiles -join ', '))
Write-Host ("  speed1  : {0} (cap {1} B/s, within 120%: {2})" -f (Format-Speed $e1Speed), $halfBytes, $speedOkE1)
Write-Host ("  speed2  : {0} (cap {1} B/s, within 120%: {2})" -f (Format-Speed $e2Speed), $halfBytes, $speedOkE2)
Write-Host ("  combined: {0} (cap {1} B/s, within 120%: {2})" -f (Format-Speed $eSpeed), $limitBytes, $speedOkEAll)
Write-Host ("  result  : {0}" -f $(if ($passE) { 'PASS' } else { 'FAIL' }))
$results += [PSCustomObject]@{ Item = 'E'; Desc = 'two concurrent downloads, two distinct files'; Pass = $passE; Elapsed = ('{0:F2}s' -f $eElapsedSec); Speed = (Format-Speed $eSpeed); WithinCap = $(if ($speedOkE) { 'yes' } else { 'no' }); Sha = ($eHashes -join ', '); Detail = ("exit1={0}, exit2={1}, files={2}, sha256ok={3}, speed1={4}, speed2={5}, combined={6}, {7}" -f $exit1, $exit2, ($eFiles -join ','), $eAllMatch, (Format-Speed $e1Speed), (Format-Speed $e2Speed), (Format-Speed $eSpeed), $noteE) }

# ---------------------------------------------------------------------------
# Item F: no leftover .idm-part and no 0-byte files
# ---------------------------------------------------------------------------
Write-Host "`n=== F: cleanup check ==="
$leftoverParts = @(Get-ChildItem -Path $Dir -Force | Where-Object { $_.Name -like '*.idm-part' }).Count
$zeroFiles = @(Get-ChildItem -Path $Dir -Force | Where-Object { (-not $_.PSIsContainer) -and ($_.Name -ne 'logs') -and ($_.Length -eq 0) }).Count
$passF = ($leftoverParts -eq 0) -and ($zeroFiles -eq 0)
Write-Host ("  leftover .idm-part: {0}" -f $leftoverParts)
Write-Host ("  0-byte files      : {0}" -f $zeroFiles)
Write-Host ("  result  : {0}" -f $(if ($passF) { 'PASS' } else { 'FAIL' }))
$results += [PSCustomObject]@{ Item = 'F'; Desc = 'no leftover .idm-part / 0-byte files'; Pass = $passF; Elapsed = '-'; Speed = '-'; WithinCap = '-'; Sha = '-'; Detail = ("parts={0}, zero={1}" -f $leftoverParts, $zeroFiles) }

# ---------------------------------------------------------------------------
# Summary table (console)
# ---------------------------------------------------------------------------
Write-Host "`n================ SUMMARY ================"
Write-Host ("{0,-4} {1,-42} {2}" -f 'ITEM', 'DESCRIPTION', 'RESULT')
foreach ($r in $results) {
    Write-Host ("{0,-4} {1,-42} {2}" -f $r.Item, $r.Desc, $(if ($r.Pass) { 'PASS' } else { 'FAIL' }))
}
$anyFail = ($results | Where-Object { -not $_.Pass }).Count -gt 0
Write-Host ("Overall: {0}" -f $(if ($anyFail) { 'FAIL' } else { 'PASS' }))

# ---------------------------------------------------------------------------
# Speed comparison (single vs 8 connections)
# ---------------------------------------------------------------------------
Write-Host "`n--- speed comparison ---"
Write-Host ("A (1 conn) : {0}" -f (Format-Speed $aSpeed))
Write-Host ("B (8 conn) : {0}" -f (Format-Speed $bSpeed))
if (($aSpeed -gt 0) -and ($bSpeed -gt 0)) {
    $ratio = $bSpeed / $aSpeed
    Write-Host ("ratio B/A  : {0:F2}x" -f $ratio)
}
else {
    Write-Host ("ratio B/A  : n/a")
}

# ---------------------------------------------------------------------------
# Write full results to docs/phase1b-network-acceptance.md
# ---------------------------------------------------------------------------
$probe = Get-ProbeInfo $resA.Output
$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine('# Phase 1b.1 Real-Network Acceptance')
[void]$sb.AppendLine('')
if ($mirror -eq '127.0.0.1' -or $mirror -eq 'localhost') {
    [void]$sb.AppendLine('> NOTE: this file currently records a SCRIPT SELF-TEST against a loopback throttled server, not the real-network acceptance. The real run on the user machine overwrites this file.')
    [void]$sb.AppendLine('')
}
[void]$sb.AppendLine('- Test time: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
[void]$sb.AppendLine('- URL: ' + $Url)
[void]$sb.AppendLine('- Mirror: ' + $mirror)
[void]$sb.AppendLine('- File size (Probe): ' + $(if ($probe.total) { $probe.total.ToString() } else { 'n/a' }))
[void]$sb.AppendLine('- Supports Range (Probe): ' + $(if ($probe.range) { $probe.range } else { 'n/a' }))
[void]$sb.AppendLine('- Official SHA256: ' + $Sha256)
[void]$sb.AppendLine('- MaxSpeed: ' + $MaxSpeed + ' (' + $limitBytes + ' B/s)')
[void]$sb.AppendLine('- E per-process MaxSpeed: ' + $halfSpeedText + ' B/s (half of MaxSpeed; combined cap ' + $limitBytes + ' B/s)')
[void]$sb.AppendLine('- Speed cap rule: a download exceeding 120% of its cap is FAIL (' + $limitNotEffective + ' / RATE LIMIT NOT EFFECTIVE)')
[void]$sb.AppendLine('- KeepFiles: ' + $(if ($KeepFiles) { 'yes' } else { 'no (deleted after run)' }))
[void]$sb.AppendLine('')
[void]$sb.AppendLine('## Items A-F')
[void]$sb.AppendLine('')
[void]$sb.AppendLine('| Item | Description | Command | Key output | Elapsed | Avg speed | Within cap | SHA256 | Result |')
[void]$sb.AppendLine('|------|-------------|---------|-----------|---------|-----------|------------|--------|--------|')
foreach ($r in $results) {
    $cmd = switch ($r.Item) {
        'A' { $cmdA }
        'B' { $cmdB }
        'C' { 'Get-FileHash -Algorithm SHA256' }
        'D' { $cmdD }
        'E' { $cmdE }
        'F' { '(directory scan)' }
        default { '' }
    }
    [void]$sb.AppendLine(('| {0} | {1} | `{2}` | {3} | {4} | {5} | {6} | `{7}` | {8} |' -f $r.Item, $r.Desc, $cmd, $r.Detail, $r.Elapsed, $r.Speed, $r.WithinCap, $r.Sha, $(if ($r.Pass) { 'PASS' } else { 'FAIL' })))
}
[void]$sb.AppendLine('')
[void]$sb.AppendLine('## Per-item details')
[void]$sb.AppendLine('')
foreach ($r in $results) {
    [void]$sb.AppendLine('- ' + $r.Item + ': ' + $r.Detail + ' | elapsed=' + $r.Elapsed + ' | speed=' + $r.Speed + ' | within_cap=' + $r.WithinCap + ' | sha256=' + $r.Sha + ' | ' + $(if ($r.Pass) { 'PASS' } else { 'FAIL' }))
}
[void]$sb.AppendLine('')
[void]$sb.AppendLine('## Speed comparison')
[void]$sb.AppendLine('')
[void]$sb.AppendLine('- A (1 connection): ' + (Format-Speed $aSpeed) + ' (' + $aElapsedSec.ToString('F2') + 's)')
[void]$sb.AppendLine('- B (8 connections): ' + (Format-Speed $bSpeed) + ' (' + $bElapsedSec.ToString('F2') + 's)')
if (($aSpeed -gt 0) -and ($bSpeed -gt 0)) {
    [void]$sb.AppendLine('- Ratio B/A: ' + ($bSpeed / $aSpeed).ToString('F2') + 'x')
}
else {
    [void]$sb.AppendLine('- Ratio B/A: n/a')
}
[void]$sb.AppendLine('')
[void]$sb.AppendLine('## Raw logs')
[void]$sb.AppendLine('')
[void]$sb.AppendLine('Per-step logs are under: ' + $logs)
[void]$sb.AppendLine('')
[void]$sb.AppendLine('## Notes / discrepancies')
[void]$sb.AppendLine('')
if ($mirror -eq '127.0.0.1' -or $mirror -eq 'localhost') {
    [void]$sb.AppendLine('- This run used a loopback HTTP server. It is a SELF-TEST of this script''s A-F logic only, NOT the real-network acceptance.')
    [void]$sb.AppendLine('- The loopback server was bandwidth-throttled on purpose so that item D''s --cancel-after 15 cancels an in-progress download.')
    [void]$sb.AppendLine('- The served bytes were a real published file with an official upstream SHA256, so items C/E remain meaningful.')
    [void]$sb.AppendLine('- Because the throttle is global, the A-vs-B speed ratio from a loopback run is NOT representative of real multi-connection speedup.')
}
else {
    if ($speedViolations.Count -eq 0) {
        [void]$sb.AppendLine('- None (or note any deviation observed during the run).')
    }
}
if ($speedViolations.Count -gt 0) {
    foreach ($v in $speedViolations) {
        [void]$sb.AppendLine('- ' + $limitNotEffective + ' / RATE LIMIT NOT EFFECTIVE: ' + $v)
    }
}

Set-Content -Path $docPath -Value $sb.ToString() -Encoding utf8
Write-Host ("`nFull report written to: {0}" -f $docPath)

# ---------------------------------------------------------------------------
# Cleanup (delete downloaded files unless -KeepFiles)
# ---------------------------------------------------------------------------
if (-not $KeepFiles) {
    Write-Host "`nCleaning downloaded files (keeping logs)..."
    foreach ($f in @(Get-ChildItem -Path $Dir -Force | Where-Object { (-not $_.PSIsContainer) -and ($_.Name -ne 'logs') })) {
        Remove-Item -Path $f.FullName -Force -ErrorAction SilentlyContinue
    }
}
else {
    Write-Host "`nKeepFiles set: downloaded files retained in $Dir"
}

if ($anyFail) { exit 1 } else { exit 0 }
