use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DEFAULT_MAX_HTTP_HEADER_SIZE: usize = 256 * 1024;

/// Header carrying the random tokens of requests this proxy (or another
/// tlsproxy hop) forwarded to a plaintext HTTP upstream. Inbound values are
/// passed through untouched and a fresh token is appended per hop, so in a
/// self-connection loop — direct or through several proxies — the originating
/// instance recognizes its own recent token and closes the loop.
pub const LOOP_TOKEN_HEADER: &str = "x-tlsproxy-rid";

/// The buffered start of a plaintext HTTP/1.x connection: everything read so
/// far (`buffered`, which may extend past the header block into the body) and
/// the routing facts parsed from the request head.
#[derive(Debug)]
pub struct HttpHead {
    pub method: String,
    /// Origin-form request path including any query string.
    pub target: String,
    /// Hostname from the `Host` header, without any port.
    pub host: String,
    /// Explicit port from the `Host` header, when the client sent one.
    pub port: Option<u16>,
    /// The raw `Host` header value (`example.com` or `example.com:8080`).
    pub host_raw: String,
    pub authorization: Option<String>,
    /// Values of `X-Tlsproxy-Rid` headers on the inbound request, checked
    /// against recently forwarded tokens to detect self-connection loops.
    pub loop_tokens: Vec<String>,
    strip_authorization: bool,
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
    let (method, target) = validate_request_line(request_line)?;

    let mut host_value: Option<&str> = None;
    let mut authorization: Option<String> = None;
    let mut loop_tokens: Vec<String> = Vec::new();
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
        } else if name.trim().eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_string());
        } else if name.trim().eq_ignore_ascii_case(LOOP_TOKEN_HEADER) {
            loop_tokens.extend(value.split(',').map(|token| token.trim().to_string()).filter(|token| !token.is_empty()));
        }
    }
    let host_raw = host_value
        .ok_or_else(|| anyhow!("HTTP request does not contain a Host header"))?
        .to_string();
    let (host, port) = split_host_port(&host_raw)?;
    Ok(HttpHead {
        method,
        target,
        host,
        port,
        host_raw,
        authorization,
        loop_tokens,
        strip_authorization: false,
        buffered,
        head_len,
    })
}

fn validate_request_line(line: &str) -> Result<(String, String)> {
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
    if !target.starts_with('/') {
        bail!("only origin-form HTTP request targets are supported");
    }
    Ok((method.to_string(), target.to_string()))
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

/// How the first request's body is delimited on the wire. Anything that
/// cannot be delimited unambiguously is rejected to prevent request smuggling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    Length(u64),
    Chunked,
}

impl HttpHead {
    pub fn consume_authorization(&mut self) { self.strip_authorization = true; }

    /// Bytes read past the request head (the start of the body, and possibly
    /// pipelined data beyond it).
    pub fn body_prefix(&self) -> &[u8] { &self.buffered[self.head_len..] }

    /// Determines how the request body ends, so the data plane can stop
    /// relaying client bytes at the message boundary instead of streaming the
    /// connection verbatim (which would let pipelined requests bypass per-path
    /// authentication and routing).
    pub fn body_framing(&self) -> Result<BodyFraming> {
        let head = std::str::from_utf8(&self.buffered[..self.head_len])
            .map_err(|_| anyhow!("HTTP request head is not valid UTF-8"))?;
        let mut content_length: Option<u64> = None;
        let mut transfer_encoding: Option<String> = None;
        let mut last_was_framing = false;
        for line in head.split('\n').skip(1).map(|line| line.strip_suffix('\r').unwrap_or(line)) {
            if line.is_empty() { break; }
            if line.starts_with([' ', '\t']) {
                if last_was_framing {
                    bail!("folded body-framing headers are not supported");
                }
                continue;
            }
            let Some((name, value)) = line.split_once(':') else { continue };
            let name = name.trim().to_ascii_lowercase();
            last_was_framing = matches!(name.as_str(), "content-length" | "transfer-encoding");
            match name.as_str() {
                "content-length" => {
                    let value = value.trim();
                    let parsed = value
                        .parse::<u64>()
                        .map_err(|_| anyhow!("invalid Content-Length `{value}`"))?;
                    if content_length.is_some_and(|previous| previous != parsed) {
                        bail!("conflicting Content-Length headers");
                    }
                    content_length = Some(parsed);
                }
                "transfer-encoding" => {
                    if transfer_encoding.is_some() {
                        bail!("multiple Transfer-Encoding headers are not supported");
                    }
                    transfer_encoding = Some(value.trim().to_ascii_lowercase());
                }
                _ => {}
            }
        }
        match transfer_encoding {
            Some(encoding) => {
                if content_length.is_some() {
                    bail!("Content-Length combined with Transfer-Encoding is rejected");
                }
                if encoding != "chunked" {
                    bail!("unsupported Transfer-Encoding `{encoding}`");
                }
                Ok(BodyFraming::Chunked)
            }
            None => Ok(BodyFraming::Length(content_length.unwrap_or(0))),
        }
    }
    pub fn strip_path_prefix(&mut self, prefix: &str) {
        let prefix = prefix.trim_end_matches('/');
        let stripped = self.target.strip_prefix(prefix).unwrap_or(&self.target);
        let new_target = if stripped.is_empty() { "/" } else { stripped };
        let head = String::from_utf8_lossy(&self.buffered[..self.head_len]);
        if let Some(line_end) = head.find('\n') {
            let first = head[..line_end].trim_end_matches('\r');
            let mut parts = first.split_ascii_whitespace();
            let method = parts.next().unwrap_or(&self.method);
            let _old_target = parts.next();
            let version = parts.next().unwrap_or("HTTP/1.1");
            let ending = if head.as_bytes().get(line_end.wrapping_sub(1)) == Some(&b'\r') { "\r\n" } else { "\n" };
            let mut rebuilt = format!("{method} {new_target} {version}{ending}").into_bytes();
            rebuilt.extend_from_slice(&self.buffered[line_end + 1..]);
            self.buffered = rebuilt;
            self.head_len = find_head_end(&self.buffered)
                .expect("rewriting a complete HTTP head preserves its terminator");
            self.target = new_target.to_string();
        }
    }

