# Phase 1b.1 Real-Network Acceptance

> NOTE: this file currently records a SCRIPT SELF-TEST against a loopback throttled server, not the real-network acceptance. The real run on the user machine overwrites this file.

- Test time: 2026-11-05 09:23:35
- URL: http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz
- Mirror: 127.0.0.1
- File size (Probe): 8780269
- Supports Range (Probe): true
- Official SHA256: 0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5
- KeepFiles: no (deleted after run)

## Items A-F

| Item | Description | Command | Key output | Elapsed | Avg speed | SHA256 | Result |
|------|-------------|---------|-----------|---------|-----------|--------|--------|
| A | single connection download | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 1` | file=node-v22.11.0-headers.tar.gz, size=8780269, elapsed=22.84s, speed=0.37 MB/s | 22.84s | 0.37 MB/s | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| B | 8-connection download (name (1).ext) | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 8` | file=node-v22.11.0-headers.tar (1).gz, size=8780269, elapsed=23.99s, speed=0.35 MB/s | 23.99s | 0.35 MB/s | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| C | SHA256 of A and B match official | `Get-FileHash -Algorithm SHA256` | a=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, b=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, expect=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | - | - | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| D | cancel keeps .idm-part; rename+delete OK | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 8 --cancel-after 15` | exit=1, part=node-v22.11.0-headers.tar (2).gz.idm-part, rename=True, delete=True | 16.16s | 0.37 MB/s | `-` | PASS |
| E | two concurrent downloads, two distinct files | `idm-cli.exe download "http://127.0.0.1:18765/node-v22.11.0-headers.tar.gz" --dir D:\dl-test --connections 4   (x2 concurrent)` | exit1=0, exit2=0, files=node-v22.11.0-headers.tar (3).gz,node-v22.11.0-headers.tar (2).gz, sha256ok=True | 48.37s | 0.35 MB/s | `0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, 0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5` | PASS |
| F | no leftover .idm-part / 0-byte files | `(directory scan)` | parts=0, zero=0 | - | - | `-` | PASS |

## Per-item details

- A: file=node-v22.11.0-headers.tar.gz, size=8780269, elapsed=22.84s, speed=0.37 MB/s | elapsed=22.84s | speed=0.37 MB/s | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- B: file=node-v22.11.0-headers.tar (1).gz, size=8780269, elapsed=23.99s, speed=0.35 MB/s | elapsed=23.99s | speed=0.35 MB/s | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- C: a=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, b=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, expect=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | elapsed=- | speed=- | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- D: exit=1, part=node-v22.11.0-headers.tar (2).gz.idm-part, rename=True, delete=True | elapsed=16.16s | speed=0.37 MB/s | sha256=- | PASS
- E: exit1=0, exit2=0, files=node-v22.11.0-headers.tar (3).gz,node-v22.11.0-headers.tar (2).gz, sha256ok=True | elapsed=48.37s | speed=0.35 MB/s | sha256=0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5, 0d42dc3b3377f49e495976dc0e4f5c3a7ffb1d714050d2f247afdbbc0898dae5 | PASS
- F: parts=0, zero=0 | elapsed=- | speed=- | sha256=- | PASS

## Speed comparison

- A (1 connection): 0.37 MB/s (22.84s)
- B (8 connections): 0.35 MB/s (23.99s)
- Ratio B/A: 0.95x

## Raw logs

Per-step logs are under: D:\dl-test\logs

## Notes / discrepancies

- This run used a loopback HTTP server. It is a SELF-TEST of this script's A-F logic only, NOT the real-network acceptance.
- The loopback server was bandwidth-throttled on purpose so that item D's --cancel-after 15 cancels an in-progress download.
- The served bytes were a real published file with an official upstream SHA256, so items C/E remain meaningful.
- Because the throttle is global, the A-vs-B speed ratio from a loopback run is NOT representative of real multi-connection speedup.

