//! HTTP interception layer: reverse-proxy host/path routing, static file
//! serving, and HTTPS redirects. Reached both by a plain-HTTP listener and by
//! the TLS terminate backend re-intercepting a decrypted stream as HTTP.

pub mod error_pages;
pub mod static_files;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::info;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::io::{AsyncRead, AsyncWrite};
use base64::Engine;
use subtle::ConstantTimeEq;

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::conn_stream::ConnStream;
use crate::dataplane::pipeline::{Intercept, Intercepted};
use crate::dataplane::RelayPolicy;
use crate::http_header;
use crate::upstream_tls::connect_trust_all_tls;

/// Interception point for an HTTP connection: its request head. The
/// continuation stream is the same connection, positioned at the message body
/// (any buffered body/pipelined bytes are retained in the returned head).
/// Reached both by a plain-HTTP listener and by the TLS terminate backend
/// re-intercepting a decrypted stream as HTTP.
pub struct HeadIntercept<S> {
    stream: ConnStream<S>,
    timeout: Duration,
    max_size: usize,
}

impl<S> HeadIntercept<S> {
    pub fn new(stream: ConnStream<S>, timeout: Duration) -> Self {
        Self {
            stream,
            timeout,
            max_size: http_header::DEFAULT_MAX_HTTP_HEADER_SIZE,
        }
    }
}

impl<S> Intercept for HeadIntercept<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    type Artifact = http_header::HttpHead;
    type Stream = ConnStream<S>;

    async fn intercept(mut self) -> Result<Intercepted<http_header::HttpHead, ConnStream<S>>> {
        let artifact =
            http_header::read_http_head(&mut self.stream, self.timeout, self.max_size).await?;
        Ok(Intercepted {
            artifact,
            stream: self.stream,
        })
    }
}

pub(crate) async fn redirect_https<S>(
    mut client: ConnStream<S>,
    head: http_header::HttpHead,
    config: Option<&crate::runtime_config::HttpRedirectConfig>,
    default_port: u16,
) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let status = config.map_or(308, |value| value.status);
    let hostname = config.and_then(|value| value.hostname.as_deref()).unwrap_or(&head.host);
    let port = config.and_then(|value| value.port).unwrap_or(default_port);
    let authority = if hostname.contains(':') { format!("[{hostname}]") } else { hostname.to_string() };
    let port = if port == 443 { String::new() } else { format!(":{port}") };
    let location = format!("https://{authority}{port}{}", head.target);
    let reason = if status == 301 { "Moved Permanently" } else { "Permanent Redirect" };
    let response = format!("HTTP/1.1 {status} {reason}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await?;
    Ok(())
}

async fn require_basic_auth<S>(client: &mut ConnStream<S>, head: &mut http_header::HttpHead, auth: &crate::runtime_config::HttpBasicAuth) -> Result<bool>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    if !auth.enabled { return Ok(true); }
    let supplied = head.authorization.as_deref().and_then(|value| value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")))
        .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value.trim()).ok());
    let allowed = supplied.as_deref().is_some_and(|candidate| auth.users.iter().any(|user| {
        let expected = format!("{}:{}", user.username, user.password);
        candidate.len() == expected.len() && bool::from(candidate.ct_eq(expected.as_bytes()))
    }));
    if allowed { head.consume_authorization(); return Ok(true); }
    let body = error_pages::render(401, "Unauthorized", error_pages::default_detail(401));
    let header = format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"tlsproxy\", charset=\"UTF-8\"\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(header.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    client.shutdown().await?;
    Ok(false)
}

async fn bad_gateway<S>(client: &mut ConnStream<S>) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    static_files::error_response(client, 502, false).await
}

async fn bad_request<S>(client: &mut ConnStream<S>) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    static_files::error_response(client, 400, false).await
}

