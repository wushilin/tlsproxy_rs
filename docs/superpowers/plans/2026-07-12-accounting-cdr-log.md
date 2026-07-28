# Accounting CDR Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-connection CSV usage records (CDR) written to a size-rotated, optionally compressed log file, per the approved spec `docs/superpowers/specs/2026-07-12-accounting-cdr-log-design.md`.

**Architecture:** A new `src/accounting.rs` module owns the CSV format, a synchronous `RotatingWriter` (logrotate-style numbered rotation with a compression gate), and a global mpsc-fed writer task. `src/active_tracker.rs` is extended to carry SNI/target/status per connection; `src/runner.rs` workers fill those in as they learn them, and the connection cleanup in `handle_new_socket` builds and submits the record. `main.rs` initializes accounting from a new optional `accounting` config block.

**Tech Stack:** Rust (edition 2021, rust-version 1.85), tokio, serde_yaml_ng, `flate2` (gzip), `zstd`, `time` (RFC3339 formatting), `tempfile` (dev).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-12-accounting-cdr-log-design.md` — consult it for any ambiguity.
- Config defaults (exact values): `enabled: false`, `rotate_size: "100MiB"`, `max_keep: 10`, `compress_after: 3`, `compression: zstd`. `log_file` is required when `enabled: true` (config load error otherwise). `max_keep` must be ≥ 1.
- CSV column order (exact): `listener_type,connection_id,listener_name,sni,target_host,target_endpoint,remote_address,status,uploaded_bytes,downloaded_bytes,connection_start,connection_end`.
- Enum strings (exact): listener_type `TLS_PASSTHROUGH` | `TLS_TERMINATE` | `PORT_FORWARD`; status `OK` | `DENIED` | `CONNECT_FAILED`; compression `zstd` | `gzip` | `none` (lowercase in YAML).
- Byte semantics: `uploaded_bytes` = bytes received FROM the client; `downloaded_bytes` = bytes received FROM upstream; counted when read, not when relayed.
- Rotation: `.1` is newest; compressed files keep their extension when shifted; rotation is barred while compression is in flight (writer appends past the limit); compressed files are NEVER decompressed or recompressed.
- Accounting must never stall or fail proxying: submission is non-blocking, writer errors are logged and the record dropped.
- Run `cargo test` after each task; all tests (existing and new) must pass before committing.
- Commit messages end with: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

---

### Task 1: `accounting` config block

**Files:**
- Modify: `Cargo.toml` (time features)
- Modify: `src/config.rs`
- Test: `src/config.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `config::AccountingConfig { enabled: bool, log_file: String, rotate_size: String, max_keep: usize, compress_after: usize, compression: Compression }` with methods `rotate_size_bytes(&self) -> Result<u64, String>` and `validate(&self) -> Result<(), String>`; `config::Compression` enum (`Zstd` default, `Gzip`, `None`) with `extension(&self) -> Option<&'static str>` (`"zst"` / `"gz"` / `None`); `config::parse_size(s: &str) -> Result<u64, String>`; new field `Config.accounting: Option<AccountingConfig>`. `Config::load_string` / `Config::load_file` validate the block and fail on bad config.

- [ ] **Step 1: Update `Cargo.toml` time features** (needed by Task 2; folded here so Cargo.toml changes once per dependency group)

Change the `time` line in `[dependencies]`:

```toml
time = { version = "0.3", features = ["formatting", "macros"] }
```

- [ ] **Step 2: Write the failing tests**

Append inside `mod tests` in `src/config.rs`:

```rust
    #[test]
    fn parse_size_accepts_bytes_and_binary_suffixes() {
        use super::parse_size;
        assert_eq!(parse_size("123").unwrap(), 123);
        assert_eq!(parse_size("2KiB").unwrap(), 2 * 1024);
        assert_eq!(parse_size("18MiB").unwrap(), 18 * 1024 * 1024);
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 10 MiB ").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("18mib").unwrap(), 18 * 1024 * 1024);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10MB").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn accounting_block_parses_with_defaults() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {}
dns: {}
accounting:
  enabled: true
  log_file: cdr.log
"#;
        let config = Config::load_string(yaml).expect("accounting configuration should parse");
        let accounting = config.accounting.expect("accounting block should be present");
        assert!(accounting.enabled);
        assert_eq!(accounting.log_file, "cdr.log");
        assert_eq!(accounting.rotate_size, "100MiB");
        assert_eq!(accounting.rotate_size_bytes().unwrap(), 100 * 1024 * 1024);
        assert_eq!(accounting.max_keep, 10);
        assert_eq!(accounting.compress_after, 3);
        assert_eq!(accounting.compression, super::Compression::Zstd);
    }

    #[test]
    fn accounting_absent_is_none_and_old_configs_still_load() {
        let yaml = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {}
dns: {}
"#;
        let config = Config::load_string(yaml).expect("config without accounting should parse");
        assert!(config.accounting.is_none());
    }

    #[test]
    fn accounting_enabled_requires_log_file_and_valid_rotate_size() {
        let base = |extra: &str| {
            format!(
                r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {{}}
dns: {{}}
accounting:
  enabled: true
{extra}
"#
            )
        };
        assert!(Config::load_string(&base("")).is_err(), "missing log_file must fail");
        assert!(
            Config::load_string(&base("  log_file: cdr.log\n  rotate_size: banana")).is_err(),
            "invalid rotate_size must fail"
        );
        assert!(
            Config::load_string(&base("  log_file: cdr.log\n  max_keep: 0")).is_err(),
            "max_keep 0 must fail"
        );
        assert!(
            Config::load_string(&base("  log_file: cdr.log\n  compression: gzip")).is_ok(),
            "gzip compression must parse"
        );
        // disabled block with nonsense values loads fine (never validated)
        let disabled = r#"
listeners:
  web:
    bind: 127.0.0.1:1443
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
options: {}
dns: {}
accounting:
  enabled: false
"#;
        assert!(Config::load_string(disabled).is_ok());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib config::tests -- --nocapture 2>&1 | tail -20`
Expected: compile error (`parse_size`, `AccountingConfig` not defined).

- [ ] **Step 4: Implement**

In `src/config.rs`, add the `accounting` field to `Config` (after `admin_server`):

```rust
    pub admin_server: Option<AdminServerConfig>,
    /// Accounting CDR log. Absent or `enabled: false` disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<AccountingConfig>,
```

Add below the `AdminServerConfig` impl block:

