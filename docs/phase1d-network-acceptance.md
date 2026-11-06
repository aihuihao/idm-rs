# Phase 1d Real-Network Acceptance (chunk-level retry, rate limited)

- Test date: 2026-11-06 10:06:02 ~ 10:27:10 (local time)
- URL: `https://mirrors.tuna.tsinghua.edu.cn/debian-cd/13.7.0/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso`
- Official SHA256: `a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355`
- File size: 792723456 bytes (matches the mirror's `Content-Length`)
- **MaxSpeed: `1.5M` (1572864 B/s) on both processes**, `--connections 4`, `--log-file` per process
- Two processes ran **concurrently** (both started at 10:06:02) through `Win32_Process.Create`
  (WMI, detached), each wrapped by `run_one.ps1` so that stdout/stderr are redirected to a
  file; progress was polled every ~2.5 minutes from SQLite (`chunks.start/committed`).
- Each process used its own database and directory (`p1.db`/`dl1`, `p2.db`/`dl2`): the task
  text does not pin a database, and sharing one SQLite file between two writers would turn
  this into a store-contention test rather than a retry test.

## Results at a glance

| Process | Finished | Elapsed | Avg speed | Chunks | Retries | SHA256 matches official |
|---------|----------|---------|-----------|--------|---------|-------------------------|
| P1 (`dl1/debian-13.7.0-amd64-netinst.iso`) | 10:27:10 | 1265.22s | 626552 B/s | 4 | 0 | **yes** |
| P2 (`dl2/debian-13.7.0-amd64-netinst.iso`) | 10:15:40 | 575.66s | 1377072 B/s | 4 | 0 | **yes** |

Both files are 792723456 bytes and hash to
`a7ef94ac2fb9a7fec454552abd629b7cc9d5155c886165a45649f5ce6167e355` (the official value).

Neither process exceeded the cap: P2 averaged 1377072 B/s (87.5% of the 1572864 B/s cap),
P1 averaged 626552 B/s. P1 received roughly half the bandwidth for most of the run because
P2 was already saturating its own share; the cap is an upper bound, not a guarantee.

## Per-chunk summary tables (printed at completion)

P1 — `CHUNK SUMMARY` from `p1.out`:

| id | range | written | elapsed | avg speed | state | retries |
|----|-------|---------|---------|-----------|-------|---------|
| 0 | 0-198180863 | 198180864 | 1264.89s | 156678 B/s | Done | 0 |
| 1 | 198180864-396361727 | 198180864 | 1261.87s | 157053 B/s | Done | 0 |
| 2 | 396361728-594542591 | 198180864 | 1261.41s | 157110 B/s | Done | 0 |
| 3 | 594542592-792723455 | 198180864 | 1262.85s | 156931 B/s | Done | 0 |

P2 — `CHUNK SUMMARY` from `p2.out`:

| id | range | written | elapsed | avg speed | state | retries |
|----|-------|---------|---------|-----------|-------|---------|
| 0 | 0-198180863 | 198180864 | 572.99s | 345870 B/s | Done | 0 |
| 1 | 198180864-396361727 | 198180864 | 575.30s | 344483 B/s | Done | 0 |
| 2 | 396361728-594542591 | 198180864 | 575.28s | 344492 B/s | Done | 0 |
| 3 | 594542592-792723455 | 198180864 | 575.09s | 344609 B/s | Done | 0 |

## Retry statistics (from the two `--log-file` outputs)

| Process | Log lines | `chunk retry` (WARN) | `chunk retries exhausted` (WARN) | Any WARN at all |
|---------|-----------|----------------------|----------------------------------|-----------------|
| P1 (`p1.log`) | 195766 | 0 | 0 | 0 |
| P2 (`p2.log`) | 197654 | 0 | 0 | 0 |

**No retry occurred in either process**, so there is no retry error type or backoff duration
to report. This is an honest zero, not a suppressed count: the log files were written at
debug level by `--log-file`, every retry would have produced a `chunk retry` WARN carrying
`task_id`, `chunk_id`, `attempt`, `error`, `wait_ms` and `resume_from`.

## Notes

- The first launch attempt used `cmd.exe /c ... > file` through WMI and failed with
  `文件名、目录名或卷标语法不正确。` — the classic `cmd /c` quote-stripping problem, not a
  downloader error. Both processes were started again at 10:06:02 through a PowerShell
  wrapper script; those two exits produced no files and are not part of the results above.
- Downloaded ISO files and their `.idm-part` files were deleted after this report was produced.
- Raw runner output (outside the repository): `D:\idm-validation\p1.out`, `p2.out`,
  `p1.log`, `p2.log`, `p1.done`, `p2.done`, `prog.py`, `run_one.ps1`.