pub(crate) async fn run<S>(
    ctx: crate::dataplane::ConnCtx,
    policy: Arc<RelayPolicy>,
    mut client: ConnStream<S>,
    inspected: Option<http_header::HttpHead>,
    route: Option<(String, crate::runtime_config::HttpRouteAction)>,
    client_tls: bool,
    expected_sni: Option<String>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let crate::dataplane::ConnCtx { name, remote: remote_address, stats, controller } = ctx;
    let conn_id = client.request_id();
    info!("{conn_id} {name} http worker started");
    let mut head = match inspected {
        Some(head) => head,
        None => http_header::read_http_head(
            &mut client,
            Duration::from_secs(10),
            http_header::DEFAULT_MAX_HTTP_HEADER_SIZE,
        )
        .await?,
    };
    let header_len = admit(&conn_id, &name, &head, expected_sni.as_deref())?;
    stats.increase_uploaded_bytes(header_len);
    active_tracker::add_uploaded(&conn_id, header_len as u64);

    // Path routing may finish the request locally (static files, 401, 404)
    // or refine the route to a per-path backend pool.
    let mut route = route;
    if let Some((_, host_action)) = &route {
        if let Some(path_route) = host_action.select_path(&head.target).cloned() {
            if !require_basic_auth(&mut client, &mut head, &path_route.basic_auth).await? { return Ok(()); }
            match path_route.action {
                crate::runtime_config::HttpPathAction::StaticFiles { document_root, index, directory_listing } => {
                    return crate::dataplane::http::static_files::serve(client, head, &path_route.prefix, &document_root, index.as_deref(), directory_listing).await;
                }
                crate::runtime_config::HttpPathAction::ReverseProxy { action } => {
                    if path_route.strip_prefix { head.strip_path_prefix(&path_route.prefix); }
                    if let Some((key, _)) = &route { route = Some((format!("{key}:{}", path_route.prefix), action)); }
                }
            }
        } else if !host_action.paths.is_empty() && host_action.backends.is_empty() && host_action.target.is_none() {
            return crate::dataplane::http::static_files::not_found(client, head.method == "HEAD").await;
        }
    }

    // Only the first request's head has passed authentication and path
    // routing, so only that request's body may be relayed. Determining the
    // body's wire framing up front lets the upload side end exactly at the
    // message boundary; pipelined bytes beyond it are never forwarded.
    let framing = match head.body_framing() {
        Ok(framing) => framing,
        Err(cause) => {
            log::warn!("{conn_id} rejecting request with ambiguous body framing: {cause:#}");
            bad_request(&mut client).await?;
            return Ok(());
        }
    };
    let (selected, upstream_tls, host_header) =
        match select_backend(&route, &head, remote_address.ip(), &name, &policy).await {
            Ok(selected) => selected,
            Err(cause) => {
                log::warn!("{conn_id} reverse-proxy backend unavailable: {cause:#}");
                bad_gateway(&mut client).await?;
                return Ok(());
            }
        };
    active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint);
    crate::relay::reject_obvious_self_connect(&policy, &selected.endpoint, &conn_id).await?;
    let upstream = match connect_backend(&conn_id, &selected.endpoint).await {
        Some(upstream) => upstream,
        None => {
            bad_gateway(&mut client).await?;
            return Ok(());
        }
    };
    info!("{conn_id} connected to http upstream {}", selected.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok);

    let relay_ctx = crate::relay::RelayContext {
        id: conn_id,
        policy,
        stats,
        controller,
        initial_uploaded: header_len as u64,
    };
    let plan = ForwardPlan {
        head,
        framing,
        scheme: if client_tls { "https" } else { "http" },
        client_ip: remote_address.ip(),
        host_header,
        upstream_tls,
        tls_server_name: selected.tls_server_name,
    };
    exchange(relay_ctx, client, upstream, plan).await
}