```rust
/// Per-connection CDR accounting log with size-based rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub log_file: String,
    #[serde(default = "default_rotate_size")]
    pub rotate_size: String,
    #[serde(default = "default_max_keep")]
    pub max_keep: usize,
    /// Rotated indexes 1..=compress_after stay uncompressed.
    #[serde(default = "default_compress_after")]
    pub compress_after: usize,
    #[serde(default)]
    pub compression: Compression,
}

fn default_rotate_size() -> String {
    "100MiB".into()
}

fn default_max_keep() -> usize {
    10
}

fn default_compress_after() -> usize {
    3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    #[default]
    Zstd,
    Gzip,
    None,
}

impl Compression {
    pub fn extension(&self) -> Option<&'static str> {
        match self {
            Compression::Zstd => Some("zst"),
            Compression::Gzip => Some("gz"),
            Compression::None => None,
        }
    }
}

impl AccountingConfig {
    pub fn rotate_size_bytes(&self) -> Result<u64, String> {
        parse_size(&self.rotate_size)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.log_file.trim().is_empty() {
            return Err("accounting.log_file is required when accounting.enabled is true".into());
        }
        if self.max_keep == 0 {
            return Err("accounting.max_keep must be at least 1".into());
        }
        self.rotate_size_bytes()?;
        Ok(())
    }
}

/// Parses `123` (bytes), `2KiB`, `18MiB`, or `1GiB` (case-insensitive) into bytes.
pub fn parse_size(size: &str) -> Result<u64, String> {
    let lower = size.trim().to_ascii_lowercase();
    let (digits, multiplier) = if let Some(digits) = lower.strip_suffix("kib") {
        (digits, 1024u64)
    } else if let Some(digits) = lower.strip_suffix("mib") {
        (digits, 1024 * 1024)
    } else if let Some(digits) = lower.strip_suffix("gib") {
        (digits, 1024 * 1024 * 1024)
    } else {
        (lower.as_str(), 1)
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid size `{size}`; use bytes or a KiB/MiB/GiB suffix"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size `{size}` overflows"))
}
```

Wire validation into loading — replace both `Config` load methods:

```rust
    pub async fn load_file<P: AsRef<Path>>(filename: P) -> Result<Config, Box<dyn Error>> {
        let content = fs::read_to_string(filename).await?;
        Self::load_string(&content)
    }

    pub fn load_string(content: &str) -> Result<Config, Box<dyn Error>> {
        let config: Config = serde_yaml_ng::from_str(content)?;
        if let Some(accounting) = &config.accounting {
            accounting.validate()?;
        }
        return Ok(config);
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib config:: 2>&1 | tail -5`
Expected: all config tests PASS (new and pre-existing).

- [ ] **Step 6: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add Cargo.toml Cargo.lock src/config.rs
git commit -m "feat: add accounting config block with size parsing and validation

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: CDR record types and CSV formatting

**Files:**
- Create: `src/accounting.rs`
- Modify: `src/main.rs` (module declaration only)
- Test: `src/accounting.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::config::ListenerMode`.
- Produces: `accounting::ListenerType` (`from_mode(ListenerMode) -> Self`, `as_str() -> &'static str`); `accounting::ConnStatus` (`Ok`, `Denied`, `ConnectFailed` — `#[default]` is `ConnectFailed`; `as_str()`); `accounting::CdrRecord` (all fields `pub`: `listener_type: ListenerType, connection_id: String, listener_name: String, sni: String, target_host: String, target_endpoint: String, remote_address: String, status: ConnStatus, uploaded_bytes: u64, downloaded_bytes: u64, start_unix_ms: u128, end_unix_ms: u128`) with `to_csv_line(&self) -> String`; `accounting::CSV_HEADER: &str`.

- [ ] **Step 1: Declare the module**

In `src/main.rs`, add to the `pub mod` list (alphabetical, before `pub mod admin_server;`):

```rust
pub mod accounting;
```

- [ ] **Step 2: Write the failing tests**

Create `src/accounting.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ListenerMode;

    #[test]
    fn csv_field_quotes_and_sanitizes() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("has,comma"), "\"has,comma\"");
        assert_eq!(csv_field("has\"quote"), "\"has\"\"quote\"");
        assert_eq!(csv_field("line\r\nbreak"), "linebreak");
        assert_eq!(csv_field("tab\tchar"), "tabchar");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn timestamps_format_as_rfc3339_utc_with_milliseconds() {
        assert_eq!(format_rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_ms(86_400_000), "1970-01-02T00:00:00.000Z");
        assert_eq!(format_rfc3339_ms(1_000_000_000_123), "2001-09-09T01:46:40.123Z");
    }

    #[test]
    fn listener_type_and_status_strings_match_spec() {
        assert_eq!(ListenerType::from_mode(ListenerMode::Passthrough).as_str(), "TLS_PASSTHROUGH");
        assert_eq!(ListenerType::from_mode(ListenerMode::Terminate).as_str(), "TLS_TERMINATE");
        assert_eq!(ListenerType::from_mode(ListenerMode::Forward).as_str(), "PORT_FORWARD");
        assert_eq!(ConnStatus::Ok.as_str(), "OK");
        assert_eq!(ConnStatus::Denied.as_str(), "DENIED");
        assert_eq!(ConnStatus::ConnectFailed.as_str(), "CONNECT_FAILED");
        assert_eq!(ConnStatus::default(), ConnStatus::ConnectFailed);
    }

    #[test]
    fn record_renders_one_csv_line_matching_header_column_count() {
        let record = CdrRecord {
            listener_type: ListenerType::TlsPassthrough,
            connection_id: "jdsaffwefaef(45)".into(),
            listener_name: "HTTPS".into(),
            sni: "asdf.dev.com".into(),
            target_host: "asdf.dev.com".into(),
            target_endpoint: "32.11.23.4:443".into(),
            remote_address: "10.0.0.9:51234".into(),
            status: ConnStatus::Ok,
            uploaded_bytes: 452_333,
            downloaded_bytes: 2_323_123,
            start_unix_ms: 0,
            end_unix_ms: 1_000,
        };
        let line = record.to_csv_line();
        assert_eq!(
            line,
            "TLS_PASSTHROUGH,jdsaffwefaef(45),HTTPS,asdf.dev.com,asdf.dev.com,32.11.23.4:443,10.0.0.9:51234,OK,452333,2323123,1970-01-01T00:00:00.000Z,1970-01-01T00:00:01.000Z"
        );
        assert_eq!(
            line.split(',').count(),
            CSV_HEADER.split(',').count(),
            "line and header column counts must match"
        );
    }

    #[test]
    fn record_with_commas_in_fields_stays_one_logical_row() {
        let record = CdrRecord {
            listener_type: ListenerType::PortForward,
            connection_id: "id(1)".into(),
            listener_name: "a,b".into(),
            sni: String::new(),
            target_host: String::new(),
            target_endpoint: String::new(),
            remote_address: "1.2.3.4:1".into(),
            status: ConnStatus::Denied,
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            start_unix_ms: 0,
            end_unix_ms: 0,
        };
        let line = record.to_csv_line();
        assert!(line.contains("\"a,b\""));
        assert!(!line.contains('\n'));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib accounting 2>&1 | tail -10`
Expected: compile error (`csv_field` etc. not defined).

- [ ] **Step 4: Implement**

Add above the test module in `src/accounting.rs`:

