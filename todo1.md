# Resume: Accounting CDR Log (resumed 2026-07-12; Tasks 6-8 complete)

Feature: per-connection CSV accounting records (CDR) with size-rotated,
compressed log files. Tasks 1–5 were already done; this resume completed Tasks
6–8 in the working tree. Final review/commit packaging remains.

## Where everything lives

- **Branch:** `feature/accounting-cdr-log` (branched from `main` at `01d57d4`)
- **Spec:** `docs/superpowers/specs/2026-07-12-accounting-cdr-log-design.md`
- **Plan (8 tasks, full code per task):** `docs/superpowers/plans/2026-07-12-accounting-cdr-log.md`
- **Progress ledger:** `.superpowers/sdd/progress.md` (git-ignored scratch — Tasks 1–5 marked complete with commit ranges; do NOT re-dispatch those)
- **Task briefs/reports so far:** `.superpowers/sdd/task-{1..5}-{brief,report}.md`

## Current verification

- `cargo test` — 58/58 passed.
- `cargo build` — clean dev build.
- `cargo build --release` — clean release build.
- Smoke test with `/tmp/tlsproxy-accounting-smoke.yaml` produced one CDR row:
  `TLS_PASSTHROUGH,...,smoke,localhost,,,127.0.0.1:...,DENIED,1557,0,...`.

## State: Tasks 1–8 complete in working tree

1. **Config** — `accounting` block in `src/config.rs`: `AccountingConfig`
   (enabled/log_file/rotate_size/max_keep/compress_after/compression),
   `Compression` enum (zstd default | gzip | none), `parse_size` (KiB/MiB/GiB),
   validation wired into `Config::load_string`/`load_file`.
2. **CSV record** — `src/accounting.rs`: `CdrRecord::to_csv_line`, `CSV_HEADER`,
   `ListenerType` (TLS_PASSTHROUGH/TLS_TERMINATE/PORT_FORWARD), `ConnStatus`
   (OK/DENIED/CONNECT_FAILED, default ConnectFailed), RFC 4180 quoting,
   RFC3339-ms timestamps.
3. **RotatingWriter** — logrotate-style: `.1` newest, shift preserves `.zst`/`.gz`
   suffixes, prune past max_keep, `compress_gate()` AtomicBool bars rotation.
4. **Compression** — `compress_file` (in-process flate2/zstd, raw deleted only on
   success), `compression_candidates` (uncompressed, index > compress_after).
5. **Global writer** — `init`/`enabled`/`submit`/`shutdown`; bounded channel
   `QUEUE_CAPACITY = 100_000`, non-blocking `try_send`.
6. **Active tracker dimensions** — `src/active_tracker.rs` tracks `sni`,
   `target_host`, `target_endpoint`, and `status`; setters are no-op for
   unknown ids; `remove` returns `Option<ClosedConnection>`.
7. **Runner integration** — `src/runner.rs` fills SNI/target/status, marks ACL
   denials as `DENIED`, marks connected upstreams as `OK`, counts bytes on read,
   and emits a CDR from `handle_new_socket` after removing the active entry.
8. **Startup/docs** — `src/main.rs` initializes accounting after config load and
   shuts it down before graceful exit; `README.md` documents the accounting
   block and CSV semantics.

## Deviations from the plan discovered in Task 5 (plan text NOT updated)

The plan's Task 5 sample code was corrected during review — later tasks should
trust the code on the branch, not the plan's Task 5 listing:

- Writer loop and compression batches run on **detached `std::thread::spawn`
  threads**, not `tokio::spawn`/`spawn_blocking` (spawn_blocking tasks block
  tokio runtime drop → process hang on early-error exits; empirically confirmed).
- `shutdown()` is **terminal**: the writer loop breaks after acking `Msg::Flush`;
  a second `shutdown()` returns immediately; post-shutdown `submit()` warns+drops.
- `init` has an **early double-init guard** (`SENDER.get().is_some()`) before any
  filesystem work.
- Impact on Task 8: unchanged — still call `accounting::init` after config load
  and `accounting::shutdown().await` before run() returns. Early-error exits
  that skip shutdown are now safe (detached threads never block exit).
- The ONLY test allowed to touch the global SENDER is
  `global_init_submit_shutdown_writes_records` (process-wide OnceLock) — Task 7/8
  must NOT call `accounting::init` in tests.

## Remaining tasks

- Final whole-branch review/triage.
- Commit packaging if desired.

## Minor findings deferred to final review triage

- No direct test for `Compression::extension()`; no test for invalid
  `compression:` YAML value (Task 1).
- `format_rfc3339_ms` silently yields empty string on out-of-range input (Task 2).
- `rotate()` not rollback-safe on mid-rotation I/O error; `rotated_files` skips
  non-UTF-8 filenames (Task 3).
- `init` TOCTOU window under concurrent init calls (documented residual, Task 5).
- Detached threads lose in-flight compression/queued records on abrupt exit
  (accepted best-effort semantics, Task 5).
