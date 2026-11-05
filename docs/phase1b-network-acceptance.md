# Phase 1b.1 Real-Network Acceptance

> NOTE: this file currently records a SCRIPT SELF-TEST against a loopback throttled server, not the real-network acceptance. The real run on the user machine overwrites this file.

- Test time: 2026-11-05 11:17:43
- URL: http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz
- Mirror: 127.0.0.1
- File size (Probe): 8780269
- Supports Range (Probe): true
- Official SHA256: 0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5
- MaxSpeed: 3M (3145728 B/s)
- E per-process MaxSpeed: 1572864 B/s (half of MaxSpeed; combined cap 3145728 B/s)
- Speed cap rule: a download exceeding 120% of its cap is FAIL (限速未生效 / RATE LIMIT NOT EFFECTIVE)
- KeepFiles: no (deleted after run)

## Items A-F

| Item | Description | Command | Key output | Elapsed | Avg speed | Within cap | SHA256 | Result |
|------|-------------|---------|-----------|---------|-----------|------------|--------|--------|
| A | single connection download | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 1 --max-speed 3M` | file=node-v22.11.0-headers.tar.gz, size=8780269, elapsed=22.78s, speed=0.37 MB/s, cap=3145728 B/s, within_cap=yes | 22.78s | 0.37 MB/s | yes | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| B | 8-connection download (name (1).ext) | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 8 --max-speed 3M` | file=node-v22.11.0-headers.tar (1).gz, size=8780269, elapsed=23.70s, speed=0.35 MB/s, cap=3145728 B/s, within_cap=yes | 23.70s | 0.35 MB/s | yes | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| C | SHA256 of A and B match official | `Get-FileHash -Algorithm SHA256` | a=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, b=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, expect=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | - | - | - | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| D | cancel keeps .idm-part; rename+delete OK | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 8 --cancel-after 15 --max-speed 3M` | exit=1, part=node-v22.11.0-headers.tar (2).gz.idm-part, rename=True, delete=True, cap=3145728 B/s, within_cap=yes | 16.13s | 0.35 MB/s | yes | `-` | PASS |
| E | two concurrent downloads, two distinct files | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 4 --max-speed 1572864   (x2 concurrent; combined cap 3M)` | exit1=0, exit2=0, files=node-v22.11.0-headers.tar (3).gz,node-v22.11.0-headers.tar (2).gz, sha256ok=True, speed1=0.17 MB/s, speed2=0.18 MB/s, combined=0.35 MB/s, per_process_cap=1572864 B/s, combined_cap=3145728 B/s, within_cap=yes | 48.46s | 0.35 MB/s | yes | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, 0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| F | no leftover .idm-part / 0-byte files | `(directory scan)` | parts=0, zero=0 | - | - | - | `-` | PASS |

## Per-item details

- A: file=node-v22.11.0-headers.tar.gz, size=8780269, elapsed=22.78s, speed=0.37 MB/s, cap=3145728 B/s, within_cap=yes | elapsed=22.78s | speed=0.37 MB/s | within_cap=yes | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- B: file=node-v22.11.0-headers.tar (1).gz, size=8780269, elapsed=23.70s, speed=0.35 MB/s, cap=3145728 B/s, within_cap=yes | elapsed=23.70s | speed=0.35 MB/s | within_cap=yes | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- C: a=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, b=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, expect=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | elapsed=- | speed=- | within_cap=- | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- D: exit=1, part=node-v22.11.0-headers.tar (2).gz.idm-part, rename=True, delete=True, cap=3145728 B/s, within_cap=yes | elapsed=16.13s | speed=0.35 MB/s | within_cap=yes | sha256=- | PASS
- E: exit1=0, exit2=0, files=node-v22.11.0-headers.tar (3).gz,node-v22.11.0-headers.tar (2).gz, sha256ok=True, speed1=0.17 MB/s, speed2=0.18 MB/s, combined=0.35 MB/s, per_process_cap=1572864 B/s, combined_cap=3145728 B/s, within_cap=yes | elapsed=48.46s | speed=0.35 MB/s | within_cap=yes | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, 0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- F: parts=0, zero=0 | elapsed=- | speed=- | within_cap=- | sha256=- | PASS

## Speed comparison

- A (1 connection): 0.37 MB/s (22.78s)
- B (8 connections): 0.35 MB/s (23.70s)
- Ratio B/A: 0.96x

## Raw logs

Per-step logs are under: D:\dl-test\logs

## Notes / discrepancies

- This run used a loopback HTTP server. It is a SELF-TEST of this script's A-F logic only, NOT the real-network acceptance.
- The loopback server was bandwidth-throttled on purpose so that item D's --cancel-after 15 cancels an in-progress download.
- The served bytes were a real published file with an official upstream SHA256, so items C/E remain meaningful.
- Because the throttle is global, the A-vs-B speed ratio from a loopback run is NOT representative of real multi-connection speedup.