```rust
use crate::config::ListenerMode;

pub const CSV_HEADER: &str = "listener_type,connection_id,listener_name,sni,target_host,target_endpoint,remote_address,status,uploaded_bytes,downloaded_bytes,connection_start,connection_end";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerType {
    TlsPassthrough,
    TlsTerminate,
    PortForward,
}

impl ListenerType {
    pub fn from_mode(mode: ListenerMode) -> Self {
        match mode {
            ListenerMode::Passthrough => ListenerType::TlsPassthrough,
            ListenerMode::Terminate => ListenerType::TlsTerminate,
            ListenerMode::Forward => ListenerType::PortForward,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ListenerType::TlsPassthrough => "TLS_PASSTHROUGH",
            ListenerType::TlsTerminate => "TLS_TERMINATE",
            ListenerType::PortForward => "PORT_FORWARD",
        }
    }
}

/// Connection outcome for the CDR. Defaults to `ConnectFailed`; workers
/// upgrade it to `Ok` once the upstream socket is connected, or mark
/// `Denied` on ACL rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnStatus {
    Ok,
    Denied,
    #[default]
    ConnectFailed,
}

impl ConnStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnStatus::Ok => "OK",
            ConnStatus::Denied => "DENIED",
            ConnStatus::ConnectFailed => "CONNECT_FAILED",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CdrRecord {
    pub listener_type: ListenerType,
    pub connection_id: String,
    pub listener_name: String,
    pub sni: String,
    pub target_host: String,
    pub target_endpoint: String,
    pub remote_address: String,
    pub status: ConnStatus,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub start_unix_ms: u128,
    pub end_unix_ms: u128,
}

impl CdrRecord {
    pub fn to_csv_line(&self) -> String {
        [
            csv_field(self.listener_type.as_str()),
            csv_field(&self.connection_id),
            csv_field(&self.listener_name),
            csv_field(&self.sni),
            csv_field(&self.target_host),
            csv_field(&self.target_endpoint),
            csv_field(&self.remote_address),
            csv_field(self.status.as_str()),
            self.uploaded_bytes.to_string(),
            self.downloaded_bytes.to_string(),
            format_rfc3339_ms(self.start_unix_ms),
            format_rfc3339_ms(self.end_unix_ms),
        ]
        .join(",")
    }
}

/// Strips control characters (so a field can never break the row structure),
/// then applies RFC 4180 quoting when the value contains a comma or quote.
fn csv_field(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    if cleaned.contains(',') || cleaned.contains('"') {
        format!("\"{}\"", cleaned.replace('"', "\"\""))
    } else {
        cleaned
    }
}

fn format_rfc3339_ms(unix_ms: u128) -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::OffsetDateTime::from_unix_timestamp_nanos((unix_ms as i128) * 1_000_000)
        .ok()
        .and_then(|timestamp| timestamp.format(&format).ok())
        .unwrap_or_default()
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib accounting 2>&1 | tail -5`
Expected: 5 tests PASS.

- [ ] **Step 6: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add src/accounting.rs src/main.rs
git commit -m "feat: add CDR record type with RFC 4180 CSV formatting

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: RotatingWriter — numbered rotation, shift, prune

**Files:**
- Modify: `src/accounting.rs`
- Test: `src/accounting.rs`

