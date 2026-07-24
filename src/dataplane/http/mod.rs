//! HTTP interception layer: reverse-proxy host/path routing, static file
//! serving, and HTTPS redirects. Reached both by a plain-HTTP listener and by
//! the TLS terminate backend re-intercepting a decrypted stream as HTTP.

pub mod static_files;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::info;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::RwLock;
use base64::Engine;
use subtle::ConstantTimeEq;

use crate::dataplane::pipeline::{Intercept, Intercepted};

/// Interception point for an HTTP connection: its request head. The
/// continuation stream is the same connection, positioned at the message body
/// (any buffered body/pipelined bytes are retained in the returned head).
/// Reached both by a plain-HTTP listener and by the TLS terminate backend
/// re-intercepting a decrypted stream as HTTP.
pub struct HeadIntercept<S> {
    stream: Extensible<S>,
    timeout: Duration,
    max_size: usize,
}

impl<S> HeadIntercept<S> {
    pub fn new(stream: Extensible<S>, timeout: Duration) -> Self {
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
    type Stream = Extensible<S>;

    async fn intercept(mut self) -> Result<Intercepted<http_header::HttpHead, Extensible<S>>> {
        let artifact =
            http_header::read_http_head(&mut self.stream, self.timeout, self.max_size).await?;
        Ok(Intercepted {
            artifact,
            stream: self.stream,
        })
    }
}

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::config::Listener;
use crate::controller::Controller;
use crate::extensible::Extensible;
use crate::http_header;
use crate::listener_stats::ListenerStats;
use crate::upstream_tls::connect_trust_all_tls;

pub(crate) async fn redirect_https<S>(
    mut client: Extensible<S>,
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

async fn require_basic_auth<S>(client: &mut Extensible<S>, head: &mut http_header::HttpHead, auth: &crate::runtime_config::HttpBasicAuth) -> Result<bool>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    if !auth.enabled { return Ok(true); }
    let supplied = head.authorization.as_deref().and_then(|value| value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")))
        .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value.trim()).ok());
    let allowed = supplied.as_deref().is_some_and(|candidate| auth.users.iter().any(|user| {
        let expected = format!("{}:{}", user.username, user.password);
        candidate.len() == expected.len() && bool::from(candidate.ct_eq(expected.as_bytes()))
    }));
    if allowed { head.consume_authorization(); return Ok(true); }
    client.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"tlsproxy\", charset=\"UTF-8\"\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 13\r\nConnection: close\r\n\r\nUnauthorized\n").await?;
    client.shutdown().await?;
    Ok(false)
}

async fn bad_gateway<S>(client: &mut Extensible<S>) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 12\r\nConnection: close\r\n\r\nBad Gateway\n").await?;
    client.shutdown().await?;
    Ok(())
}

async fn bad_request<S>(client: &mut Extensible<S>) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    client.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 12\r\nConnection: close\r\n\r\nBad Request\n").await?;
    client.shutdown().await?;
    Ok(())
}

