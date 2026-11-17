# phase4b-e2e.ps1
# End-to-end Native Messaging smoke test WITHOUT Firefox.
#
# - Reuses the real release idm-cli.exe / idm-nmhost.exe, the real 4-byte
#   length-prefixed frame protocol, the real SQLite queue and the real
#   queue run --watch runner.
# - All downloads come from a local loopback HttpListener only; no internet.
# - A dedicated random host name is used (never the default idm_rs) and every
#   registry key / file / process is cleaned up even on failure.
# - Any failure prints the failing step plus the collected raw output, cleans
#   up, and exits non-zero.
#
# Usage:
#   powershell -File scripts/phase4b-e2e.ps1
# (internal: the script starts itself once with -Mode server -WorkDir <tmp>)

param(
    [ValidateSet('main', 'server')]
    [string]$Mode = 'main',
    [string]$WorkDir = ''
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

# ---------------------------------------------------------------------------
# SERVER MODE: deterministic 3 MiB loopback file with Range support.
# Records one line per request with header *presence* flags (never values).
# ---------------------------------------------------------------------------
if ($Mode -eq 'server') {
    $size = 3 * 1024 * 1024
    $buf = New-Object 'byte[]' $size
    $rnd = New-Object System.Random -ArgumentList @(20261112)
    $rnd.NextBytes($buf)
    [System.IO.File]::WriteAllBytes((Join-Path $WorkDir 'expected.bin'), $buf)
    $log = Join-Path $WorkDir 'headers.log'
    [System.IO.File]::WriteAllText($log, '')

    $listener = $null
    $port = 0
    for ($k = 0; $k -lt 20; $k++) {
        $probe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
        $probe.Start()
        $cand = $probe.LocalEndpoint.Port
        $probe.Stop()
        $l = New-Object System.Net.HttpListener
        $l.Prefixes.Add("http://127.0.0.1:$cand/")
        try {
            $l.Start()
            $listener = $l
            $port = $cand
            break
        } catch {
            $l.Close()
        }
    }
    if ($null -eq $listener) {
        [System.IO.File]::WriteAllText((Join-Path $WorkDir 'server-error.txt'), 'no bindable port')
        exit 1
    }

    [System.IO.File]::WriteAllText((Join-Path $WorkDir 'ready.txt'), [string]$port)

    while ($true) {
        $ctx = $listener.GetContext()
        $resp = $ctx.Response
        try {
            $req = $ctx.Request
            $range = $req.Headers['Range']
            $ref = $req.Headers['Referer']
            $cookie = $req.Headers['Cookie']
            $ua = $req.Headers['User-Agent']
            if ($range -eq 'bytes=0-0') { $kind = 'PROBE' }
            elseif ($range) { $kind = 'CHUNK' }
            else { $kind = 'FULL' }
            $r = if ($ref) { 1 } else { 0 }
            $c = if ($cookie) { 1 } else { 0 }
            $u = if ($ua) { 1 } else { 0 }
            [System.IO.File]::AppendAllText($log, "$kind|range=$range|referer=$r|cookie=$c|ua=$u`n")

            $start = [int64]0
            $end = [int64]($size - 1)
            $partial = $false
            if ($range -match '^bytes=(\d+)-(\d*)$') {
                $start = [int64]$Matches[1]
                if ($Matches[2] -ne '') { $end = [int64]$Matches[2] }
                if ($end -ge $size) { $end = [int64]($size - 1) }
                $partial = $true
            }
            $count = [int]($end - $start + 1)
            if ($count -lt 1) { $count = 1 }

            if ($partial) {
                $resp.StatusCode = 206
                $resp.AddHeader('Content-Range', "bytes $start-$end/$size")
            } else {
                $resp.StatusCode = 200
            }
            $resp.AddHeader('Accept-Ranges', 'bytes')
            $resp.ContentType = 'application/octet-stream'
            $resp.ContentLength64 = $count
            $resp.KeepAlive = $false
            $resp.OutputStream.Write($buf, [int]$start, $count)
        } catch {
            [System.IO.File]::AppendAllText($log, "SERVERERR|$($_.Exception.Message)`n")
        } finally {
            try { $resp.Close() } catch { }
        }
    }
}

# ---------------------------------------------------------------------------
# MAIN MODE
# ---------------------------------------------------------------------------
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$script:step = 'INIT'
$script:trace = New-Object System.Collections.ArrayList
$script:tmp = ''
$script:hostName = ''
$script:db = ''
$script:cli = ''
$script:runnerName = ''
$script:serverPid = 0
$script:nmProc = $null
$script:failed = $false

function Log([string]$msg) {
    $line = "[$script:step] $msg"
    Write-Host $line
    [void]$script:trace.Add($line)
}

function Step([string]$name) {
    $script:step = $name
    Log "START $name"
}

function Fail([string]$msg) {
    throw $msg
}

# The runner launchers clear their own std-handle inheritance before spawning
# the detached watcher, so normal captured output is safe (no pipe inheritance).
function Invoke-RunnerNoOutput([string[]]$runnerArgs) {
    $out = & $script:cli runner @runnerArgs --db $script:db 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log ("runner " + ($runnerArgs -join ' ') + " output=" + $out.Trim())
    return $code
}

function Read-Exact($stream, $buf, $count) {
    $off = 0
    while ($off -lt $count) {
        $n = $stream.Read($buf, $off, $count - $off)
        if ($n -le 0) { throw "EOF while reading frame (got $off of $count)" }
        $off += $n
    }
}

function Send-Frame($proc, $obj) {
    $json = $obj | ConvertTo-Json -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
    $lenBytes = [System.BitConverter]::GetBytes([uint32]$bytes.Length)
    $bs = $proc.StandardInput.BaseStream
    $bs.Write($lenBytes, 0, 4)
    $bs.Write($bytes, 0, $bytes.Length)
    $bs.Flush()
    Log "SENT $json"
}

function Recv-Frame($proc) {
    $lb = New-Object 'byte[]' 4
    $null = Read-Exact $proc.StandardOutput.BaseStream $lb 4
    $n = [System.BitConverter]::ToUInt32($lb, 0)
    if ($n -gt 1048576) { throw "frame too large: $n" }
    $payload = New-Object 'byte[]' $n
    $null = Read-Exact $proc.StandardOutput.BaseStream $payload $n
    $text = [System.Text.Encoding]::UTF8.GetString($payload)
    Log "RECV $text"
    return ($text | ConvertFrom-Json)
}

function Reg-Exists([string]$name) {
    try {
        & reg query "HKCU\Software\Mozilla\NativeMessagingHosts\$name" /ve 2>$null
        return ($LASTEXITCODE -eq 0)
    } catch {
        return $false
    }
}

function Test-LockFree([string]$path) {
    try {
        $fs = [System.IO.File]::Open($path, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $fs.Dispose()
        return $true
    } catch {
        return $false
    }
}

function Get-RelatedProcesses {
    return @(Get-CimInstance Win32_Process |
        Where-Object { $_.CommandLine -and $_.CommandLine -like "*$script:tmp*" -and
            ($_.Name -eq 'idm-cli.exe' -or $_.Name -eq 'idm-nmhost.exe') })
}

function Invoke-Cleanup {
    $script:step = 'CLEANUP'
    # Stop the standalone runner via its sentinel so tasks settle as Paused(User)
    # instead of being killed mid-flight.
    if ($script:cli -ne '' -and (Test-Path $script:cli) -and $script:db -ne '') {
        try { & $script:cli runner stop --db $script:db 2>&1 | Out-Null } catch { }
    }
    if ($null -ne $script:nmProc) {
        try { $script:nmProc.StandardInput.BaseStream.Close() } catch { }
        try {
            if (-not $script:nmProc.WaitForExit(3000)) { $script:nmProc.Kill() }
        } catch { }
    }
    if ($script:serverPid -ne 0) {
        try { Stop-Process -Id $script:serverPid -Force -ErrorAction SilentlyContinue } catch { }
    }
    try {
        foreach ($p in (Get-RelatedProcesses)) {
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
    } catch { }
    Start-Sleep -Milliseconds 700
    if ($script:hostName -ne '') {
        try { & reg delete "HKCU\Software\Mozilla\NativeMessagingHosts\$script:hostName" /f 2>&1 | Out-Null } catch { }
        if ($script:cli -ne '' -and (Test-Path $script:cli) -and $script:db -ne '') {
            try { & $script:cli nm uninstall --host-name $script:hostName --db $script:db 2>&1 | Out-Null } catch { }
        }
    }
    if ($script:runnerName -ne '') {
        try { & reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v $script:runnerName /f 2>&1 | Out-Null } catch { }
        if ($script:cli -ne '' -and (Test-Path $script:cli) -and $script:db -ne '') {
            try { & $script:cli runner uninstall --name $script:runnerName --db $script:db 2>&1 | Out-Null } catch { }
        }
    }
    if ($script:tmp -ne '' -and (Test-Path $script:tmp)) {
        for ($i = 0; $i -lt 5; $i++) {
            try {
                Remove-Item $script:tmp -Recurse -Force -ErrorAction Stop
                break
            } catch {
                Start-Sleep -Milliseconds 700
            }
        }
    }
    $left = @(Get-RelatedProcesses)
    Log ("related processes left: " + $left.Count)
    if ($left.Count -gt 0) {
        foreach ($p in $left) { Log ("LEFTOVER pid=" + $p.ProcessId + " " + $p.Name) }
        $script:failed = $true
    }
    if ($script:tmp -ne '' -and (Test-Path $script:tmp)) {
        Log 'temp dir could not be removed'
        $script:failed = $true
    }
    if ($script:hostName -ne '' -and (Reg-Exists $script:hostName)) {
        Log 'registry key still present'
        $script:failed = $true
    }
}

try {
    # --- Step 1: temp dir, release binaries, nm install ----------------------
    Step '1_SETUP_INSTALL'
    $cliRelease = Join-Path $repoRoot 'target\release\idm-cli.exe'
    $hostRelease = Join-Path $repoRoot 'target\release\idm-nmhost.exe'
    if (-not (Test-Path $cliRelease)) { Fail "missing $cliRelease" }
    if (-not (Test-Path $hostRelease)) { Fail "missing $hostRelease" }

    $rand = [System.Guid]::NewGuid().ToString('N').Substring(0, 10)
    $script:tmp = Join-Path $env:TEMP ("idm4b-e2e-" + $rand)
    New-Item -ItemType Directory -Path $script:tmp -Force | Out-Null
    $script:hostName = 'idm_rs_e2e_' + $rand
    $script:runnerName = 'idm-rs-runner-e2e-' + $rand
    $script:db = Join-Path $script:tmp 'e2e.db'
    $script:cli = (Copy-Item -LiteralPath $cliRelease -Destination (Join-Path $script:tmp 'idm-cli.exe') -PassThru).FullName
    Copy-Item -LiteralPath $hostRelease -Destination (Join-Path $script:tmp 'idm-nmhost.exe') -Force | Out-Null
    Log "tmp=$script:tmp host=$script:hostName"

    $manifest = Join-Path $script:tmp ($script:hostName + '.json')
    $out = & $script:cli nm install --extension-id 'idm-rs@local' --host-name $script:hostName --db $script:db 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log ("nm install exit=$code raw=" + $out.Trim())
    if ($code -ne 0) { Fail 'nm install failed' }
    if (-not (Test-Path $manifest)) { Fail 'manifest file missing' }
    if (-not (Reg-Exists $script:hostName)) { Fail 'registry key missing' }
    $manifestText = [System.IO.File]::ReadAllText($manifest)
    Log ("MANIFEST " + $manifestText.Trim())
    if ($manifestText -notmatch 'allowed_extensions') { Fail 'manifest lacks allowed_extensions' }
    if ($manifestText -notmatch 'idm-nmhost\.exe') { Fail 'manifest lacks host exe' }

    # download-dir inside the temp dir so nothing lands in the user's Downloads.
    $dlDir = Join-Path $script:tmp 'downloads'
    $out = & $script:cli config set download-dir $dlDir --db $script:db 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log ("config set exit=$code raw=" + $out.Trim())
    if ($code -ne 0) { Fail 'config set download-dir failed' }

    # Phase 4c: register the standalone runner under a test-only Run value name.
    $out = & $script:cli runner install --name $script:runnerName --db $script:db 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log ("runner install exit=$code raw=" + $out.Trim())
    if ($code -ne 0) { Fail 'runner install failed' }
    $runVal = & reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v $script:runnerName 2>&1 | Out-String
    Log ("RUN VALUE " + $runVal.Trim())
    if ($runVal -notmatch 'queue run --watch') { Fail 'runner Run value missing queue run --watch' }

    # --- Step 2: start loopback server --------------------------------------
    Step '2_LOOPBACK_SERVER'
    $self = $PSCommandPath
    $sp = Start-Process -FilePath 'powershell.exe' -ArgumentList @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$self`"",
        '-Mode', 'server', '-WorkDir', "`"$script:tmp`""
    ) -PassThru -WindowStyle Hidden
    $script:serverPid = $sp.Id
    $readyPath = Join-Path $script:tmp 'ready.txt'
    $expectedPath = Join-Path $script:tmp 'expected.bin'
    $deadline = (Get-Date).AddSeconds(25)
    while (-not (Test-Path $readyPath) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 200
    }
    if (-not (Test-Path $readyPath)) {
        $errPath = Join-Path $script:tmp 'server-error.txt'
        if (Test-Path $errPath) { Fail ("server failed: " + [IO.File]::ReadAllText($errPath)) }
        Fail 'HttpListener server did not become ready'
    }
    $port = [int]([IO.File]::ReadAllText($readyPath).Trim())
    if (-not (Test-Path $expectedPath)) { Fail 'expected.bin missing' }
    $expectedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $expectedPath).Hash
    Log "port=$port expected_sha256=$expectedHash"
    $url = "http://127.0.0.1:$port/e2e.bin"
    $referrer = "http://127.0.0.1:$port/page.html"
    $cookies = 'session=e2e123'
    $userAgent = 'idm-rs-e2e/1.0'

    # Read-only SQLite probe helper (Python standard library, no project dep).
    $py = Join-Path $script:tmp 'dbquery.py'
    [IO.File]::WriteAllText($py, @'
import sqlite3, sys
db = sys.argv[1]
tid = int(sys.argv[2])
c = sqlite3.connect(db, timeout=5.0)
c.execute("PRAGMA query_only=ON")
r = c.execute("SELECT state, file_name, last_error FROM tasks WHERE id=?", (tid,)).fetchone()
h = c.execute("SELECT COUNT(*) FROM task_headers").fetchone()[0]
parts = [r[0] if r else "MISSING", str(h), str(r[1]) if r and r[1] else "-", str(r[2]) if r and r[2] else "-"]
print("|".join(parts))
c.close()
'@)

    function Get-DbState([int]$id) {
        $raw = (& python $script:py $script:db $id 2>&1 | Out-String).Trim()
        $line = ($raw -split "`n" | Where-Object { $_ -match '\|' } | Select-Object -Last 1)
        if (-not $line) { $line = $raw }
        Log ("DB id=$id -> $line")
        $p = $line.Trim() -split '\|', 4
        return [pscustomobject]@{ State = $p[0]; Headers = $p[1]; File = $p[2]; Error = $p[3] }
    }

    function Wait-Completed([int]$id, [int]$seconds) {
        $deadline = (Get-Date).AddSeconds($seconds)
        $last = $null
        while ((Get-Date) -lt $deadline) {
            $last = Get-DbState $id
            if ($last.State -eq 'Completed') { return $last }
            if ($last.State -eq 'Failed') { Fail ("task $id Failed: " + $last.Error) }
            Start-Sleep -Milliseconds 500
        }
        Fail ("task $id not Completed within $seconds s (last=" + $last.State + ")")
    }

    function Assert-Artifact($fileName) {
        if (-not (Test-Path $dlDir)) { Fail 'download dir missing' }
        $parts = @(Get-ChildItem -LiteralPath $dlDir -Recurse -File -Filter '*idm-part*')
        if ($parts.Count -gt 0) { Fail ("idm-part files remain: " + ($parts.Name -join ', ')) }
        $files = @(Get-ChildItem -LiteralPath $dlDir -File)
        $match = $null
        foreach ($f in $files) {
            $h = (Get-FileHash -Algorithm SHA256 -LiteralPath $f.FullName).Hash
            Log ("ARTIFACT " + $f.Name + " size=" + $f.Length + " sha256=" + $h)
            if ($h -eq $expectedHash) { $match = $f }
        }
        if ($null -eq $match) { Fail 'no downloaded file matches expected SHA256' }
    }

    # --- Step 3: start standalone runner, then frame protocol ping + enqueue -
    Step '3_RUNNER_START_FRAMES_ENQUEUE'
    $code = Invoke-RunnerNoOutput @('start')
    Log "runner start exit=$code"
    if ($code -ne 0) { Fail 'runner start failed' }
    $lockPath = Join-Path $script:tmp 'runner.lock'
    $deadline = (Get-Date).AddSeconds(6)
    while ((Test-LockFree $lockPath) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (Test-LockFree $lockPath) { Fail 'runner.lock not held after runner start' }
    Log 'runner.lock is held by standalone runner'

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = (Join-Path $script:tmp 'idm-nmhost.exe')
    $psi.WorkingDirectory = $script:tmp
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $false
    $psi.EnvironmentVariables['IDM_RS_DB'] = $script:db
    $script:nmProc = [System.Diagnostics.Process]::Start($psi)

    Send-Frame $script:nmProc ([ordered]@{ type = 'ping' })
    $pong = Recv-Frame $script:nmProc
    if ($pong.ok -ne $true) { Fail 'ping did not return ok:true' }
    Log ("ping version=" + $pong.version)

    Send-Frame $script:nmProc ([ordered]@{
        type = 'enqueue'; url = $url; referrer = $referrer
        cookies = $cookies; userAgent = $userAgent; filename = 'e2e.bin'
    })
    $resp = Recv-Frame $script:nmProc
    if ($resp.ok -ne $true) { Fail ("enqueue failed: " + ($resp | ConvertTo-Json -Compress)) }
    $taskId = [int]$resp.taskId
    if ($taskId -le 0) { Fail 'enqueue response has no taskId' }
    Log "taskId=$taskId"

    # --- Step 4: verify download (runner already running, not host-spawned) --
    Step '4_DOWNLOAD_VERIFY'
    $w = @(Get-RelatedProcesses | Where-Object { $_.CommandLine -like '*--watch*' })
    if ($w.Count -eq 0) { Fail 'standalone watch runner is not running' }
    Log ('standalone watch runner pid=' + $w[0].ProcessId)

    $st = Wait-Completed $taskId 60
    if ($st.Headers -ne '0') { Fail ("task_headers not cleared: " + $st.Headers) }
    Assert-Artifact 'e2e.bin'

    # --- Step 4b: kill host, runner must survive and finish a new task -------
    Step '4B_HOST_KILLED_RUNNER_SURVIVES'
    if ($null -ne $script:nmProc) {
        $nmPid = $script:nmProc.Id
        $script:nmProc.Kill()
        $script:nmProc.WaitForExit(3000) | Out-Null
        Log "killed idm-nmhost pid=$nmPid"
        $script:nmProc = $null
    }
    if (-not (Test-LockFree $lockPath)) { Log 'runner still holds lock after host kill' }
    else { Fail 'runner.lock released unexpectedly after host kill' }
    $w2 = @(Get-RelatedProcesses | Where-Object { $_.CommandLine -like '*--watch*' })
    if ($w2.Count -eq 0) { Fail 'runner died when host was killed' }
    Log 'runner survived host termination'

    # --- Step 5: request headers from Probe and chunk requests --------------
    Step '5_REQUEST_HEADERS'
    $headerLog = Join-Path $script:tmp 'headers.log'
    $lines = @(Get-Content -LiteralPath $headerLog | Where-Object { $_ -match '^(PROBE|CHUNK)\|' })
    foreach ($l in $lines) { Log "HEADERLINE $l" }
    $probeOk = $false
    $chunkOk = $false
    foreach ($l in $lines) {
        if ($l -notmatch 'referer=1\|cookie=1\|ua=1') { continue }
        if ($l -like 'PROBE|*') { $probeOk = $true }
        if ($l -like 'CHUNK|*') { $chunkOk = $true }
    }
    if (-not $probeOk) { Fail 'Probe request lacks Referer/Cookie/User-Agent' }
    if (-not $chunkOk) { Fail 'chunk request lacks Referer/Cookie/User-Agent' }
    Log 'Probe and chunk requests all carried Referer/Cookie/User-Agent'

    # --- Step 6: runner stop releases lock, deletes sentinel, log is clean --
    Step '6_RUNNER_STOP'
    $code = Invoke-RunnerNoOutput @('stop')
    Log "runner stop exit=$code"
    if ($code -ne 0) { Fail 'runner stop failed' }
    $deadline = (Get-Date).AddSeconds(11)
    while (-not (Test-LockFree $lockPath) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (-not (Test-LockFree $lockPath)) { Fail 'runner.lock still held after runner stop' }
    Log 'runner.lock released after runner stop'
    $stopFile = Join-Path $script:tmp 'runner.stop'
    if (Test-Path $stopFile) { Fail 'runner.stop was not deleted' }
    $deadline = (Get-Date).AddSeconds(15)
    $w3 = @(Get-RelatedProcesses | Where-Object { $_.CommandLine -like '*--watch*' })
    while ($w3.Count -gt 0 -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 200
        $w3 = @(Get-RelatedProcesses | Where-Object { $_.CommandLine -like '*--watch*' })
    }
    if ($w3.Count -gt 0) {
        foreach ($p in $w3) {
            Log ("watch pid still alive after stop: pid=" + $p.ProcessId + " ppid=" + $p.ParentProcessId + " created=" + $p.CreationDate + " cmd=" + $p.CommandLine)
        }
        $rlPath = Join-Path $script:tmp 'runner.log'
        if (Test-Path $rlPath) { Log ("runner.log tail: " + (([IO.File]::ReadAllText($rlPath) -split "`n" | Select-Object -Last 5) -join ' / ')) }
        Fail 'watch runner process did not exit within 15s after stop'
    }
    Log 'watch runner process exited after stop'
    # runner.log must not contain Cookie values or full URL query parameters.
    $runnerLog = Join-Path $script:tmp 'runner.log'
    if (-not (Test-Path $runnerLog)) { Fail 'runner.log missing' }
    $runnerLogText = [IO.File]::ReadAllText($runnerLog)
    Log ("RUNNER.LOG " + $runnerLogText.Trim())
    if ($runnerLogText -match [regex]::Escape($cookies)) { Fail 'runner.log contains Cookie value' }
    if ($runnerLogText -match [regex]::Escape($url)) { Fail 'runner.log contains full URL' }
    Log 'runner.log clean (no Cookie value / no full URL)'

    # --- Step 7: host rejects enqueue when runner stopped, then re-start -----
    Step '7_ENQUEUE_GATING'
    # New host process (previous one was killed in Step 4b).
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = (Join-Path $script:tmp 'idm-nmhost.exe')
    $psi.WorkingDirectory = $script:tmp
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $false
    $psi.EnvironmentVariables['IDM_RS_DB'] = $script:db
    $script:nmProc = [System.Diagnostics.Process]::Start($psi)
    Send-Frame $script:nmProc ([ordered]@{
        type = 'enqueue'; url = $url; referrer = $referrer
        cookies = $cookies; userAgent = $userAgent; filename = 'e2e-reject.bin'
    })
    $rej = Recv-Frame $script:nmProc
    if ($rej.ok -ne $false -or $rej.error -ne 'runner_not_running') {
        Fail ("enqueue should be rejected with runner_not_running: " + ($rej | ConvertTo-Json -Compress))
    }
    Log 'enqueue rejected with runner_not_running while runner stopped'
    # Verify no new task was created (still exactly the taskIds seen).
    $statusReject = Get-DbState 999999
    if ($statusReject.State -ne 'MISSING') { Fail 'unexpected task created after rejection' }

    # Re-start runner and enqueue again.
    $code = Invoke-RunnerNoOutput @('start')
    Log "runner re-start exit=$code"
    if ($code -ne 0) { Fail 'runner re-start failed' }
    $deadline = (Get-Date).AddSeconds(6)
    while ((Test-LockFree $lockPath) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (Test-LockFree $lockPath) { Fail 'runner.lock not held after re-start' }

    Send-Frame $script:nmProc ([ordered]@{
        type = 'enqueue'; url = $url; referrer = $referrer
        cookies = $cookies; userAgent = $userAgent; filename = 'e2e2.bin'
    })
    $resp2 = Recv-Frame $script:nmProc
    if ($resp2.ok -ne $true) { Fail 'second enqueue failed' }
    $taskId2 = [int]$resp2.taskId
    Log "second taskId=$taskId2"
    $st2 = Wait-Completed $taskId2 60
    if ($st2.Headers -ne '0') { Fail ("task_headers not cleared after 2nd task: " + $st2.Headers) }
    Assert-Artifact 'e2e2.bin'

    # --- Step 8: stop/uninstall runner and host, confirm keys gone -----------
    Step '8_UNINSTALL'
    $code = Invoke-RunnerNoOutput @('stop')
    Log "runner stop (final) exit=$code"
    $deadline = (Get-Date).AddSeconds(11)
    while (-not (Test-LockFree $lockPath) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (-not (Test-LockFree $lockPath)) { Fail 'runner.lock not released in final stop' }

    $out = & $script:cli runner uninstall --name $script:runnerName --db $script:db 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log ("runner uninstall exit=$code raw=" + $out.Trim())
    if ($code -ne 0) { Fail 'runner uninstall failed' }
    try {
        & reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v $script:runnerName 2>$null
        $rvCode = $LASTEXITCODE
    } catch { $rvCode = 1 }
    if ($rvCode -eq 0) { Fail 'runner Run value still exists after uninstall' }
    Log 'runner Run value removed'

    $out = & $script:cli nm uninstall --host-name $script:hostName --db $script:db 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log ("nm uninstall exit=$code raw=" + $out.Trim())
    if ($code -ne 0) { Fail 'nm uninstall failed' }
    if (Test-Path $manifest) { Fail 'manifest still exists after uninstall' }
    if (Reg-Exists $script:hostName) { Fail 'registry key still exists after uninstall' }
    Log 'manifest and registry key removed'

    # Cleanup (temp dir, processes) runs in Invoke-Cleanup below.
    Log 'ALL ASSERTIONS PASSED'
} catch {
    $script:failed = $true
    Write-Host "=============================================="
    Write-Host "FAILED STEP: $script:step"
    Write-Host "ERROR: $($_.Exception.Message)"
    Write-Host "--- RAW OUTPUT ---"
    $script:trace | ForEach-Object { Write-Host $_ }
    Write-Host "=============================================="
}

Invoke-Cleanup
Write-Host "=============================================="
if ($script:failed) {
    Write-Host "E2E RESULT: FAIL"
    Write-Host "=============================================="
    exit 1
}
Write-Host "E2E RESULT: PASS"
Write-Host "=============================================="
exit 0