**Interfaces:**
- Consumes: `CSV_HEADER` (Task 2).
- Produces: `accounting::RotatingWriter` — `open(log_path: &Path, rotate_size: u64, max_keep: usize) -> io::Result<Self>` (append mode, header written iff file is empty), `write_record(&mut self, line: &str) -> io::Result<bool>` (returns `true` if a rotation happened), `compress_gate(&self) -> Arc<AtomicBool>` (rotation is skipped while the gate is `true`). Private helpers reused by Task 4: `struct RotatedFile { index: usize, suffix: Option<String>, path: PathBuf }`, `fn rotated_files(log_path: &Path) -> Vec<RotatedFile>`, `fn indexed_path(log_path: &Path, index: usize, suffix: Option<&str>) -> PathBuf`, `fn prune(log_path: &Path, keep_upto: usize) -> io::Result<()>`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/accounting.rs` (add `use std::fs; use std::path::Path; use tempfile::tempdir;` to the test module's imports):

```rust
    fn writer_in(dir: &Path, rotate_size: u64, max_keep: usize) -> (std::path::PathBuf, RotatingWriter) {
        let log_path = dir.join("cdr.log");
        let writer = RotatingWriter::open(&log_path, rotate_size, max_keep).unwrap();
        (log_path, writer)
    }

    #[test]
    fn open_writes_header_once_and_appends_on_reopen() {
        let dir = tempdir().unwrap();
        let (log_path, mut writer) = writer_in(dir.path(), 1_000_000, 3);
        writer.write_record("row1").unwrap();
        drop(writer);
        let mut writer = RotatingWriter::open(&log_path, 1_000_000, 3).unwrap();
        writer.write_record("row2").unwrap();
        let content = fs::read_to_string(&log_path).unwrap();
        assert_eq!(content, format!("{CSV_HEADER}\nrow1\nrow2\n"));
    }

    #[test]
    fn write_over_rotate_size_rotates_to_dot_one_with_fresh_active_file() {
        let dir = tempdir().unwrap();
        // tiny limit: the first record already crosses it
        let (log_path, mut writer) = writer_in(dir.path(), 10, 3);
        assert!(writer.write_record("a-full-row").unwrap(), "should rotate");
        let rotated = fs::read_to_string(dir.path().join("cdr.log.1")).unwrap();
        assert_eq!(rotated, format!("{CSV_HEADER}\na-full-row\n"));
        let active = fs::read_to_string(&log_path).unwrap();
        assert_eq!(active, format!("{CSV_HEADER}\n"), "new active file has only the header");
    }

    #[test]
    fn rotation_shifts_existing_files_up_and_preserves_compression_suffix() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("cdr.log.1"), "old-1").unwrap();
        fs::write(dir.path().join("cdr.log.2.zst"), "old-2-compressed").unwrap();
        let (_log_path, mut writer) = writer_in(dir.path(), 10, 5);
        assert!(writer.write_record("new-row").unwrap());
        assert_eq!(fs::read_to_string(dir.path().join("cdr.log.2")).unwrap(), "old-1");
        assert_eq!(
            fs::read_to_string(dir.path().join("cdr.log.3.zst")).unwrap(),
            "old-2-compressed"
        );
        assert!(dir.path().join("cdr.log.1").exists(), "newest rotation takes index 1");
    }

    #[test]
    fn rotation_prunes_files_beyond_max_keep() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("cdr.log.1"), "keep-as-2").unwrap();
        fs::write(dir.path().join("cdr.log.2.zst"), "dies").unwrap();
        fs::write(dir.path().join("cdr.log.7"), "stale-dies").unwrap();
        let (_log_path, mut writer) = writer_in(dir.path(), 10, 2);
        assert!(writer.write_record("new-row").unwrap());
        assert!(dir.path().join("cdr.log.1").exists());
        assert_eq!(fs::read_to_string(dir.path().join("cdr.log.2")).unwrap(), "keep-as-2");
        assert!(!dir.path().join("cdr.log.2.zst").exists(), "index 2 would shift past max_keep");
        assert!(!dir.path().join("cdr.log.3.zst").exists());
        assert!(!dir.path().join("cdr.log.7").exists());
        assert!(!dir.path().join("cdr.log.8").exists());
    }

    #[test]
    fn rotation_is_barred_while_compression_gate_is_held() {
        let dir = tempdir().unwrap();
        let (log_path, mut writer) = writer_in(dir.path(), 10, 3);
        let gate = writer.compress_gate();
        gate.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!writer.write_record("row-a").unwrap(), "gate held: no rotation");
        assert!(!writer.write_record("row-b").unwrap(), "still barred, keeps appending");
        assert!(!dir.path().join("cdr.log.1").exists());
        gate.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(writer.write_record("row-c").unwrap(), "gate released: rotates");
        let rotated = fs::read_to_string(dir.path().join("cdr.log.1")).unwrap();
        assert_eq!(rotated, format!("{CSV_HEADER}\nrow-a\nrow-b\nrow-c\n"));
        assert_eq!(fs::read_to_string(&log_path).unwrap(), format!("{CSV_HEADER}\n"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib accounting 2>&1 | tail -10`
Expected: compile error (`RotatingWriter` not defined).

- [ ] **Step 3: Implement**

Add to `src/accounting.rs` (extend the `use` block at the top):

```rust
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
```

```rust
/// Appends CSV lines to `log_path`, rotating logrotate-style: on rotation
/// the active file becomes `.1` and every existing `.N` shifts to `.N+1`
/// (keeping any compression suffix). Rotation is skipped entirely while the
/// compression gate is held; the writer then appends past `rotate_size`.
pub struct RotatingWriter {
    log_path: PathBuf,
    rotate_size: u64,
    max_keep: usize,
    file: File,
    current_size: u64,
    compress_in_flight: Arc<AtomicBool>,
}

struct RotatedFile {
    index: usize,
    suffix: Option<String>,
    path: PathBuf,
}

impl RotatingWriter {
    pub fn open(log_path: &Path, rotate_size: u64, max_keep: usize) -> io::Result<Self> {
        let (file, current_size) = open_active(log_path)?;
        Ok(Self {
            log_path: log_path.to_path_buf(),
            rotate_size,
            max_keep,
            file,
            current_size,
            compress_in_flight: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The rotation gate: while `true`, a compression job is running and
    /// rotation is barred.
    pub fn compress_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.compress_in_flight)
    }

    /// Appends one line and rotates if the size limit is reached. Returns
    /// `true` when a rotation happened.
    pub fn write_record(&mut self, line: &str) -> io::Result<bool> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.current_size += line.len() as u64 + 1;
        if self.current_size >= self.rotate_size && !self.compress_in_flight.load(Ordering::SeqCst)
        {
            self.rotate()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // After the shift every survivor's index grows by one, so anything
        // already at max_keep or above must go first.
        prune(&self.log_path, self.max_keep.saturating_sub(1))?;
        let mut files = rotated_files(&self.log_path);
        files.sort_by(|a, b| b.index.cmp(&a.index));
        for file in files {
            let to = indexed_path(&self.log_path, file.index + 1, file.suffix.as_deref());
            fs::rename(&file.path, &to)?;
        }
        fs::rename(&self.log_path, indexed_path(&self.log_path, 1, None))?;
        let (file, current_size) = open_active(&self.log_path)?;
        self.file = file;
        self.current_size = current_size;
        Ok(())
    }
}

fn open_active(log_path: &Path) -> io::Result<(File, u64)> {
    let mut file = OpenOptions::new().create(true).append(true).open(log_path)?;
    let mut size = file.metadata()?.len();
    if size == 0 {
        let header = format!("{CSV_HEADER}\n");
        file.write_all(header.as_bytes())?;
        file.flush()?;
        size = header.len() as u64;
    }
    Ok((file, size))
}

/// `<log>.N` or `<log>.N.<suffix>` files next to the active log.
fn rotated_files(log_path: &Path) -> Vec<RotatedFile> {
    let Some(base) = log_path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let parent = match log_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let prefix = format!("{base}.");
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else { continue };
        let (index_part, suffix) = match rest.split_once('.') {
            Some((index_part, suffix)) => (index_part, Some(suffix.to_string())),
            None => (rest, None),
        };
        let Ok(index) = index_part.parse::<usize>() else { continue };
        result.push(RotatedFile {
            index,
            suffix,
            path: entry.path(),
        });
    }
    result
}

fn indexed_path(log_path: &Path, index: usize, suffix: Option<&str>) -> PathBuf {
    let mut name = log_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}"));
    if let Some(suffix) = suffix {
        name.push(format!(".{suffix}"));
    }
    log_path.with_file_name(name)
}

/// Deletes every rotated file with index > `keep_upto`.
fn prune(log_path: &Path, keep_upto: usize) -> io::Result<()> {
    for file in rotated_files(log_path) {
        if file.index > keep_upto {
            fs::remove_file(&file.path)?;
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib accounting 2>&1 | tail -5`
Expected: all accounting tests PASS.

- [ ] **Step 5: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add src/accounting.rs
git commit -m "feat: add rotating CDR writer with numbered rotation and pruning

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: In-process compression and startup scan helpers

**Files:**
- Modify: `Cargo.toml` (add flate2, zstd)
- Modify: `src/accounting.rs`
- Test: `src/accounting.rs`

**Interfaces:**
- Consumes: `RotatedFile`, `rotated_files`, `prune` (Task 3); `config::Compression` (Task 1).
- Produces: `accounting::compress_file(path: &Path, compression: Compression) -> io::Result<()>` (creates `<path>.zst`/`<path>.gz`, deletes the raw file; no-op for `Compression::None`); `accounting::compression_candidates(log_path: &Path, compress_after: usize, compression: Compression) -> Vec<PathBuf>` (uncompressed rotated files with index > `compress_after`, ascending; empty for `Compression::None`).

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` `[dependencies]`:

```toml
flate2 = "1"
zstd = "0.13"
```

- [ ] **Step 2: Write the failing tests**

Append inside `mod tests` in `src/accounting.rs` (add `use crate::config::Compression;` and `use std::io::Read;` to the test imports):

```rust
    #[test]
    fn compress_file_zstd_roundtrips_and_removes_raw() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("cdr.log.4");
        fs::write(&raw, "some,cdr,content\n").unwrap();
        compress_file(&raw, Compression::Zstd).unwrap();
        assert!(!raw.exists());
        let compressed = dir.path().join("cdr.log.4.zst");
        let decoded = zstd::stream::decode_all(fs::File::open(&compressed).unwrap()).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "some,cdr,content\n");
    }

    #[test]
    fn compress_file_gzip_roundtrips_and_removes_raw() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("cdr.log.4");
        fs::write(&raw, "gzip,cdr,content\n").unwrap();
        compress_file(&raw, Compression::Gzip).unwrap();
        assert!(!raw.exists());
        let compressed = dir.path().join("cdr.log.4.gz");
        let mut decoder = flate2::read::GzDecoder::new(fs::File::open(&compressed).unwrap());
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded, "gzip,cdr,content\n");
    }

    #[test]
    fn compress_file_none_is_a_noop() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("cdr.log.4");
        fs::write(&raw, "content").unwrap();
        compress_file(&raw, Compression::None).unwrap();
        assert!(raw.exists());
    }

    #[test]
    fn compression_candidates_picks_uncompressed_past_boundary_only() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("cdr.log");
        fs::write(dir.path().join("cdr.log.1"), "x").unwrap();
        fs::write(dir.path().join("cdr.log.3"), "x").unwrap();
        fs::write(dir.path().join("cdr.log.4"), "x").unwrap();
        fs::write(dir.path().join("cdr.log.5.zst"), "x").unwrap();
        fs::write(dir.path().join("cdr.log.6.gz"), "x").unwrap();
        fs::write(dir.path().join("cdr.log.7"), "x").unwrap();
        let candidates = compression_candidates(&log_path, 3, Compression::Zstd);
        assert_eq!(
            candidates,
            vec![dir.path().join("cdr.log.4"), dir.path().join("cdr.log.7")],
            "only uncompressed files past the boundary, ascending; compressed never touched"
        );
        assert!(compression_candidates(&log_path, 3, Compression::None).is_empty());
        assert!(compression_candidates(&log_path, 10, Compression::Zstd).is_empty());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib accounting 2>&1 | tail -10`
Expected: compile error (`compress_file` not defined).

- [ ] **Step 4: Implement**

Add to `src/accounting.rs`:

```rust
/// Compresses `path` to `<path>.zst` / `<path>.gz` and removes the raw file.
/// Overwrites a partial output left by an earlier crash. No-op for `None`.
pub fn compress_file(path: &Path, compression: Compression) -> io::Result<()> {
    let Some(extension) = compression.extension() else {
        return Ok(());
    };
    let mut output_name = path.file_name().unwrap_or_default().to_os_string();
    output_name.push(format!(".{extension}"));
    let output_path = path.with_file_name(output_name);
    let input = io::BufReader::new(File::open(path)?);
    match compression {
        Compression::Gzip => {
            let mut encoder =
                flate2::write::GzEncoder::new(File::create(&output_path)?, flate2::Compression::default());
            let mut input = input;
            io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
        }
        Compression::Zstd => {
            zstd::stream::copy_encode(input, File::create(&output_path)?, 0)?;
        }
        Compression::None => unreachable!("extension() returned Some"),
    }
    fs::remove_file(path)?;
    Ok(())
}

/// Uncompressed rotated files that sit past the `compress_after` boundary,
/// in ascending index order. Compressed files are never revisited.
pub fn compression_candidates(
    log_path: &Path,
    compress_after: usize,
    compression: Compression,
) -> Vec<PathBuf> {
    if compression == Compression::None {
        return Vec::new();
    }
    let mut candidates: Vec<RotatedFile> = rotated_files(log_path)
        .into_iter()
        .filter(|file| file.suffix.is_none() && file.index > compress_after)
        .collect();
    candidates.sort_by_key(|file| file.index);
    candidates.into_iter().map(|file| file.path).collect()
}
```

Also extend the top-of-file imports with `use crate::config::Compression;`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib accounting 2>&1 | tail -5`
Expected: all accounting tests PASS.

- [ ] **Step 6: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add Cargo.toml Cargo.lock src/accounting.rs
git commit -m "feat: add in-process gzip/zstd compression for rotated CDR files

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Global writer task — init, submit, shutdown

**Files:**
- Modify: `src/accounting.rs`
- Test: `src/accounting.rs`

**Interfaces:**
- Consumes: `RotatingWriter`, `compress_file`, `compression_candidates`, `prune` (Tasks 3–4); `config::AccountingConfig` (Task 1).
- Produces: `accounting::init(cfg: &AccountingConfig) -> anyhow::Result<()>` (async; no-op when `cfg.enabled` is false; errors if called twice); `accounting::enabled() -> bool`; `accounting::submit(record: CdrRecord)` (non-blocking `try_send` into a bounded channel of `QUEUE_CAPACITY = 100_000` records — drops with a warning when full/closed, silently no-op when not initialized); `accounting::shutdown()` (async; drains the queue and flushes; safe when never initialized).

- [ ] **Step 1: Write the failing test**

This is the ONLY test in the suite allowed to touch the global `SENDER` (it is a process-wide `OnceLock`; a second init in another test would fail). Append inside `mod tests`:

```rust
    #[tokio::test]
    async fn global_init_submit_shutdown_writes_records() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("cdr.log");
        let cfg = crate::config::AccountingConfig {
            enabled: true,
            log_file: log_path.to_string_lossy().into_owned(),
            rotate_size: "1MiB".into(),
            max_keep: 3,
            compress_after: 3,
            compression: Compression::None,
        };
        assert!(!enabled());
        init(&cfg).await.unwrap();
        assert!(enabled());
        let record = CdrRecord {
            listener_type: ListenerType::TlsTerminate,
            connection_id: "abc(7)".into(),
            listener_name: "HTTPS".into(),
            sni: "example.com".into(),
            target_host: "example.com".into(),
            target_endpoint: "1.2.3.4:443".into(),
            remote_address: "5.6.7.8:50000".into(),
            status: ConnStatus::Ok,
            uploaded_bytes: 10,
            downloaded_bytes: 20,
            start_unix_ms: 0,
            end_unix_ms: 1,
        };
        submit(record.clone());
        submit(record);
        shutdown().await;
        let content = fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two records");
        assert_eq!(lines[0], CSV_HEADER);
        assert!(lines[1].starts_with("TLS_TERMINATE,abc(7),HTTPS,example.com,"));
        assert_eq!(lines[1], lines[2]);
        // double init must fail
        assert!(init(&cfg).await.is_err());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib accounting::tests::global_init 2>&1 | tail -10`
Expected: compile error (`init` not defined).

- [ ] **Step 3: Implement**

Add to `src/accounting.rs` (extend imports with `use crate::config::AccountingConfig; use anyhow::anyhow; use log::{error, warn}; use std::sync::OnceLock; use tokio::sync::{mpsc, oneshot};`):

```rust
enum Msg {
    Record(CdrRecord),
    Flush(oneshot::Sender<()>),
}

/// Bounded so a stalled disk caps memory instead of growing without limit;
/// tokio allocates the buffer lazily, so an idle queue costs nothing.
const QUEUE_CAPACITY: usize = 100_000;

static SENDER: OnceLock<mpsc::Sender<Msg>> = OnceLock::new();

pub fn enabled() -> bool {
    SENDER.get().is_some()
}

/// Non-blocking; drops the record (with a warning) if the queue is full or
/// the writer is gone, and silently when accounting was never enabled.
pub fn submit(record: CdrRecord) {
    if let Some(sender) = SENDER.get() {
        if sender.try_send(Msg::Record(record)).is_err() {
            warn!("accounting queue full or writer gone; dropping CDR record");
        }
    }
}

/// Starts the writer task. Prunes and compresses leftovers from previous
/// runs first (never decompressing existing archives), then appends to the
/// existing active file.
pub async fn init(cfg: &AccountingConfig) -> anyhow::Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    cfg.validate().map_err(|cause| anyhow!(cause))?;
    let rotate_size = cfg.rotate_size_bytes().map_err(|cause| anyhow!(cause))?;
    let log_path = PathBuf::from(&cfg.log_file);
    let mut writer = RotatingWriter::open(&log_path, rotate_size, cfg.max_keep)?;
    let compress_after = cfg.compress_after;
    let compression = cfg.compression;
    let gate = writer.compress_gate();
    // startup scan
    prune(&log_path, cfg.max_keep)?;
    let leftovers = compression_candidates(&log_path, compress_after, compression);
    if !leftovers.is_empty() {
        spawn_compression(leftovers, compression, Arc::clone(&gate));
    }
    let (tx, mut rx) = mpsc::channel(QUEUE_CAPACITY);
    SENDER
        .set(tx)
        .map_err(|_| anyhow!("accounting already initialized"))?;
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Record(record) => match writer.write_record(&record.to_csv_line()) {
                    Ok(true) => {
                        let candidates =
                            compression_candidates(&log_path, compress_after, compression);
                        if !candidates.is_empty() {
                            spawn_compression(candidates, compression, writer.compress_gate());
                        }
                    }
                    Ok(false) => {}
                    Err(cause) => {
                        error!("accounting: failed to write CDR record: {cause}");
                    }
                },
                Msg::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
        }
    });
    Ok(())
}