    /// Rebuilds the buffered bytes with proxy forwarding headers injected into
    /// the first request: the client is appended to `X-Forwarded-For` and
    /// `Forwarded`, while `X-Forwarded-Host` and `X-Forwarded-Proto` are set
    /// to this hop's values. Bytes past the header block pass through as-is.
    pub fn with_forwarded_headers(&self, client_ip: IpAddr) -> Vec<u8> {
        self.rewrite_for_proxy(client_ip, "http", None)
    }

    pub fn rewrite_for_proxy(
        &self,
        client_ip: IpAddr,
        scheme: &str,
        upstream_host: Option<&str>,
    ) -> Vec<u8> {
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
                    "host" if upstream_host.is_some() => {
                        skipping_folds = true;
                        continue;
                    }
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
                    "authorization" if self.strip_authorization => {
                        skipping_folds = true;
                        continue;
                    }
                    "x-forwarded-host" | "x-forwarded-proto" | "connection" => {
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
            "{forwarded_for};host={};proto={scheme}", self.host_raw
        ));
        let mut push_header = |name: &str, value: &str| {
            output.extend_from_slice(name.as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(value.as_bytes());
            output.extend_from_slice(b"\r\n");
        };
        push_header("X-Forwarded-For", &xff_values.join(", "));
        push_header("X-Forwarded-Host", &self.host_raw);
        push_header("X-Forwarded-Proto", scheme);
        push_header("Forwarded", &forwarded_values.join(", "));
        // Fresh loop token per hop; inbound tokens from other tlsproxy hops
        // were passed through above so a multi-instance loop is still caught
        // by whichever instance sees its own token return.
        let loop_token = {
            use rand::Rng as _;
            format!("{:032x}", rand::rng().random::<u128>())
        };
        crate::hello_cache::insert_request_token(loop_token.clone());
        push_header("X-Tlsproxy-Rid", &loop_token);
        // The current data plane selects a path backend once per connection.
        // Closing after the response guarantees every subsequent request is
        // independently routed, including clients that otherwise use keep-alive.
        push_header("Connection", "close");
        if let Some(host) = upstream_host {
            push_header("Host", host);
        }
        output.extend_from_slice(b"\r\n");
        output.extend_from_slice(&self.buffered[self.head_len..]);
        output
    }
}

/// Passes a chunked request body through verbatim while tracking the chunk
/// framing, and reports EOF exactly at the end of the message (terminal chunk
/// plus trailers). Framing lines are read one byte at a time so no bytes
/// belonging to a pipelined follow-up request are ever consumed.
pub struct ChunkedBodyReader<R> {
    inner: R,
    state: ChunkedState,
}

enum ChunkedState {
    SizeLine { line: Vec<u8> },
    Data { remaining: u64 },
    DataEnd { seen_cr: bool },
    Trailers { line: Vec<u8>, consumed: usize },
    Done,
}

const MAX_CHUNK_LINE: usize = 256;
const MAX_TRAILER_BYTES: usize = 16 * 1024;

