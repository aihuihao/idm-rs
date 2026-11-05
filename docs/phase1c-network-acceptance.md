# Phase 1c Real-Network Acceptance (rate limited)

- Test date: 2026-11-05 15:10 ~ 16:12 (local time)
- URL: `https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso`
- Official SHA256: `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355`
- File size: 792723456 bytes
- **MaxSpeed: `3M` (3145728 B/s) on every real download** (G.1, G.3 resume, G.4 download, G.4 resume)
- Database: `D:\idm-validation\g_phase1c.db`
- Download dir: `D:\dl-test`
- `--log-file` (G.1 only): `D:\idm-validation\g_download.log`
- WMI detached runner; logs polled every 2-3 minutes; no process was terminated early except the two deliberate interruptions below.

## Results at a glance

| Step | Interruption | Progress before | State before | Elapsed | Progress after | SHA256 matches official |
|------|--------------|-----------------|--------------|---------|----------------|-------------------------|
| G.1 download (task 1) | `Stop-Process -Force` at 30% | 238239744 (30.05%) | Downloading | 354.03s | n/a | n/a |
| G.2 `list` | startup recovery | 238239744 / 792723456 | **Paused / Interrupted** | 1.07s | n/a | n/a |
| G.3 `resume 1` | none | 238239744 / 792723456 | Paused / Interrupted | 1314.26s | 792723456 (100%) | **yes** |
| G.4 download (task 2) | real `CTRL_C_EVENT` at 50% | 396738560 (50.05%) | Downloading | 960.77s | n/a | n/a |
| G.4 `list` | startup recovery | 396886016 / 792723456 | **Paused / User** | 1.08s | n/a | n/a |
| G.4 `resume 2` | none | 396886016 / 792723456 | Paused / User | 1046.85s | 792723456 (100%) | **yes** |

Both completed files are 792723456 bytes and hash to
`a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355`
(the official value). Task 1 file: `debian-13.7.0-amd64-netinst.iso`;
task 2 file: `debian-13.7.0-amd64-netinst (1).iso`.

## G.1 forced termination (no summary table)

G.1 was ended with `Stop-Process -Force` at 30.05% (238239744 bytes after
354.03s). Because the process was killed, it never reached its normal exit path,
so **no per-chunk summary table was printed for G.1**. Per the user's decision,
the summary tables recorded in this report are the ones printed when the G.3 and
G.4 resumes completed.

`idm-cli list` output after the kill (startup recovery turned Downloading into
Paused / Interrupted, as required by 9.4):

```text
ID | FILE | STATE | PAUSE | PROGRESS | LAST_ERROR
1 | debian-13.7.0-amd64-netinst.iso | Paused | Interrupted | 238239744/792723456 | -
```

## G.3 `resume 1`

`idm-cli resume 1 --db <db> --max-speed 3M` → `Completed`, 1314.26s,
`downloaded_this_run=554483712`, final SHA256 matches.

Per-chunk summary table printed at G.3 resume completion
(range starts at each chunk's `committed` position, not the original `start`):

| id | range | written | elapsed | avg speed | state |
|----|-------|---------|---------|-----------|-------|
| 0 | 30343168-99090431 | 68747264 | 1277.92s | 53796 B/s | Done |
| 1 | 129236992-198180863 | 68943872 | 1294.26s | 53269 B/s | Done |
| 2 | 229801984-297271295 | 67469312 | 1277.43s | 52817 B/s | Done |
| 3 | 327680000-396361727 | 68681728 | 1294.92s | 53039 B/s | Done |
| 4 | 425533440-495452159 | 69918720 | 1301.23s | 53733 B/s | Done |
| 5 | 524877824-594542591 | 69664768 | 1295.99s | 53754 B/s | Done |
| 6 | 623681536-693633023 | 69951488 | 1304.62s | 53618 B/s | Done |
| 7 | 721616896-792723455 | 71106560 | 1312.90s | 54160 B/s | Done |

## G.4 real Ctrl+C and `resume 2`

The second download ran in its own console with no output redirection. At
50.05% (396738560 bytes after 960.77s) a real Ctrl+C was delivered with
`FreeConsole` → `AttachConsole(pid)` → `SetConsoleCtrlHandler(NULL, TRUE)` →
`GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`:
`FreeConsole=True, AttachConsole=True, SetConsoleCtrlHandler=True,
GenerateConsoleCtrlEvent=True`, process exited within 10s. Neither
`Stop-Process` nor `--cancel-after` was used for this step.

`idm-cli list` after Ctrl+C:

```text
ID | FILE | STATE | PAUSE | PROGRESS | LAST_ERROR
1 | debian-13.7.0-amd64-netinst.iso | Completed | - | 792723456/792723456 | -
2 | debian-13.7.0-amd64-netinst (1).iso | Paused | User | 396886016/792723456 | -
```

`idm-cli resume 2 --db <db> --max-speed 3M` → `Completed`, 1046.85s,
`downloaded_this_run=395837440`, final SHA256 matches.

Per-chunk summary table printed at G.4 resume completion:

| id | range | written | elapsed | avg speed | state |
|----|-------|---------|---------|-----------|-------|
| 0 | 48037888-99090431 | 51052544 | 1034.30s | 49360 B/s | Done |
| 1 | 149028864-198180863 | 49152000 | 996.51s | 49324 B/s | Done |
| 2 | 246513664-297271295 | 50757632 | 1044.79s | 48582 B/s | Done |
| 3 | 348823552-396361727 | 47538176 | 1008.44s | 47140 B/s | Done |
| 4 | 445538304-495452159 | 49913856 | 1043.25s | 47844 B/s | Done |
| 5 | 547028992-594542591 | 47513600 | 976.13s | 48675 B/s | Done |
| 6 | 644186112-693633023 | 49446912 | 1007.38s | 49084 B/s | Done |
| 7 | 742260736-792723455 | 50462720 | 1026.67s | 49152 B/s | Done |

## Notes

- Observed throughput was below the 3M cap for most of the run (roughly 0.2-0.6 MB/s
  in the long transfers); the cap is an upper bound, not a guaranteed rate. No step
  exceeded 120% of the cap.
- The runner read progress directly from SQLite with a short Python script instead of
  repeatedly invoking `idm-cli list` during a live download, so startup recovery did
  not mutate a task that was still downloading.
- Raw runner output: `D:\idm-validation\g_results.txt`, `g_steps.log`,
  `g_phase*.out`, `g_launch.out` (outside the repository).
- Downloaded ISO files were deleted after this report was produced (G.6).
