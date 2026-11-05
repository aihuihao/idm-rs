# Phase 1b.1 Real-Network Acceptance

- Test time: 2026-11-05 12:29:33
- URL: https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso
- Mirror: mirrors.tuna.tsinghua.edu.cn
- File size (Probe): 792723456
- Supports Range (Probe): true
- Official SHA256: a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355
- MaxSpeed: 3M (3145728 B/s)
- E per-process MaxSpeed: 1572864 B/s (half of MaxSpeed; combined cap 3145728 B/s)
- Speed cap rule: a download exceeding 120% of its cap is FAIL (限速未生效 / RATE LIMIT NOT EFFECTIVE)
- KeepFiles: no (deleted after run)

## Items A-F

| Item | Description | Command | Key output | Elapsed | Avg speed | Within cap | SHA256 | Result |
|------|-------------|---------|-----------|---------|-----------|------------|--------|--------|
| A | single connection download | `idm-cli.exe download "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso" --dir D:\dl-test --connections 1 --max-speed 3M` | file=debian-13.7.0-amd64-netinst.iso, size=792723456, elapsed=1994.09s, speed=0.38 MB/s, cap=3145728 B/s, within_cap=yes | 1994.09s | 0.38 MB/s | yes | `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355` | PASS |
| B | 8-connection download (name (1).ext) | `idm-cli.exe download "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso" --dir D:\dl-test --connections 8 --max-speed 3M` | file=debian-13.7.0-amd64-netinst (1).iso, size=792723456, elapsed=1121.74s, speed=0.67 MB/s, cap=3145728 B/s, within_cap=yes | 1121.74s | 0.67 MB/s | yes | `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355` | PASS |
| C | SHA256 of A and B match official | `Get-FileHash -Algorithm SHA256` | a=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355, b=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355, expect=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355 | - | - | - | `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355` | PASS |
| D | cancel keeps .idm-part; rename+delete OK | `idm-cli.exe download "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso" --dir D:\dl-test --connections 8 --cancel-after 15 --max-speed 3M` | exit=1, part=debian-13.7.0-amd64-netinst (2).iso.idm-part, rename=True, delete=True, cap=3145728 B/s, within_cap=yes | 16.06s | 2.84 MB/s | yes | `-` | PASS |
| E | two concurrent downloads, two distinct files | `idm-cli.exe download "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso" --dir D:\dl-test --connections 4 --max-speed 1572864   (x2 concurrent; combined cap 3M)` | exit1=0, exit2=0, files=debian-13.7.0-amd64-netinst (2).iso,debian-13.7.0-amd64-netinst (3).iso, sha256ok=True, speed1=1.50 MB/s, speed2=0.89 MB/s, combined=1.79 MB/s, per_process_cap=1572864 B/s, combined_cap=3145728 B/s, within_cap=yes | 845.98s | 1.79 MB/s | yes | `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355, a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355` | PASS |
| F | no leftover .idm-part / 0-byte files | `(directory scan)` | parts=0, zero=0 | - | - | - | `-` | PASS |

## Per-item details

- A: file=debian-13.7.0-amd64-netinst.iso, size=792723456, elapsed=1994.09s, speed=0.38 MB/s, cap=3145728 B/s, within_cap=yes | elapsed=1994.09s | speed=0.38 MB/s | within_cap=yes | sha256=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355 | PASS
- B: file=debian-13.7.0-amd64-netinst (1).iso, size=792723456, elapsed=1121.74s, speed=0.67 MB/s, cap=3145728 B/s, within_cap=yes | elapsed=1121.74s | speed=0.67 MB/s | within_cap=yes | sha256=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355 | PASS
- C: a=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355, b=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355, expect=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355 | elapsed=- | speed=- | within_cap=- | sha256=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355 | PASS
- D: exit=1, part=debian-13.7.0-amd64-netinst (2).iso.idm-part, rename=True, delete=True, cap=3145728 B/s, within_cap=yes | elapsed=16.06s | speed=2.84 MB/s | within_cap=yes | sha256=- | PASS
- E: exit1=0, exit2=0, files=debian-13.7.0-amd64-netinst (2).iso,debian-13.7.0-amd64-netinst (3).iso, sha256ok=True, speed1=1.50 MB/s, speed2=0.89 MB/s, combined=1.79 MB/s, per_process_cap=1572864 B/s, combined_cap=3145728 B/s, within_cap=yes | elapsed=845.98s | speed=1.79 MB/s | within_cap=yes | sha256=a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355, a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355 | PASS
- F: parts=0, zero=0 | elapsed=- | speed=- | within_cap=- | sha256=- | PASS

## Speed comparison

- A (1 connection): 0.38 MB/s (1994.09s)
- B (8 connections): 0.67 MB/s (1121.74s)
- Ratio B/A: 1.78x

## Raw logs

Per-step logs are under: D:\dl-test\logs

## Notes / discrepancies

- None (or note any deviation observed during the run).

## Item G: real Ctrl+C test (rate limited)

- Helper script: `D:\idm-g-test\ctrl_c_test.ps1` (outside the repository, removed after the test).
  `Add-Type` embedded C# called `FreeConsole` -> `AttachConsole(target PID)` ->
  `SetConsoleCtrlHandler(NULL, TRUE)` -> `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`.
- `idm-cli.exe` was started with `Start-Process` in its own console window, with **no
  output redirection**, at 2026-11-05 13:33:11:
  `.\target\release\idm-cli.exe download "<same URL>" --dir D:\dl-test2 --connections 8 --max-speed 3M`
- Ctrl+C was sent after 15 s. Neither `Stop-Process` nor `--cancel-after` was used.
- Raw Win32 calls: FreeConsole=True (err=6), AttachConsole=True (err=0),
  SetConsoleCtrlHandler(NULL,TRUE)=True, GenerateConsoleCtrlEvent(CTRL_C_EVENT,0)=True (err=0).
- Exited within 10 s: True; exit code: 1 (non-zero).
- `.idm-part` present: `debian-13.7.0-amd64-netinst.iso.idm-part` (792723456 bytes);
  immediate `Rename-Item` = True, immediate `Remove-Item` = True.
- No `idm-cli.exe` process remained afterwards. Result: **PASS**.

### A-G summary

| Item | Description | Elapsed | Avg speed | Within cap | Result |
|------|-------------|---------|-----------|------------|--------|
| A | single connection, cap 3M | 1994.09s | 0.38 MB/s | yes | PASS |
| B | 8 connections, cap 3M | 1121.74s | 0.67 MB/s | yes | PASS |
| C | SHA256 of A and B vs official | - | - | - | PASS |
| D | cancel after 15 s, cap 3M | 16.06s | 2.84 MB/s | yes | PASS |
| E | 2 concurrent, cap 1.5M each | 845.98s | 1.79 MB/s combined (1.50 / 0.89 per process) | yes | PASS |
| F | no leftover `.idm-part` / 0-byte files | - | - | - | PASS |
| G | real Ctrl+C, cap 3M | ~15s (13:33:11 -> 13:33:26) | n/a (output not redirected) | yes (cap 3M applied) | PASS |

- Speed comparison: A 0.38 MB/s (1 connection) vs B 0.67 MB/s (8 connections), ratio B/A = 1.78x.
- SHA256: A = `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355`,
  B = `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355`,
  official = `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355` (both match).
- E's two files also matched the official SHA256. Every download in this run carried
  `--max-speed`; no unlimited real-network download was performed.