impl<R> ChunkedBodyReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, state: ChunkedState::SizeLine { line: Vec::new() } }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ChunkedBodyReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::io::{Error, ErrorKind};
        use std::task::Poll;
        let this = self.as_mut().get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let want = match &this.state {
            ChunkedState::Done => return Poll::Ready(Ok(())),
            // Framing lines advance one byte at a time to avoid overshooting
            // the message boundary; chunk payloads read in bulk.
            ChunkedState::Data { remaining } => (*remaining).min(buffer.remaining() as u64) as usize,
            _ => 1,
        };
        let mut chunk = [0u8; 8192];
        let mut scratch = tokio::io::ReadBuf::new(&mut chunk[..want.min(8192)]);
        match std::pin::Pin::new(&mut this.inner).poll_read(context, &mut scratch) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(cause)) => return Poll::Ready(Err(cause)),
            Poll::Ready(Ok(())) => {}
        }
        let filled = scratch.filled();
        if filled.is_empty() {
            return Poll::Ready(Err(Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed inside a chunked request body",
            )));
        }
        let malformed = |message: &str| Error::new(ErrorKind::InvalidData, message.to_string());
        match &mut this.state {
            ChunkedState::SizeLine { line } => {
                let byte = filled[0];
                if byte == b'\n' {
                    let text = std::str::from_utf8(line).map_err(|_| malformed("chunk size line is not UTF-8"))?;
                    let size_text = text.trim_end_matches('\r');
                    let size_text = size_text.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size_text, 16)
                        .map_err(|_| malformed("invalid chunk size"))?;
                    this.state = if size == 0 {
                        ChunkedState::Trailers { line: Vec::new(), consumed: 0 }
                    } else {
                        ChunkedState::Data { remaining: size }
                    };
                } else {
                    if line.len() >= MAX_CHUNK_LINE {
                        return Poll::Ready(Err(malformed("chunk size line too long")));
                    }
                    line.push(byte);
                }
            }
            ChunkedState::Data { remaining } => {
                *remaining -= filled.len() as u64;
                if *remaining == 0 {
                    this.state = ChunkedState::DataEnd { seen_cr: false };
                }
            }
            ChunkedState::DataEnd { seen_cr } => match (filled[0], *seen_cr) {
                (b'\r', false) => *seen_cr = true,
                (b'\n', _) => this.state = ChunkedState::SizeLine { line: Vec::new() },
                _ => return Poll::Ready(Err(malformed("chunk data is not terminated by CRLF"))),
            },
            ChunkedState::Trailers { line, consumed } => {
                *consumed += 1;
                if *consumed > MAX_TRAILER_BYTES {
                    return Poll::Ready(Err(malformed("chunked trailers too long")));
                }
                let byte = filled[0];
                if byte == b'\n' {
                    if line.iter().all(|value| *value == b'\r') && line.len() <= 1 {
                        this.state = ChunkedState::Done;
                    } else {
                        line.clear();
                    }
                } else {
                    line.push(byte);
                }
            }
            ChunkedState::Done => unreachable!("Done returns before reading"),
        }
        buffer.put_slice(filled);
        Poll::Ready(Ok(()))
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
    async fn rewrite_injects_a_registered_loop_token_and_passes_foreign_tokens_through() {
        let head = parse("GET / HTTP/1.1\r\nHost: app.example\r\nX-Tlsproxy-Rid: feedface\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(head.loop_tokens, vec!["feedface".to_string()]);
        let rewritten = String::from_utf8(head.with_forwarded_headers("192.0.2.7".parse::<IpAddr>().unwrap())).unwrap();
        // the upstream hop's token is preserved so the originating instance
        // can still recognize it in a multi-instance loop
        assert!(rewritten.contains("X-Tlsproxy-Rid: feedface\r\n"));
        let ours = rewritten
            .lines()
            .filter_map(|line| line.strip_prefix("X-Tlsproxy-Rid: "))
            .find(|value| *value != "feedface")
            .expect("a fresh token is appended");
        assert_eq!(ours.len(), 32);
        assert!(crate::hello_cache::request_token_is_looped(ours), "our token is registered");
        assert!(!crate::hello_cache::request_token_is_looped(ours), "detection consumes the token");
        assert!(!crate::hello_cache::request_token_is_looped("feedface"), "foreign tokens are not registered");
    }

    #[tokio::test]
    async fn body_framing_handles_lengths_chunked_and_smuggling_conflicts() {
        use super::BodyFraming;
        let head = parse("GET / HTTP/1.1\r\nHost: a\r\n\r\n").await.unwrap();
        assert_eq!(head.body_framing().unwrap(), BodyFraming::Length(0));
        let head = parse("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 12\r\n\r\n").await.unwrap();
        assert_eq!(head.body_framing().unwrap(), BodyFraming::Length(12));
        let head = parse("POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        assert_eq!(head.body_framing().unwrap(), BodyFraming::Chunked);
        let head = parse("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        assert!(head.body_framing().is_err());
        let head = parse("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\n").await.unwrap();
        assert!(head.body_framing().is_err());
        let head = parse("POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\n\r\n").await.unwrap();
        assert!(head.body_framing().is_err());
    }

    #[tokio::test]
    async fn chunked_reader_stops_exactly_at_the_message_boundary() {
        use tokio::io::AsyncReadExt;
        let wire = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\nGET /admin HTTP/1.1\r\n".to_vec();
        let mut reader = super::ChunkedBodyReader::new(Cursor::new(wire));
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n");

        let wire = b"3\r\nabc\r\n0\r\nExpires: soon\r\n\r\ntrailing pipelined".to_vec();
        let mut reader = super::ChunkedBodyReader::new(Cursor::new(wire));
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"3\r\nabc\r\n0\r\nExpires: soon\r\n\r\n");

        let truncated = b"5\r\nab".to_vec();
        let mut reader = super::ChunkedBodyReader::new(Cursor::new(truncated));
        let mut body = Vec::new();
        assert!(reader.read_to_end(&mut body).await.is_err());
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
