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
