use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DEFAULT_MAX_HTTP_HEADER_SIZE: usize = 256 * 1024;

/// The buffered start of a plaintext HTTP/1.x connection: everything read so
/// far (`buffered`, which may extend past the header block into the body) and
/// the routing facts parsed from the request head.
#[derive(Debug)]
pub struct HttpHead {
    /// Hostname from the `Host` header, without any port.
    pub host: String,
    /// Explicit port from the `Host` header, when the client sent one.
    pub port: Option<u16>,
    /// The raw `Host` header value (`example.com` or `example.com:8080`).
    pub host_raw: String,
    /// Bytes read from the client; the head occupies `..head_len`.
    pub buffered: Vec<u8>,
    head_len: usize,
}

pub async fn read_http_head<R>(reader: &mut R, timeout: Duration, max_size: usize) -> Result<HttpHead>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_http_head_inner(reader, max_size))
        .await
        .map_err(|_| anyhow!("HTTP request head timed out after {timeout:?}"))?
}

async fn read_http_head_inner<R>(reader: &mut R, max_size: usize) -> Result<HttpHead>
where
    R: AsyncRead + Unpin,
{
    let mut buffered = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(head_len) = find_head_end(&buffered) {
            return parse_head(buffered, head_len);
        }
        if buffered.len() >= max_size {
            bail!("HTTP request head exceeds the {max_size}-byte limit");
        }
        let read_size = (max_size - buffered.len()).min(chunk.len());
        let count = reader
            .read(&mut chunk[..read_size])
            .await
            .context("failed to read HTTP request head")?;
        if count == 0 {
            bail!("connection closed before a complete HTTP request head was received");
        }
        buffered.extend_from_slice(&chunk[..count]);
    }
}

/// Returns the length of the head including the blank-line terminator.
/// Accepts `\r\n\r\n` and, leniently, bare `\n\n`.
fn find_head_end(buffered: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index < buffered.len() {
        if buffered[index] == b'\n' {
            match buffered.get(index + 1) {
                Some(b'\n') => return Some(index + 2),
                Some(b'\r') if buffered.get(index + 2) == Some(&b'\n') => return Some(index + 3),
                _ => {}
            }
        }
        index += 1;
    }
    None
}

fn parse_head(buffered: Vec<u8>, head_len: usize) -> Result<HttpHead> {
    let head = std::str::from_utf8(&buffered[..head_len])
        .map_err(|_| anyhow!("HTTP request head is not valid UTF-8"))?;
    let mut lines = head.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line));
    let request_line = lines.next().unwrap_or("");
    validate_request_line(request_line)?;

    let mut host_value: Option<&str> = None;
    let mut last_was_host = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            // obs-fold continuation; a folded Host header is not supported
            if last_was_host {
                bail!("folded Host header is not supported");
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            bail!("malformed HTTP header line");
        };
        last_was_host = name.trim().eq_ignore_ascii_case("host");
        if last_was_host {
            if host_value.is_some() {
                bail!("HTTP request contains multiple Host headers");
            }
            host_value = Some(value.trim());
        }
    }
    let host_raw = host_value
        .ok_or_else(|| anyhow!("HTTP request does not contain a Host header"))?
        .to_string();
    let (host, port) = split_host_port(&host_raw)?;
    Ok(HttpHead {
        host,
        port,
        host_raw,
        buffered,
        head_len,
    })
}

fn validate_request_line(line: &str) -> Result<()> {
    let mut parts = line.split_ascii_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty() || target.is_empty() || parts.next().is_some() {
        bail!("malformed HTTP request line");
    }
    if !version.starts_with("HTTP/1.") {
        bail!("only HTTP/1.x requests are supported, got `{version}`");
    }
    Ok(())
}