/// Waits until every record submitted so far has been written and flushed.
/// Uses the awaiting `send` (not `try_send`) so the flush marker queues even
/// when the channel is momentarily full.
pub async fn shutdown() {
    if let Some(sender) = SENDER.get() {
        let (ack_tx, ack_rx) = oneshot::channel();
        if sender.send(Msg::Flush(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
    }
}

/// At most one compression batch runs at a time; the shared gate bars
/// rotation for its whole duration.
fn spawn_compression(candidates: Vec<PathBuf>, compression: Compression, gate: Arc<AtomicBool>) {
    if gate.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        for path in candidates {
            if let Err(cause) = compress_file(&path, compression) {
                warn!("accounting: failed to compress {}: {cause}", path.display());
            }
        }
        gate.store(false, Ordering::SeqCst);
    });
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib accounting 2>&1 | tail -5`
Expected: all accounting tests PASS.

- [ ] **Step 5: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add src/accounting.rs
git commit -m "feat: add global accounting writer task with startup scan

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Extend active_tracker with CDR dimensions

**Files:**
- Modify: `src/active_tracker.rs`
- Test: `src/active_tracker.rs` (new `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `accounting::ConnStatus` (Task 2).
- Produces: `active_tracker::set_sni(request_id: &RequestId, sni: &str)`, `set_target(request_id: &RequestId, target_host: &str, target_endpoint: &str)`, `set_status(request_id: &RequestId, status: ConnStatus)` (all async, no-op for unknown ids); `active_tracker::remove(request_id: &RequestId) -> Option<ClosedConnection>` where `pub struct ClosedConnection { pub listener: String, pub remote_address: SocketAddr, pub started_at_unix_ms: u128, pub uploaded_bytes: u64, pub downloaded_bytes: u64, pub sni: String, pub target_host: String, pub target_endpoint: String, pub status: ConnStatus }`. The existing `remove` call site compiles unchanged (return value ignored until Task 7).

- [ ] **Step 1: Write the failing tests**

Add at the bottom of `src/active_tracker.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::ConnStatus;

    #[tokio::test]
    async fn remove_returns_accumulated_connection_details() {
        let request_id = RequestId::new();
        let addr: SocketAddr = "10.1.2.3:44444".parse().unwrap();
        put(&request_id, "listener-a", addr).await;
        add_uploaded(&request_id, 100).await;
        add_downloaded(&request_id, 200).await;
        set_sni(&request_id, "example.com").await;
        set_target(&request_id, "example.com", "1.2.3.4:443").await;
        set_status(&request_id, ConnStatus::Ok).await;
        let closed = remove(&request_id).await.expect("connection should be tracked");
        assert_eq!(closed.listener, "listener-a");
        assert_eq!(closed.remote_address, addr);
        assert_eq!(closed.uploaded_bytes, 100);
        assert_eq!(closed.downloaded_bytes, 200);
        assert_eq!(closed.sni, "example.com");
        assert_eq!(closed.target_host, "example.com");
        assert_eq!(closed.target_endpoint, "1.2.3.4:443");
        assert_eq!(closed.status, ConnStatus::Ok);
        assert!(closed.started_at_unix_ms > 0);
        assert!(remove(&request_id).await.is_none(), "second remove returns None");
    }

    #[tokio::test]
    async fn new_connections_default_to_connect_failed_with_empty_dimensions() {
        let request_id = RequestId::new();
        let addr: SocketAddr = "10.1.2.3:44445".parse().unwrap();
        put(&request_id, "listener-b", addr).await;
        let closed = remove(&request_id).await.unwrap();
        assert_eq!(closed.status, ConnStatus::ConnectFailed);
        assert_eq!(closed.sni, "");
        assert_eq!(closed.target_host, "");
        assert_eq!(closed.target_endpoint, "");
    }

    #[tokio::test]
    async fn setters_ignore_unknown_request_ids() {
        let request_id = RequestId::new();
        set_sni(&request_id, "ghost").await;
        set_status(&request_id, ConnStatus::Denied).await;
        assert!(remove(&request_id).await.is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib active_tracker 2>&1 | tail -10`
Expected: compile error (`set_sni` not defined, `remove` returns `()`).

- [ ] **Step 3: Implement**

In `src/active_tracker.rs`, add the import and extend `ActiveConnection`:

```rust
use crate::accounting::ConnStatus;
```

```rust
#[derive(Debug, Clone)]
struct ActiveConnection {
    listener: String,
    remote_address: SocketAddr,
    started_at: Instant,
    started_at_unix_ms: u128,
    uploaded_bytes: u64,
    downloaded_bytes: u64,
    sni: String,
    target_host: String,
    target_endpoint: String,
    status: ConnStatus,
}
```

In `put`, initialize the new fields:

```rust
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            sni: String::new(),
            target_host: String::new(),
            target_endpoint: String::new(),
            status: ConnStatus::default(),
```

Add setters after `add_downloaded`:

```rust
pub async fn set_sni(request_id: &RequestId, sni: &str) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.sni = sni.to_string();
    }
}

pub async fn set_target(request_id: &RequestId, target_host: &str, target_endpoint: &str) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.target_host = target_host.to_string();
        active.target_endpoint = target_endpoint.to_string();
    }
}

pub async fn set_status(request_id: &RequestId, status: ConnStatus) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.status = status;
    }
}
```

Replace `remove` and add `ClosedConnection`:

```rust
/// Final connection details handed to accounting when a connection ends.
#[derive(Debug, Clone)]
pub struct ClosedConnection {
    pub listener: String,
    pub remote_address: SocketAddr,
    pub started_at_unix_ms: u128,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub sni: String,
    pub target_host: String,
    pub target_endpoint: String,
    pub status: ConnStatus,
}

pub async fn remove(request_id: &RequestId) -> Option<ClosedConnection> {
    let mut w = ACTIVE.write().await;
    w.remove(request_id).map(|active| ClosedConnection {
        listener: active.listener,
        remote_address: active.remote_address,
        started_at_unix_ms: active.started_at_unix_ms,
        uploaded_bytes: active.uploaded_bytes,
        downloaded_bytes: active.downloaded_bytes,
        sni: active.sni,
        target_host: active.target_host,
        target_endpoint: active.target_endpoint,
        status: active.status,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib active_tracker 2>&1 | tail -5`
Expected: 3 tests PASS. (The existing `active_tracker::remove(&request_id).await;` statement in `runner.rs` compiles unchanged — the returned `Option` is discarded by the statement position; if the compiler warns about an unused result there, leave it for Task 7 which consumes it.)

- [ ] **Step 5: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add src/active_tracker.rs
git commit -m "feat: track sni, target, and status per connection for accounting

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Runner integration — fill dimensions, count on read, emit CDR

**Files:**
- Modify: `src/runner.rs`
- Test: `src/runner.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `active_tracker::{set_sni, set_target, set_status, remove -> Option<ClosedConnection>}` (Task 6); `accounting::{enabled, submit, CdrRecord, ConnStatus, ListenerType}` (Tasks 2, 5).
- Produces: every connection termination path emits one CDR record when accounting is enabled. Byte counts move to the read side of `pipe` (spec: billed as received).

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `src/runner.rs`:

```rust
    #[tokio::test]
    async fn denied_connection_tracks_sni_status_and_client_hello_bytes() {
        // a raw ClientHello for "localhost", exactly as a real client sends it
        let config = RustlsClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut connection = ClientConnection::new(Arc::new(config), server_name).unwrap();
        let mut hello_bytes = Vec::new();
        connection
            .write_tls(&mut Cursor::new(&mut hello_bytes))
            .unwrap();

        // ALLOW policy with empty rules denies every host
        let listener = Arc::new(Listener {
            bind: "127.0.0.1:0".into(),
            target: None,
            target_port: 443,
            policy: Policy::ALLOW,
            rules: Rules {
                static_hosts: vec![],
                patterns: vec![],
            },
            max_idle_time_ms: Some(5_000),
            speed_limit: Some(0.0),
            mode: ListenerMode::Passthrough,
            upstream_tls: false,
        });
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let request_id = RequestId::new();
        let request_id_for_task = request_id.clone();
        let stats = Arc::new(ListenerStats::new("deny-test", 5_000));
        let proxy_task = tokio::spawn(async move {
            let (socket, addr) = proxy.accept().await.unwrap();
            active_tracker::put(&request_id_for_task, "deny-test", addr).await;
            let ext = Extensible::of(socket);
            ext.extend(request_id_for_task).await;
            Runner::worker_passthrough(
                Arc::new("deny-test".into()),
                ext,
                listener,
                stats,
                Arc::new(RwLock::new(Controller::new())),
            )
            .await
        });

        let mut socket = TcpStream::connect(proxy_address).await.unwrap();
        socket.write_all(&hello_bytes).await.unwrap();

        let error = timeout(Duration::from_secs(2), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("denied by ACL"));
        let closed = active_tracker::remove(&request_id)
            .await
            .expect("denied connection should still be tracked");
        assert_eq!(closed.status, crate::accounting::ConnStatus::Denied);
        assert_eq!(closed.sni, "localhost");
        assert!(
            closed.uploaded_bytes > 0,
            "client hello bytes must be billed even when denied"
        );
        assert_eq!(closed.target_endpoint, "", "denied before any target was chosen");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib runner::tests::denied_connection 2>&1 | tail -10`
Expected: FAIL — `closed.status` is `ConnectFailed` (not `Denied`), `sni` empty, `uploaded_bytes` 0 (header is currently only counted after a successful upstream connect).

- [ ] **Step 3: Implement the runner changes**

All in `src/runner.rs`. Add imports:

```rust
use crate::accounting::{self, CdrRecord, ConnStatus, ListenerType};
use std::time::{SystemTime, UNIX_EPOCH};
```

**(a) `handle_new_socket` — emit the CDR.** Replace the body section from `let rr = Self::worker(` through `active_tracker::remove(&request_id).await;` with:

```rust
            let listener_mode = listener_config.mode;
            let rr = Self::worker(
                Arc::clone(&name),
                ext,
                listener_config,
                stats_local_clone,
                controller,
                ca,
            )
            .await;
            if rr.is_err() {
                let err = rr.err().unwrap();
                warn!("{request_id} connection error: {err}");
            }
            let closed = active_tracker::remove(&request_id).await;
            if let Some(closed) = closed {
                if accounting::enabled() {
                    let end_unix_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.as_millis())
                        .unwrap_or_default();
                    accounting::submit(CdrRecord {
                        listener_type: ListenerType::from_mode(listener_mode),
                        connection_id: request_id.to_string(),
                        listener_name: (*name).clone(),
                        sni: closed.sni,
                        target_host: closed.target_host,
                        target_endpoint: closed.target_endpoint,
                        remote_address: closed.remote_address.to_string(),
                        status: closed.status,
                        uploaded_bytes: closed.uploaded_bytes,
                        downloaded_bytes: closed.downloaded_bytes,
                        start_unix_ms: closed.started_at_unix_ms,
                        end_unix_ms,
                    });
                }
            }
```

(Note `Self::worker(Arc::clone(&name), …)` — `name` was previously moved into the call; it is now needed afterwards for `listener_name`.)

**(b) `worker_passthrough` — record SNI, bill the ClientHello before the ACL check, set target/status.** After `let sni_target = client_hello.sni_host;` / `info!("{conn_id} sni target is {sni_target}");` insert:

```rust
        active_tracker::set_sni(&conn_id, &sni_target).await;
        context.increase_uploaded_bytes(header_len);
        active_tracker::add_uploaded(&conn_id, header_len as u64).await;
```

Delete the now-duplicate pair of lines further down (after `upstream_write.write_all(&client_hello.buffered).await?;`):

```rust
        context.increase_uploaded_bytes(header_len);
        active_tracker::add_uploaded(&conn_id, header_len as u64).await;
```

After the `Self::resolve_target(…)` call succeeds insert:

```rust
        active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint).await;
```

After `info!("{conn_id} connected to TLS upstream {}", selected.endpoint);` insert:

```rust
        active_tracker::set_status(&conn_id, ConnStatus::Ok).await;
```

(`worker_passthrough` takes `context: Arc<ListenerStats>` — already in scope.)

**(c) `worker_terminate`.** After `info!("{conn_id} received TLS ClientHello for SNI target {sni_target}");` insert:

```rust
        active_tracker::set_sni(&conn_id, &sni_target).await;
```

After the `let selected = Self::resolve_target(…)?;` call insert:

```rust
        active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint).await;
