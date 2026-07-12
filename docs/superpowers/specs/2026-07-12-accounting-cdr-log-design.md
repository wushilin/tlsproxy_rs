# Accounting CDR Log — Design

Date: 2026-07-12
Status: Approved pending user review

## Purpose

Per-connection usage records (CDR-style) for multi-tenant accounting. Every
accepted client connection produces one CSV line when it ends, carrying the
listener, SNI, target, byte counts, and timestamps, so tenants can be billed
by grouping on any column. Records go to a dedicated, size-rotated,
optionally compressed log file, independent of the application log.

## Configuration

New optional top-level `accounting` block in `config.yaml` / `src/config.rs`.
An absent block or `enabled: false` disables accounting entirely.

```yaml
accounting:
  enabled: true         # default false
  log_file: cdr.log     # required when enabled: true (config error otherwise)
  rotate_size: 18MiB    # default 100MiB; integer bytes or KiB/MiB/GiB suffix
  max_keep: 10          # default 10; rotated files kept (excludes active file)
  compress_after: 3     # default 3; rotated indexes 1..=N stay uncompressed
  compression: zstd     # default zstd; zstd (.zst) | gzip (.gz) | none
```

`rotate_size` accepts a plain integer (bytes) or a value with a `KiB`, `MiB`,
or `GiB` suffix. `compress_after` is ignored when `compression: none`.

## Record format

CSV, RFC 4180 quoting (fields containing comma, quote, or newline are
quoted; embedded quotes doubled). Fields are validated before writing:
control characters are rejected/stripped so a record can never corrupt the
file structure. Each newly created active file starts with a header row.

Columns, in order:

| Column | Content |
|---|---|
| `listener_type` | `TLS_PASSTHROUGH` \| `TLS_TERMINATE` \| `PORT_FORWARD`, from the listener's `mode` |
| `connection_id` | The connection's request id (as in the application log) |
| `listener_name` | Listener key from the config |
| `sni` | SNI hostname from the ClientHello; empty when none was received/parsed |
| `target_host` | Logical target host chosen for the connection; empty if never resolved |
| `target_endpoint` | Resolved upstream `ip:port` actually connected to; empty if never connected |
| `remote_address` | Client `ip:port` |
| `status` | `OK` \| `DENIED` (ACL) \| `CONNECT_FAILED` (upstream connect failure/timeout) |
| `uploaded_bytes` | Bytes the proxy **received from the client** |
| `downloaded_bytes` | Bytes the proxy **received from upstream** |
| `connection_start` | RFC3339 UTC with milliseconds |
| `connection_end` | RFC3339 UTC with milliseconds |

Byte-count semantics: traffic is billed as received on each leg, regardless
of whether it was successfully relayed to the other side — the cost was
already incurred on the wire. A denied connection still accounts for the
bytes the client sent (e.g. the ClientHello); its target fields are empty.

One record per accepted client connection. Records are written when the
connection ends, on every termination path that runs the connection cleanup
(normal close, error, idle-timeout kill, ACL denial, upstream connect
failure). Best-effort on premature termination: if the process is killed
hard, records for still-open connections are lost. On graceful shutdown the
writer drains its queue and flushes.

## Writer architecture

New module `src/accounting.rs`, following the global-module style of
`active_tracker`:

- Connection tasks build a `CdrRecord` and send it over an unbounded mpsc
  channel. The send is non-blocking; if the channel is closed or accounting
  is disabled, the record is dropped (with a warning for the closed case).
  Accounting must never stall proxying.
- A single writer task owns the active file. Writes are therefore serialized
  without any lock on the data path.
- The record's dimensional fields (SNI, target host/endpoint, status) are
  threaded through the connection handler in `runner.rs`; byte counts and
  start time come from the same accumulation already done for
  `active_tracker` (counting bytes as read from each leg).

## Rotation (logrotate-style numbered files)

`.1` is the newest rotated file. After each write, if the active file size
≥ `rotate_size` and no compression is in flight:

1. Delete any file with index > `max_keep`.
2. Shift `cdr.log.N → cdr.log.N+1` from highest index down. Compressed
   files keep their extension when shifted (`.4.zst → .5.zst`).
3. Rename `cdr.log → cdr.log.1` and open a fresh active file (header row
   written).
4. If the file that crossed the `compress_after` boundary (now at index
   `compress_after + 1`) is uncompressed and compression is enabled,
   compress it on a background task (in-process via the `zstd` / `flate2`
   crates — never an external command), producing `.zst`/`.gz` and deleting
   the raw file on success.

While any compression task is in flight, rotation is barred: the writer
keeps appending to the active file even past `rotate_size`. Overflow is
accepted as the best available behavior.

Already-compressed files are never decompressed or recompressed — if
`compress_after` is later raised, or the codec is switched (old `.gz` files
under a `zstd` config), existing compressed files simply keep shifting with
their existing extension until pruned.

## Startup scan

When accounting starts, before serving:

- Prune rotated files with index > `max_keep`.
- If compression is enabled, find every uncompressed `cdr.log.N` with
  `N > compress_after` and compress it (background, same in-flight rule).
- Append to an existing active file rather than truncating it.

## Error handling

- Writer I/O errors (disk full, permission): log the error, drop the
  record, keep trying on subsequent records. Proxying is never affected.
- Compression failure: log, leave the raw file in place (it remains
  eligible for compression on next startup/rotation).
- Invalid `rotate_size` / missing `log_file` when enabled: config load
  error at startup.

## Testing

Unit tests (existing style, `#[cfg(test)]` + `tempfile`):

- `rotate_size` parsing: bytes, KiB/MiB/GiB, invalid values.
- CSV quoting and field validation (commas, quotes, newlines, control
  characters).
- Rotation trigger, shift/rename order, header row in new file.
- `max_keep` pruning, including mixed compressed/uncompressed names.
- `compress_after` boundary: correct file compressed, compressed files
  shifted untouched.
- Rotation barred while compression in flight (writer keeps appending).
- Startup scan: compresses eligible files, never touches compressed ones.
- Config round-trip / defaults / disabled-when-absent, matching the tests
  in `src/config.rs`.

## Out of scope

- Interim records for long-lived connections (end-of-connection only, by
  decision).
- Aggregation — consumers group the CSV themselves.
- Per-listener accounting config (single global block).
- Hot-reload of the accounting block (applies on process restart along with
  the rest of the config, unless existing reload machinery makes it free).
