# Resume: Accounting CDR Log (paused 2026-07-12 after Task 5 of 8)

Feature: per-connection CSV accounting records (CDR) with size-rotated,
compressed log files. Paused at a clean boundary — Tasks 1–5 done, reviewed,
approved; Tasks 6–8 + final review remain.

## Where everything lives

- **Branch:** `feature/accounting-cdr-log` (branched from `main` at `01d57d4`)
- **Spec:** `docs/superpowers/specs/2026-07-12-accounting-cdr-log-design.md`
- **Plan (8 tasks, full code per task):** `docs/superpowers/plans/2026-07-12-accounting-cdr-log.md`
- **Progress ledger:** `.superpowers/sdd/progress.md` (git-ignored scratch — Tasks 1–5 marked complete with commit ranges; do NOT re-dispatch those)
- **Task briefs/reports so far:** `.superpowers/sdd/task-{1..5}-{brief,report}.md`

## How to resume

Use the `superpowers:subagent-driven-development` skill, resuming at **Task 6**
(active_tracker extension). Per-task flow: `scripts/task-brief PLAN 6` →
dispatch implementer subagent → `scripts/review-package BASE HEAD` → dispatch
task reviewer → fix loop if needed → append to ledger. Then Task 7, Task 8,
then the final whole-branch review (`scripts/review-package $(git merge-base
main HEAD) HEAD`, most capable model), then `superpowers:finishing-a-development-branch`.

Focused test command in this crate: `cargo test accounting` (no lib target —
plain `cargo test <filter>`). Full suite currently 54/54 green, `cargo check`
clean.

## State: Tasks 1–5 complete (commits f6765b4..04427dc)

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

## Remaining tasks (details + full code in the plan)

- **Task 6:** extend `src/active_tracker.rs` — sni/target_host/target_endpoint/
  status fields, `set_sni`/`set_target`/`set_status`, `remove` returns
  `Option<ClosedConnection>`.
- **Task 7:** `src/runner.rs` integration — workers fill dimensions, ACL denial
  sets `Denied`, upstream-connect success sets `Ok`, byte counting moves to the
  read side of `pipe`, `handle_new_socket` builds+submits the CDR after
  `active_tracker::remove`; new denied-connection test.
- **Task 8:** `src/main.rs` wiring (init after config load, shutdown before
  Ok(())), README section, end-to-end smoke test with a scratch config in /tmp
  (do NOT modify repo config.yaml).

## Minor findings deferred to final review triage

- No direct test for `Compression::extension()`; no test for invalid
  `compression:` YAML value (Task 1).
- `format_rfc3339_ms` silently yields empty string on out-of-range input (Task 2).
- `rotate()` not rollback-safe on mid-rotation I/O error; `rotated_files` skips
  non-UTF-8 filenames (Task 3).
- `init` TOCTOU window under concurrent init calls (documented residual, Task 5).
- Detached threads lose in-flight compression/queued records on abrupt exit
  (accepted best-effort semantics, Task 5).