pub(crate) async fn run<S>(
    name: Arc<String>,
    mut client: Extensible<S>,
    remote_address: SocketAddr,
    listener_config: Arc<Listener>,
    context: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
    inspected: Option<http_header::HttpHead>,
    route: Option<(String, crate::runtime_config::HttpRouteAction)>,
    client_tls: bool,
    expected_sni: Option<String>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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
    if head.loop_tokens.iter().any(|token| crate::hello_cache::request_token_is_looped(token)) {
        log::warn!("{conn_id} {name} inbound request carries a loop token this proxy recently forwarded; closing self-connection loop");
        return Err(anyhow!("detected self-connection loop"));
    }
    // Count only the request head here; buffered body-prefix bytes are
    // counted by the relay pipe when the framing-bounded reader replays them.
    let header_len = head.buffered.len() - head.body_prefix().len();
    if expected_sni.as_deref().is_some_and(|sni| !sni.trim_end_matches('.').eq_ignore_ascii_case(head.host.trim_end_matches('.'))) {
        return Err(anyhow!("HTTPS SNI `{}` does not match HTTP Host `{}`", expected_sni.unwrap_or_default(), head.host));
    }
    info!("{conn_id} http host is {}", head.host_raw);
    active_tracker::set_sni(&conn_id, &head.host);
    context.increase_uploaded_bytes(header_len);
    active_tracker::add_uploaded(&conn_id, header_len as u64);
    if route.is_none() {
        crate::relay::check_acl(&listener_config, &head.host, &conn_id).await?;
    }
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
    // Only the first request's head has passed authentication, ACL, and path
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
    let selected_result: Result<_> = async {
        Ok(if let Some((route_key, action)) = &route {
            let (selected, tls) = crate::forward::select_http_backend(route_key, &head.host, remote_address.ip(), action).await?;
            (selected, tls, action.host_header.as_deref())
        } else if crate::forward::http_listener_targets(&listener_config).is_some() {
            (crate::forward::choose_online(&name, remote_address.ip(), crate::runtime_config::HttpLoadBalancing::RoundRobin)
                .await
                .ok_or_else(|| anyhow!("no online http backends"))?, listener_config.upstream_tls, None)
        } else {
            let port = head.port.unwrap_or(listener_config.target_port);
            (crate::forward::select_runtime_target(&head.host, port, false, &head.host).await?, false, None)
        })
    }.await;
    let (selected, upstream_tls, host_header) = match selected_result {
        Ok(selected) => selected,
        Err(cause) => {
            log::warn!("{conn_id} reverse-proxy backend unavailable: {cause:#}");
            bad_gateway(&mut client).await?;
            return Ok(());
        }
    };
    active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint);
    crate::relay::reject_obvious_self_connect(&listener_config, &selected.endpoint, &conn_id).await?;
    let upstream = match tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&selected.endpoint),
    )
    .await {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(cause)) => {
            log::warn!("{conn_id} failed to connect to reverse-proxy backend {}: {cause}", selected.endpoint);
            bad_gateway(&mut client).await?;
            return Ok(());
        }
        Err(_) => {
            log::warn!("{conn_id} timed out connecting to reverse-proxy backend {}", selected.endpoint);
            bad_gateway(&mut client).await?;
            return Ok(());
        }
    };
    info!("{conn_id} connected to http upstream {}", selected.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok);
    let (client_read, client_write) = tokio::io::split(client);
    let prefix = head.body_prefix().to_vec();
    let mut rewritten = head.rewrite_for_proxy(remote_address.ip(), if client_tls { "https" } else { "http" }, host_header);
    // rewrite_for_proxy appends the buffered body prefix; the body is instead
    // delivered through the framing-bounded reader below.
    rewritten.truncate(rewritten.len() - prefix.len());
    let body_start = std::io::Cursor::new(prefix);
    let client_read: Box<dyn AsyncRead + Send + Unpin> = match framing {
        crate::http_header::BodyFraming::Length(length) => {
            Box::new(tokio::io::AsyncReadExt::take(tokio::io::AsyncReadExt::chain(body_start, client_read), length))
        }
        crate::http_header::BodyFraming::Chunked => {
            Box::new(crate::http_header::ChunkedBodyReader::new(tokio::io::AsyncReadExt::chain(body_start, client_read)))
        }
    };
    if upstream_tls {
        let upstream = connect_trust_all_tls(upstream, &selected.tls_server_name).await?;
        let (upstream_read, mut upstream_write) = tokio::io::split(upstream);
        upstream_write.write_all(&rewritten).await?;
        crate::relay::relay(conn_id, client_read, client_write, upstream_read, upstream_write, listener_config, context, controller, header_len as u64).await
    } else {
        let (upstream_read, mut upstream_write) = tokio::io::split(upstream);
        upstream_write.write_all(&rewritten).await?;
        crate::relay::relay(conn_id, client_read, client_write, upstream_read, upstream_write, listener_config, context, controller, header_len as u64).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn head_intercept_yields_head_and_keeps_body_prefix() {
        let (mut browser, server) = tokio::io::duplex(2048);
        browser
            .write_all(b"POST /submit HTTP/1.1\r\nHost: h.example\r\nContent-Length: 4\r\n\r\nBODY")
            .await
            .unwrap();
        let client = Extensible::of(server);
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
        redirect_https(Extensible::of(server), head, None, 443).await.unwrap();
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
        redirect_https(Extensible::of(server), head, Some(&redirect), 443).await.unwrap();
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
        assert!(require_basic_auth(&mut Extensible::of(server), &mut head, &auth).await.unwrap());
        assert!(!String::from_utf8(head.rewrite_for_proxy("127.0.0.1".parse().unwrap(), "http", None)).unwrap().to_ascii_lowercase().contains("authorization:"));

        let (mut browser, mut server) = tokio::io::duplex(2048);
        browser.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nAuthorization: Basic YWxpY2U6d3Jvbmc=\r\n\r\n").await.unwrap();
        let head = http_header::read_http_head(&mut server, Duration::from_secs(1), 2048).await.unwrap();
        let mut head = head;
        assert!(!require_basic_auth(&mut Extensible::of(server), &mut head, &auth).await.unwrap());
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
        assert!(require_basic_auth(&mut Extensible::of(server), &mut head, &auth).await.unwrap());
        let rewritten = String::from_utf8(head.rewrite_for_proxy("127.0.0.1".parse().unwrap(), "https", None)).unwrap();
        assert!(rewritten.contains("GET /processmaster/ HTTP/1.1\r\n"));
        assert!(rewritten.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
        assert!(rewritten.contains("Host: public.example\r\n"));

        head.strip_path_prefix("/processmaster/");
        let stripped = String::from_utf8(head.rewrite_for_proxy("127.0.0.1".parse().unwrap(), "https", Some("localhost:9001"))).unwrap();
        assert!(stripped.contains("GET / HTTP/1.1\r\n"));
        assert!(stripped.contains("Host: localhost:9001\r\n"));
        assert!(stripped.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
    }
}