```

After `info!("{conn_id} connected to upstream {}", selected.endpoint);` insert:

```rust
        active_tracker::set_status(&conn_id, ConnStatus::Ok).await;
```

**(d) `worker_forward`.** After the `let resolved = crate::forward::choose_online(…)?;` block insert:

```rust
        active_tracker::set_target(&conn_id, &resolved.tls_server_name, &resolved.endpoint).await;
```

After `info!("{conn_id} connected to forward upstream {}", resolved.endpoint);` insert:

```rust
        active_tracker::set_status(&conn_id, ConnStatus::Ok).await;
```

**(e) `resolve_target` — mark ACL denials.** In the `false` arm, before `return Err(…)`:

```rust
            false => {
                info!("{conn_id} {sni_target} denied by ACL");
                active_tracker::set_status(conn_id, ConnStatus::Denied).await;
                return Err(anyhow!("{sni_target} denied by ACL"));
            }
```

**(f) `pipe` — count bytes when read, not when relayed** (spec: traffic is billed as received on each leg). Replace the loop body after `if n == 0 { break; }` with:

```rust
                counter.fetch_add(n as u64, Ordering::SeqCst);
                if is_upload {
                    context.increase_uploaded_bytes(n);
                    active_tracker::add_uploaded(&request_id, n as u64).await;
                } else {
                    context.increase_downloaded_bytes(n);
                    active_tracker::add_downloaded(&request_id, n as u64).await;
                }
                limiter.consume(n).await;
                let write_result = writer.write_all(&buf[0..n]).await;
                match write_result {
                    Err(_) => {
                        break;
                    }
                    Ok(_) => {
                        idle_tracker.lock().await.mark();
                    }
                }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib runner 2>&1 | tail -5`
Expected: all runner tests PASS, including the three pre-existing relay tests (byte totals are unchanged on success paths — only the counting moment moved).

- [ ] **Step 5: Run the full suite and commit**

Run: `cargo test 2>&1 | tail -5` — expected: all pass.

```bash
git add src/runner.rs
git commit -m "feat: emit accounting CDR record on every connection close

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Startup wiring and documentation