/// Validates a parsed request head for proxying: no self-connection loop and
/// (behind TLS) a Host that matches the routed SNI. Returns the head's wire
/// length for upload accounting; buffered body-prefix bytes are counted by
/// the relay pipe when the framing-bounded reader replays them.
fn admit(
    conn_id: &crate::request_id::RequestId,
    name: &str,
    head: &http_header::HttpHead,
    expected_sni: Option<&str>,
) -> Result<usize> {
    if head.loop_tokens.iter().any(|token| crate::hello_cache::request_token_is_looped(token)) {
        log::warn!("{conn_id} {name} inbound request carries a loop token this proxy recently forwarded; closing self-connection loop");
        return Err(anyhow!("detected self-connection loop"));
    }
    if expected_sni.is_some_and(|sni| !sni.trim_end_matches('.').eq_ignore_ascii_case(head.host.trim_end_matches('.'))) {
        return Err(anyhow!("HTTPS SNI `{}` does not match HTTP Host `{}`", expected_sni.unwrap_or_default(), head.host));
    }
    info!("{conn_id} http host is {}", head.host_raw);
    active_tracker::set_sni(conn_id, &head.host);
    Ok(head.buffered.len() - head.body_prefix().len())
}

/// Picks the upstream for this request: the routed per-path backend pool, the
/// listener's configured backends, or dynamic Host-header resolution.
async fn select_backend(
    route: &Option<(String, crate::runtime_config::HttpRouteAction)>,
    head: &http_header::HttpHead,
    client_ip: std::net::IpAddr,
    listener_name: &str,
    policy: &RelayPolicy,
) -> Result<(crate::forward::SelectedTarget, bool, Option<String>)> {
    if let Some((route_key, action)) = route {
        let (selected, tls) = crate::forward::select_http_backend(route_key, &head.host, client_ip, action).await?;
        return Ok((selected, tls, action.host_header.clone()));
    }
    if crate::forward::http_listener_targets(policy).is_some() {
        let selected = crate::forward::choose_online(listener_name, client_ip, crate::runtime_config::HttpLoadBalancing::RoundRobin)
            .await
            .ok_or_else(|| anyhow!("no online http backends"))?;
        return Ok((selected, policy.upstream_tls, None));
    }
    let port = head.port.unwrap_or(policy.target_port);
    let selected = crate::forward::select_runtime_target(&head.host, port, false, &head.host).await?;
    Ok((selected, false, None))
}

/// Connects to the chosen backend with a bounded timeout, logging failures;
/// the caller answers the client with 502 when this returns `None`.
async fn connect_backend(conn_id: &crate::request_id::RequestId, endpoint: &str) -> Option<TcpStream> {
    match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(endpoint)).await {
        Ok(Ok(upstream)) => Some(upstream),
        Ok(Err(cause)) => {
            log::warn!("{conn_id} failed to connect to reverse-proxy backend {endpoint}: {cause}");
            None
        }
        Err(_) => {
            log::warn!("{conn_id} timed out connecting to reverse-proxy backend {endpoint}");
            None
        }
    }
}

/// Everything decided about a request before its bytes start moving.
struct ForwardPlan {
    head: http_header::HttpHead,
    framing: http_header::BodyFraming,
    scheme: &'static str,
    client_ip: std::net::IpAddr,
    host_header: Option<String>,
    upstream_tls: bool,
    tls_server_name: String,
}

