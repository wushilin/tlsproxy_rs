//! TLS termination data-plane handler.
//!
//! Certificate selection and upstream relay live here; default-listener SNI
//! routing and ACME interception remain in `listener::default`.

use std::sync::Arc;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::info;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::ca::LocalCa;
use crate::dataplane::pipeline::Intercept;
use crate::dataplane::RelayPolicy;
use crate::controller::Controller;
use crate::conn_stream::ConnStream;
use crate::listener_stats::ListenerStats;
use crate::request_id::RequestId;
use crate::upstream_tls::connect_trust_all_tls;
use crate::tls_header::ClientHello;

/// Terminates a connection whose ClientHello was consumed by the mandatory
/// listener dispatcher. The buffered bytes are replayed into rustls and the
/// already-selected route controls both destination and upstream TLS mode.
pub(crate) async fn run_inspected(
    ctx: crate::dataplane::ConnCtx,
    listener_config: Arc<RelayPolicy>,
    ca: LocalCa,
    client: ConnStream<TcpStream>,
    hello: ClientHello,
    target: Option<String>,
    target_port: u16,
    upstream_tls: bool,
    load_balancing: crate::runtime_config::HttpLoadBalancing,
    certified_key: Arc<rustls::sign::CertifiedKey>,
) -> Result<()> {
    let crate::dataplane::ConnCtx { name, stats: context, controller, remote } = ctx;
    let conn_id = client.request_id();
    let expected_sni = hello.sni_host.clone();
    let mut client = client;
    // Restore the peeked ClientHello so the TLS acceptor sees the pristine
    // wire stream.
    client.unread(hello.buffered);
    run_stream(
        name,
        client,
        conn_id,
        listener_config,
        context,
        controller,
        ca,
        Some((expected_sni, target, target_port, upstream_tls, load_balancing, remote.ip())),
        Some(certified_key),
    )
    .await
}

/// Terminates TLS for a ReverseProxy route and re-intercepts the decrypted
/// stream as HTTP. The dispatcher resolves the certificate and hands the
/// pristine wire stream over; this function owns the handshake and the
/// protocol handoff.
pub(crate) async fn run_reverse_proxy(
    ctx: crate::dataplane::ConnCtx,
    policy: Arc<RelayPolicy>,
    mut client: ConnStream<TcpStream>,
    hello: crate::tls_header::ClientHello,
    certified_key: Arc<rustls::sign::CertifiedKey>,
    action: crate::runtime_config::HttpRouteAction,
) -> Result<()> {
    let sni = hello.sni_host.clone();
    let request_id = client.request_id();
    // Restore the peeked ClientHello so the TLS acceptor sees the pristine
    // wire stream.
    client.unread(hello.buffered);
    let acceptor = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), client);
    let start = tokio::time::timeout(Duration::from_secs(5), acceptor).await??;
    let mut server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls_stream = tokio::time::timeout(Duration::from_secs(5), start.into_stream(Arc::new(server))).await??;
    // Preserve the original connection id across the TLS boundary onto the
    // fresh decrypted stream.
    let client = ConnStream::with_request_id(tls_stream, request_id);
    // The decrypted stream is re-intercepted as an HTTP connection: same
    // pipeline shape, next protocol layer.
    let crate::dataplane::pipeline::Intercepted { artifact: head, stream: client } =
        crate::dataplane::http::HeadIntercept::new(client, Duration::from_secs(10))
            .intercept()
            .await?;
    let route_key = format!("{}:{}", ctx.name, sni.to_ascii_lowercase());
    crate::dataplane::http::run(ctx, policy, client, Some(head), Some((route_key, action)), true, Some(sni)).await
}

async fn run_stream<S>(
    name: Arc<String>,
    client: S,
    conn_id: Arc<RequestId>,
    listener_config: Arc<RelayPolicy>,
    context: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
    ca: LocalCa,
    route: Option<(String, Option<String>, u16, bool, crate::runtime_config::HttpLoadBalancing, IpAddr)>,
    certified_key: Option<Arc<rustls::sign::CertifiedKey>>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    info!("{conn_id} {name} terminating TLS worker started");
    let acceptor =
        tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), client);
    let start = tokio::time::timeout(Duration::from_secs(5), acceptor)
        .await
        .map_err(|_| anyhow!("TLS ClientHello timed out"))??;
    let sni_target = start
        .client_hello()
        .server_name()
        .ok_or_else(|| anyhow!("TLS client did not provide SNI"))?
        .to_string();
    if route
        .as_ref()
        .is_some_and(|(expected, ..)| expected != &sni_target)
    {
        return Err(anyhow!("TLS SNI changed between inspection and handshake"));
    }
    info!("{conn_id} received TLS ClientHello for SNI target {sni_target}");
    active_tracker::set_sni(&conn_id, &sni_target);
    let upstream_tls = route
        .as_ref()
        .map_or(listener_config.upstream_tls, |(_, _, _, tls, _, _)| *tls);
    let selected = match route.as_ref() {
        Some((_, target, target_port, _, load_balancing, client_ip)) => crate::forward::select_routed_pool(
            &name,
            &sni_target,
            target.as_deref(),
            *target_port,
            upstream_tls,
            *client_ip,
            *load_balancing,
        )
        .await?,
        None => crate::relay::resolve_target(
            &listener_config,
            &sni_target,
            upstream_tls,
            &sni_target,
            &conn_id,
        )
        .await?,
    };
    active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint);
    let certified_key = match certified_key {
        Some(key) => key,
        None => ca.resolve_or_mint(&sni_target)?,
    };
    let server_config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                certified_key,
            ))),
    );
    let tls_stream = tokio::time::timeout(
        Duration::from_secs(5),
        start.into_stream(server_config),
    )
    .await
    .map_err(|_| anyhow!("TLS handshake timed out"))??;
    if upstream_tls {
        crate::relay::reject_obvious_self_connect(&listener_config, &selected.endpoint, &conn_id)
            .await?;
    }
    let upstream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&selected.endpoint),
    )
    .await??;
    info!("{conn_id} connected to upstream {}", selected.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok);
    let (client_read, client_write) = tokio::io::split(tls_stream);
    if upstream_tls {
        let upstream = connect_trust_all_tls(upstream, &sni_target).await?;
        info!(
            "{conn_id} wrapped upstream {} in trust-all TLS",
            selected.endpoint
        );
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        crate::relay::relay(
            crate::relay::RelayContext { id: conn_id, policy: listener_config, stats: context, controller, initial_uploaded: 0 },
            client_read,
            client_write,
            upstream_read,
            upstream_write,
        )
        .await
    } else {
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        crate::relay::relay(
            crate::relay::RelayContext { id: conn_id, policy: listener_config, stats: context, controller, initial_uploaded: 0 },
            client_read,
            client_write,
            upstream_read,
            upstream_write,
        )
        .await
    }
}