**Files:**
- Modify: `src/main.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: `accounting::{init, shutdown}` (Task 5); `Config.accounting` (Task 1).
- Produces: accounting starts with the process and flushes on graceful exit.

- [ ] **Step 1: Wire init and shutdown into `run()`**

In `src/main.rs` `run()`, after `let config = load_config(&config_path).await?;` insert:

```rust
    if let Some(accounting_config) = &config.accounting {
        accounting::init(accounting_config)
            .await
            .context("failed to initialize accounting")?;
    }
```

At the end of `run()`, immediately before the final `Ok(())`, insert:

```rust
    accounting::shutdown().await;
```

- [ ] **Step 2: Build and verify startup behavior end-to-end**

```bash
cargo build 2>&1 | tail -3
```
Expected: clean build.

Smoke-test with a scratch config (do NOT modify the repo's `config.yaml`):

```bash
cd /tmp && cat > tlsproxy-accounting-smoke.yaml <<'EOF'
listeners:
  smoke:
    bind: 127.0.0.1:19443
    target: ''
    target_port: 443
    policy: DENY
    rules:
      static_hosts: []
      patterns: []
    mode: passthrough
options:
  runtime_dir: /tmp/tlsproxy-smoke-runtime
dns: {}
accounting:
  enabled: true
  log_file: /tmp/tlsproxy-smoke-cdr.log
  rotate_size: 1MiB
  max_keep: 3
  compression: none