/// Forwards the rewritten request and relays the response according to the
/// plan: a `101 Switching Protocols` answer to an Upgrade request becomes an
/// unbounded bidirectional tunnel, anything else the framing-bounded relay.
async fn exchange<S>(
    relay_ctx: crate::relay::RelayContext,
    mut client: ConnStream<S>,
    upstream: TcpStream,
    plan: ForwardPlan,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ForwardPlan { head, framing, scheme, client_ip, host_header, upstream_tls, tls_server_name } = plan;
    let upgrade = head.upgrade_requested();
    let prefix = head.body_prefix().to_vec();
    let mut rewritten = head.rewrite_for_proxy(client_ip, scheme, host_header.as_deref(), &crate::hello_cache::mint_request_token());
    // rewrite_for_proxy appends the buffered body prefix; the body is instead
    // restored onto the stream and delivered through the framing-bounded
    // reader below.
    rewritten.truncate(rewritten.len() - prefix.len());
    client.unread(&prefix);
    let (base_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write): (Box<dyn AsyncRead + Send + Unpin>, Box<dyn AsyncWrite + Send + Unpin>) = if upstream_tls {
        let upstream = tokio::time::timeout(std::time::Duration::from_secs(10), connect_trust_all_tls(upstream, &tls_server_name)).await.map_err(|_| anyhow!("upstream TLS handshake timed out"))??;
        let (read, write) = tokio::io::split(upstream);
        (Box::new(read), Box::new(write))
    } else {
        let (read, write) = tokio::io::split(upstream);
        (Box::new(read), Box::new(write))
    };
    upstream_write.write_all(&rewritten).await?;

    if upgrade {
        // An Upgrade request (for example WebSocket) is decided by the
        // upstream's response head: `101 Switching Protocols` turns the
        // connection into an unbounded bidirectional tunnel. Any other
        // status keeps the normal framing-bounded relay so a refused
        // upgrade cannot be used to push further, unrouted requests.
        let response_head = match read_response_head(&mut upstream_read).await {
            Ok(head) => head,
            Err(cause) => {
                log::warn!("{} upstream upgrade response failed: {cause:#}", relay_ctx.id);
                bad_gateway_split(&mut client_write).await?;
                return Ok(());
            }
        };
        relay_ctx.stats.increase_downloaded_bytes(response_head.len());
        active_tracker::add_downloaded(&relay_ctx.id, response_head.len() as u64);
        client_write.write_all(&response_head).await?;
        if response_status(&response_head) == Some(101) {
            info!("{} upgrade accepted by upstream; relaying as a bidirectional tunnel", relay_ctx.id);
            return crate::relay::relay(relay_ctx, base_read, client_write, upstream_read, upstream_write).await;
        }
    }

    let client_read: Box<dyn AsyncRead + Send + Unpin> = match framing {
        crate::http_header::BodyFraming::Length(length) => {
            Box::new(tokio::io::AsyncReadExt::take(base_read, length))
        }
        crate::http_header::BodyFraming::Chunked => {
            Box::new(crate::http_header::ChunkedBodyReader::new(base_read))
        }
    };
    // The framing-bounded reader reaches EOF the moment the request body is
    // complete — while the client socket is still open. That EOF must not
    // become a TCP FIN/close_notify to the upstream: servers without HTTP/1.1
    // half-close support (hyper's default, so most Rust backends) treat it as
    // a client abort and drop the connection without responding. The request
    // is already delimited by its framing plus the `Connection: close` the
    // rewrite always sends, so the upstream needs no FIN to answer.
    let upstream_write = KeepWriteOpen(upstream_write);
    crate::relay::relay(relay_ctx, client_read, client_write, upstream_read, upstream_write).await
}

/// Suppresses `shutdown` on the wrapped writer, downgrading it to a flush.
/// Used for the framed request relay above; the tunnel and raw relays keep
/// real FIN propagation because their EOFs come from actual peer closes.
struct KeepWriteOpen<W>(W);

impl<W: AsyncWrite + Unpin> AsyncWrite for KeepWriteOpen<W> {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
}

/// Reads an HTTP response head (through the blank line) from the upstream,
/// returning every byte read — including any body bytes that arrived in the
/// same segments — so the caller can forward them verbatim.
async fn read_response_head<R: AsyncRead + Send + Unpin>(upstream: &mut R) -> Result<Vec<u8>> {
    const MAX_RESPONSE_HEAD: usize = 64 * 1024;
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        let count = tokio::time::timeout(Duration::from_secs(30), tokio::io::AsyncReadExt::read(upstream, &mut chunk))
            .await
            .map_err(|_| anyhow!("upstream response head timed out"))??;
        if count == 0 {
            anyhow::bail!("upstream closed before sending a response head");
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(buffer);
        }
        if buffer.len() > MAX_RESPONSE_HEAD {
            anyhow::bail!("upstream response head exceeds {MAX_RESPONSE_HEAD} bytes");
        }
    }
}

fn response_status(head: &[u8]) -> Option<u16> {
    let first_line = head.split(|byte| *byte == b'\n').next()?;
    let text = std::str::from_utf8(first_line).ok()?;
    if !text.starts_with("HTTP/1.") { return None; }
    text.split_ascii_whitespace().nth(1)?.parse().ok()
}