/// Splits `example.com`, `example.com:8080`, `[::1]`, or `[::1]:8080` from a
/// Host header value.
fn split_host_port(value: &str) -> Result<(String, Option<u16>)> {
    if value.is_empty() {
        bail!("HTTP Host header is empty");
    }
    let (host, port_text) = if let Some(rest) = value.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("invalid IPv6 Host header `{value}`"))?;
        (host, rest.strip_prefix(':'))
    } else if let Some((host, port)) = value.rsplit_once(':') {
        (host, Some(port))
    } else {
        (value, None)
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        bail!("invalid HTTP Host header `{value}`");
    }
    let port = match port_text {
        Some(port) => Some(
            port.parse::<u16>()
                .map_err(|_| anyhow!("invalid port in Host header `{value}`"))?,
        ),
        None => None,
    };
    Ok((host.to_string(), port))
}

impl HttpHead {
    /// Rebuilds the buffered bytes with proxy forwarding headers injected into
    /// the first request: the client is appended to `X-Forwarded-For` and
    /// `Forwarded`, while `X-Forwarded-Host` and `X-Forwarded-Proto` are set
    /// to this hop's values. Bytes past the header block pass through as-is.
    pub fn with_forwarded_headers(&self, client_ip: IpAddr) -> Vec<u8> {
        let head = String::from_utf8_lossy(&self.buffered[..self.head_len]);
        let mut lines = head
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line));
        let mut output = Vec::with_capacity(self.buffered.len() + 256);
        let request_line = lines.next().unwrap_or("");
        output.extend_from_slice(request_line.as_bytes());
        output.extend_from_slice(b"\r\n");

        let mut xff_values: Vec<String> = Vec::new();
        let mut forwarded_values: Vec<String> = Vec::new();
        let mut skipping_folds = false;
        for line in lines {
            if line.is_empty() {
                break;
            }
            if line.starts_with([' ', '\t']) {
                if skipping_folds {
                    continue;
                }
                output.extend_from_slice(line.as_bytes());
                output.extend_from_slice(b"\r\n");
                continue;
            }
            skipping_folds = false;
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "x-forwarded-for" => {
                        xff_values.push(value.trim().to_string());
                        skipping_folds = true;
                        continue;
                    }
                    "forwarded" => {
                        forwarded_values.push(value.trim().to_string());
                        skipping_folds = true;
                        continue;
                    }
                    "x-forwarded-host" | "x-forwarded-proto" => {
                        skipping_folds = true;
                        continue;
                    }
                    _ => {}
                }
            }
            output.extend_from_slice(line.as_bytes());
            output.extend_from_slice(b"\r\n");
        }

        xff_values.push(client_ip.to_string());
        let forwarded_for = match client_ip {
            IpAddr::V4(ip) => format!("for={ip}"),
            IpAddr::V6(ip) => format!("for=\"[{ip}]\""),
        };
        forwarded_values.push(format!(
            "{forwarded_for};host={};proto=http",
            self.host_raw
        ));
        let mut push_header = |name: &str, value: &str| {
            output.extend_from_slice(name.as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(value.as_bytes());
            output.extend_from_slice(b"\r\n");
        };
        push_header("X-Forwarded-For", &xff_values.join(", "));
        push_header("X-Forwarded-Host", &self.host_raw);
        push_header("X-Forwarded-Proto", "http");
        push_header("Forwarded", &forwarded_values.join(", "));
        output.extend_from_slice(b"\r\n");
        output.extend_from_slice(&self.buffered[self.head_len..]);
        output
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::net::IpAddr;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use super::{read_http_head, DEFAULT_MAX_HTTP_HEADER_SIZE};

    async fn parse(request: &str) -> anyhow::Result<super::HttpHead> {
        read_http_head(
            &mut Cursor::new(request.as_bytes().to_vec()),
            Duration::from_secs(1),
            DEFAULT_MAX_HTTP_HEADER_SIZE,
        )
        .await
    }

    #[tokio::test]
    async fn parses_host_and_optional_port() {
        let head = parse("GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(head.host, "example.com");
        assert_eq!(head.port, None);

        let head = parse("GET / HTTP/1.1\r\nHOST: example.com:8080\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(head.host, "example.com");
        assert_eq!(head.port, Some(8080));
        assert_eq!(head.host_raw, "example.com:8080");

        let head = parse("GET / HTTP/1.0\r\nHost: [::1]:8443\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(head.host, "::1");
        assert_eq!(head.port, Some(8443));
    }

    #[tokio::test]
    async fn tolerates_bare_lf_line_endings() {
        let head = parse("GET / HTTP/1.1\nHost: lf.example\n\n").await.unwrap();
        assert_eq!(head.host, "lf.example");
    }

    #[tokio::test]
    async fn reads_fragmented_request_and_keeps_body_prefix() {
        let request = b"POST /submit HTTP/1.1\r\nHost: frag.example\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let expected = request.clone();
        tokio::spawn(async move {
            for chunk in request.chunks(7) {
                writer.write_all(chunk).await.unwrap();
            }
        });
        let head = read_http_head(
            &mut reader,
            Duration::from_secs(1),
            DEFAULT_MAX_HTTP_HEADER_SIZE,
        )
        .await
        .unwrap();
        assert_eq!(head.host, "frag.example");
        assert_eq!(head.buffered, expected);
        assert!(head.buffered.ends_with(b"hello"));
    }

    #[tokio::test]
    async fn rejects_missing_or_duplicate_host() {
        let error = parse("GET / HTTP/1.1\r\nAccept: */*\r\n\r\n").await.unwrap_err();
        assert!(error.to_string().contains("Host header"));

        let error = parse("GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("multiple Host"));
    }

    #[tokio::test]
    async fn rejects_non_http1_traffic() {
        let error = parse("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await.unwrap_err();
        assert!(error.to_string().contains("HTTP/1.x"));

        let error = parse("\x16\x03\x01 not http\r\n\r\n").await.unwrap_err();
        assert!(error.to_string().contains("HTTP"));
    }

    #[tokio::test]
    async fn rejects_head_over_limit() {
        let request = format!(
            "GET / HTTP/1.1\r\nHost: big.example\r\nX-Pad: {}\r\n\r\n",
            "a".repeat(64)
        );
        let error = read_http_head(
            &mut Cursor::new(request.into_bytes()),
            Duration::from_secs(1),
            32,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn times_out_on_incomplete_head() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"GET / HTTP/1.1\r\nHos").await.unwrap();
        let error = read_http_head(&mut reader, Duration::from_millis(10), 1024)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn injects_forwarding_headers_and_merges_existing_chain() {
        let head = parse(
            "GET /path HTTP/1.1\r\nHost: app.example:8080\r\nX-Forwarded-For: 10.0.0.9\r\nX-Forwarded-Proto: https\r\nForwarded: for=10.0.0.9\r\nAccept: */*\r\n\r\n",
        )
        .await
        .unwrap();
        let rewritten = head.with_forwarded_headers("192.0.2.7".parse::<IpAddr>().unwrap());
        let text = String::from_utf8(rewritten).unwrap();
        assert!(text.starts_with("GET /path HTTP/1.1\r\n"));
        assert!(text.contains("Host: app.example:8080\r\n"));
        assert!(text.contains("Accept: */*\r\n"));
        assert!(text.contains("X-Forwarded-For: 10.0.0.9, 192.0.2.7\r\n"));
        assert!(text.contains("X-Forwarded-Host: app.example:8080\r\n"));
        assert!(text.contains("X-Forwarded-Proto: http\r\n"));
        assert!(text.contains("Forwarded: for=10.0.0.9, for=192.0.2.7;host=app.example:8080;proto=http\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
        // the client-sent proto must not survive
        assert!(!text.contains("https"));
    }

    #[tokio::test]
    async fn forwarded_header_quotes_ipv6_and_body_passes_through() {
        let head = parse("POST / HTTP/1.1\r\nHost: v6.example\r\nContent-Length: 4\r\n\r\nbody")
            .await
            .unwrap();
        let rewritten = head.with_forwarded_headers("2001:db8::1".parse::<IpAddr>().unwrap());
        let text = String::from_utf8(rewritten).unwrap();
        assert!(text.contains("X-Forwarded-For: 2001:db8::1\r\n"));
        assert!(text.contains("Forwarded: for=\"[2001:db8::1]\";host=v6.example;proto=http\r\n"));
        assert!(text.ends_with("\r\n\r\nbody"));
    }
}