EOF
/home/code/home_workspace/tlsproxy_rs/target/debug/tlsproxy validate -c /tmp/tlsproxy-accounting-smoke.yaml
```
Expected: `.../tlsproxy-accounting-smoke.yaml is valid`.

Then run the proxy briefly, drive one denied connection through it, and check the CDR:

```bash
timeout 10 /home/code/home_workspace/tlsproxy_rs/target/debug/tlsproxy run -c /tmp/tlsproxy-accounting-smoke.yaml &
sleep 2
curl -sk --max-time 3 https://localhost:19443/ --resolve localhost:19443:127.0.0.1 || true
sleep 2
cat /tmp/tlsproxy-smoke-cdr.log
```
Expected: header line plus one `TLS_PASSTHROUGH,…,smoke,localhost,,,127.0.0.1:…,DENIED,…` row (policy DENY with empty rules allows everything — wait: DENY policy with no matching rules means NOT matched → allowed; the connect to a real upstream will then fail, giving `CONNECT_FAILED`. Either `DENIED` or `CONNECT_FAILED` is acceptable here; what matters is that exactly one data row appears with the right listener name and SNI `localhost`.)
(The admin server is absent from this config, so the proxy waits on Ctrl-C; `timeout` kills it — records were already flushed per-connection.)

- [ ] **Step 3: Document the feature in README.md**

Add a section (match the README's existing tone/format; place it near the other configuration sections):

```markdown
## Accounting (CDR log)

When enabled, every client connection writes one CSV line on close — suitable
for multi-tenant usage accounting by SNI, listener, or target:

```yaml
accounting:
  enabled: true
  log_file: cdr.log
  rotate_size: 18MiB   # bytes, or KiB/MiB/GiB
  max_keep: 10         # rotated files kept
  compress_after: 3    # cdr.log.1..3 stay raw; cdr.log.4+ are compressed
  compression: zstd    # zstd (default) | gzip | none
```

Columns: `listener_type, connection_id, listener_name, sni, target_host,
target_endpoint, remote_address, status, uploaded_bytes, downloaded_bytes,
connection_start, connection_end`. `uploaded_bytes` counts bytes received
from the client, `downloaded_bytes` bytes received from upstream — billed as
received. `status` is `OK`, `DENIED` (ACL), or `CONNECT_FAILED`. Rotation is
logrotate-style (`.1` is newest); rotated files past `compress_after` are
compressed in-process. Records are written when a connection ends; connections
open when the process is killed are lost (best effort).
```

- [ ] **Step 4: Final verification**

```bash
cargo test 2>&1 | tail -5
cargo build --release 2>&1 | tail -3
```
Expected: all tests pass, release build clean.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs README.md
git commit -m "feat: wire accounting CDR log into startup and document it

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