async fn bad_gateway_split<W: AsyncWrite + Unpin>(client: &mut W) -> Result<()> {
    static_files::error_response(client, 502, false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn upgrade_response_head_is_read_through_blank_line_with_early_body_bytes() {
        let (mut upstream, mut proxy_side) = tokio::io::duplex(2048);
        upstream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n\x81\x05hello")
            .await
            .unwrap();
        let head = read_response_head(&mut proxy_side).await.unwrap();
        assert_eq!(response_status(&head), Some(101));
        // Bytes past the blank line (an early WebSocket frame) ride along.
        assert!(head.ends_with(b"\x81\x05hello"));

        let (mut upstream, mut proxy_side) = tokio::io::duplex(2048);
        upstream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        let head = read_response_head(&mut proxy_side).await.unwrap();
        assert_eq!(response_status(&head), Some(400));
        assert_eq!(response_status(b"garbage\r\n\r\n"), None);
    }

    #[tokio::test]
    async fn framed_relay_never_half_closes_upstream_before_the_response() {
        // Mimics servers without HTTP/1.1 half-close support (hyper's
        // default, e.g. rustfs): after reading the request they wait briefly,
        // and if the client's FIN arrives before the response was written the
        // connection is dropped without responding.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 4096];
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "upstream saw EOF before the request completed");
                request.extend_from_slice(&buffer[..count]);
            }
            // A FIN arriving now is exactly the pre-fix proxy behavior.
            match tokio::time::timeout(Duration::from_millis(500), socket.read(&mut buffer)).await {
                Ok(Ok(0)) => return, // half-closed: abort without responding
                Ok(Ok(_)) | Ok(Err(_)) => panic!("unexpected extra request bytes"),
                Err(_) => {} // no FIN: answer normally
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let (mut browser, server) = tokio::io::duplex(4096);
        browser.write_all(b"GET / HTTP/1.1\r\nHost: h.example\r\n\r\n").await.unwrap();
        let client = ConnStream::of(server);
        let Intercepted { artifact: head, stream: client } =
            HeadIntercept::new(client, Duration::from_secs(1)).intercept().await.unwrap();
        let framing = head.body_framing().unwrap();
        let upstream = TcpStream::connect(endpoint).await.unwrap();
        let relay_ctx = crate::relay::RelayContext {
            id: Arc::new(crate::request_id::RequestId::new()),
            policy: Arc::new(RelayPolicy { bind: "127.0.0.1:443".into(), target: None, target_port: 80, speed_limit: None, upstream_tls: false }),
            stats: Arc::new(crate::listener_stats::ListenerStats::new("test", 5_000)),
            controller: Arc::new(tokio::sync::RwLock::new(crate::controller::Controller::new())),
            initial_uploaded: 0,
        };
        let plan = ForwardPlan {
            head,
            framing,
            scheme: "http",
            client_ip: "127.0.0.1".parse().unwrap(),
            host_header: None,
            upstream_tls: false,
            tls_server_name: String::new(),
        };
        let exchange_task = tokio::spawn(exchange(relay_ctx, client, upstream, plan));

        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut browser, &mut response).await.unwrap();
        upstream_task.await.unwrap();
        exchange_task.await.unwrap().unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "upstream dropped the request; got: {response:?}");
        assert!(response.ends_with("ok"));
    }

    #[tokio::test]
    async fn head_intercept_yields_head_and_keeps_body_prefix() {
        let (mut browser, server) = tokio::io::duplex(2048);
        browser
            .write_all(b"POST /submit HTTP/1.1\r\nHost: h.example\r\nContent-Length: 4\r\n\r\nBODY")
            .await
            .unwrap();
        let client = ConnStream::of(server);
        let Intercepted { artifact: head, stream: _client } =
            HeadIntercept::new(client, Duration::from_secs(1)).intercept().await.unwrap();
        assert_eq!(head.host, "h.example");
        assert_eq!(head.method, "POST");
        // The body that arrived with the head rides along in the artifact; the
        // continuation stream carries anything sent afterwards.
        assert_eq!(head.body_prefix(), b"BODY");
    }

    #[tokio::test]
    async fn https_redirect_preserves_path_query_and_omits_standard_port() {
        let (mut browser, mut server) = tokio::io::duplex(2048);
        browser.write_all(b"GET /app?q=1 HTTP/1.1\r\nHost: example.com\r\n\r\n").await.unwrap();
        let head = http_header::read_http_head(&mut server, Duration::from_secs(1), 2048).await.unwrap();
        redirect_https(ConnStream::of(server), head, None, 443).await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("HTTP/1.1 308 Permanent Redirect"));
        assert!(response.contains("Location: https://example.com/app?q=1\r\n"));
    }

    #[tokio::test]
    async fn https_redirect_applies_hostname_and_port_override() {
        let (mut browser, mut server) = tokio::io::duplex(2048);
        browser.write_all(b"GET /old HTTP/1.1\r\nHost: old.example\r\n\r\n").await.unwrap();
        let head = http_header::read_http_head(&mut server, Duration::from_secs(1), 2048).await.unwrap();
        let redirect = crate::runtime_config::HttpRedirectConfig { hostname: Some("new.example".into()), port: Some(8443), status: 301, preserve_host: false };
        redirect_https(ConnStream::of(server), head, Some(&redirect), 443).await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("HTTP/1.1 301 Moved Permanently"));
        assert!(response.contains("Location: https://new.example:8443/old\r\n"));
    }

    #[tokio::test]
    async fn basic_auth_accepts_matching_pair_and_challenges_bad_credentials() {
        let auth = crate::runtime_config::HttpBasicAuth { enabled: true, users: vec![crate::runtime_config::HttpBasicAuthUser { username: "alice".into(), password: "secret".into() }] };
        let (mut browser, mut server) = tokio::io::duplex(2048);
        browser.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nAuthorization: Basic YWxpY2U6c2VjcmV0\r\n\r\n").await.unwrap();
        let head = http_header::read_http_head(&mut server, Duration::from_secs(1), 2048).await.unwrap();
        let mut head = head;
        assert!(require_basic_auth(&mut ConnStream::of(server), &mut head, &auth).await.unwrap());
        assert!(!String::from_utf8(head.rewrite_for_proxy("127.0.0.1".parse().unwrap(), "http", None, "cafe")).unwrap().to_ascii_lowercase().contains("authorization:"));

        let (mut browser, mut server) = tokio::io::duplex(2048);
        browser.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nAuthorization: Basic YWxpY2U6d3Jvbmc=\r\n\r\n").await.unwrap();
        let head = http_header::read_http_head(&mut server, Duration::from_secs(1), 2048).await.unwrap();
        let mut head = head;
        assert!(!require_basic_auth(&mut ConnStream::of(server), &mut head, &auth).await.unwrap());
        let mut response = String::new(); browser.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
        assert!(response.contains("WWW-Authenticate: Basic"));
    }

    #[tokio::test]
    async fn disabled_basic_auth_preserves_authorization_for_upstream() {
        let auth = crate::runtime_config::HttpBasicAuth::default();
        let (mut browser, mut server) = tokio::io::duplex(2048);
        browser.write_all(b"GET /processmaster/ HTTP/1.1\r\nHost: public.example\r\nAuthorization: Basic YWxpY2U6c2VjcmV0\r\n\r\n").await.unwrap();
        let mut head = http_header::read_http_head(&mut server, Duration::from_secs(1), 2048).await.unwrap();
        assert!(require_basic_auth(&mut ConnStream::of(server), &mut head, &auth).await.unwrap());
        let rewritten = String::from_utf8(head.rewrite_for_proxy("127.0.0.1".parse().unwrap(), "https", None, "cafe")).unwrap();
        assert!(rewritten.contains("GET /processmaster/ HTTP/1.1\r\n"));
        assert!(rewritten.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
        assert!(rewritten.contains("Host: public.example\r\n"));

        head.strip_path_prefix("/processmaster/");
        let stripped = String::from_utf8(head.rewrite_for_proxy("127.0.0.1".parse().unwrap(), "https", Some("localhost:9001"), "cafe")).unwrap();
        assert!(stripped.contains("GET / HTTP/1.1\r\n"));
        assert!(stripped.contains("Host: localhost:9001\r\n"));
        assert!(stripped.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
    }
}
