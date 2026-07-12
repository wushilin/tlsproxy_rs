use crate::config::{Compression, ListenerMode};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Compression, ListenerMode};
    use std::fs;
    use std::io::Read;
    use std::path::Path;
    use tempfile::tempdir;

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
}
