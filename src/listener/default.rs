use std::sync::Arc;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use log::{info, warn};

use crate::acme_challenge::TLS_ALPN_PROTOCOL;
use crate::ca::LocalCa;
use crate::config::{Listener, ListenerMode, Policy, Rules};
use crate::controller::Controller;
use crate::extensible::Extensible;
use crate::listener_stats::ListenerStats;
use crate::runtime_config::{DefaultListenerConfig, TlsRouteAction, UpstreamTransport};
use crate::store::normalize_domain;
use crate::tls_header::ClientHello;
use crate::request_id::RequestId;

use super::{DefaultRoute, RejectReason};

#[derive(Debug, Clone)]
pub enum ConnectionRoute {
    AcmeChallenge { domain: String },
    ControlPlane { hostname: String },
    Ordinary { sni: String, action: TlsRouteAction },
    Reject(RejectReason),
}

pub type ControlFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Adapter implemented by the RocksDB/form-login admin service. It receives
/// the original buffered hello and socket, and must complete TLS locally.
pub trait ControlPlaneService: Send + Sync + 'static {
    fn serve<'a>(
        &'a self,
        hostname: String,
        hello: ClientHello,
        client: Extensible<TcpStream>,
    ) -> ControlFuture<'a>;
}

/// Runs the already-bound mandatory listener. Binding is kept outside this
/// function so startup can report bind failures synchronously and tests can
/// use an ephemeral port without a find-then-bind race.
pub async fn run(
    listener: tokio::net::TcpListener,
    _config: Arc<DefaultListenerConfig>,
    control_hostname: Option<String>,
    stats: Arc<ListenerStats>,
    mut listener_controller: Controller,
    ca: LocalCa,
    control_service: Arc<dyn ControlPlaneService>,
    certificate_cache: crate::managed_tls::ManagedCertificateCache,
    _certificate_fallback: crate::runtime_config::CertificateFallbackPolicy,
) -> Result<()> {
    let name = Arc::new(crate::runtime_config::DEFAULT_LISTENER_NAME.to_owned());
    loop {
        let (socket, remote_address) = listener.accept().await?;
        let client = Extensible::of(socket);
        client.extend(RequestId::new()).await;
        let connection_controller = Arc::new(RwLock::new(listener_controller.child()));
        let task_name = Arc::clone(&name);
        let live = crate::runtime_live::load();
        let task_config = Arc::new(live.default_listener.clone());
        stats.set_idle_timeout_ms(task_config.ordinary_traffic.max_idle_time_ms.unwrap_or(u64::MAX));
        let task_stats = Arc::clone(&stats);
        let task_ca = ca.clone();
        let task_control = Arc::clone(&control_service);
        let task_control_hostname = if live.control_plane.hostname.is_empty() { control_hostname.clone() } else { Some(live.control_plane.hostname.clone()) };
        let task_fallback = live.certificate_fallback;
        let task_certificate_cache = certificate_cache.clone();
        drop(listener_controller.spawn(async move {
            handle_connection(
                task_name,
                client,
                remote_address,
                task_config,
                task_control_hostname,
                task_stats,
                connection_controller,
                task_ca,
                task_control,
                task_certificate_cache,
                task_fallback,
            )
            .await;
        }));
    }
}

async fn handle_connection(
    name: Arc<String>,
    mut client: Extensible<TcpStream>,
    remote_address: SocketAddr,
    config: Arc<DefaultListenerConfig>,
    control_hostname: Option<String>,
    stats: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
    ca: LocalCa,
    control_service: Arc<dyn ControlPlaneService>,
    certificate_cache: crate::managed_tls::ManagedCertificateCache,
    certificate_fallback: crate::runtime_config::CertificateFallbackPolicy,
) {
    let request_id = client
        .get_extension::<RequestId>()
        .await
        .expect("default listener installs request ID");
    // The guard owns tracking, active counts, and the CDR for this connection;
    // it cleans up in Drop even if this task is force cancelled.
    let guard = crate::dataplane::ConnGuard::start(
        request_id.clone(),
        name.clone(),
        remote_address,
        stats.clone(),
        crate::accounting::ListenerType::TlsPassthrough,
    );
    info!("{request_id} ({name}) default-listener connection from {remote_address}");

    let result: Result<()> = async {
        let hello = crate::tls_header::read_client_hello(
            &mut client,
            Duration::from_secs(5),
            crate::tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
        )
        .await?;
        crate::active_tracker::set_sni(&request_id, &hello.sni_host);
        let route = decide(
            &hello,
            &config,
            control_hostname.as_deref(),
            |domain| crate::acme_challenge::global().resolve(domain).is_some(),
        );
        info!("{request_id} default-listener route selected: {route:?}");
        let listener_type = match &route {
            ConnectionRoute::ControlPlane { .. }
            | ConnectionRoute::AcmeChallenge { .. }
            | ConnectionRoute::Ordinary {
                action: TlsRouteAction::Terminate { .. } | TlsRouteAction::ReverseProxy { .. },
                ..
            } => crate::accounting::ListenerType::TlsTerminate,
            _ => crate::accounting::ListenerType::TlsPassthrough,
        };
        crate::active_tracker::set_listener_type(&request_id, listener_type);
        match route {
            ConnectionRoute::ControlPlane { hostname } => {
                control_service.serve(hostname, hello, client).await?;
                crate::active_tracker::set_status(&request_id, crate::accounting::ConnStatus::Ok);
            }
            route => {
                let is_acme = matches!(&route, ConnectionRoute::AcmeChallenge { .. });
                dispatch_non_control(
                    route,
                    hello,
                    client,
                    remote_address,
                    name.clone(),
                    &config,
                    stats.clone(),
                    controller,
                    ca,
                    certificate_cache,
                    certificate_fallback,
                )
                .await?;
                if is_acme {
                    crate::active_tracker::set_status(&request_id, crate::accounting::ConnStatus::Ok);
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(cause) = &result {
        warn!("{request_id} default-listener connection failed: {cause:#}");
    }
    info!(
        "{request_id} default-listener connection closed: elapsed_ms={}",
        guard.elapsed().as_millis()
    );
}

/// Complete default-listener decision based on one parsed ClientHello. This
/// layer owns ACME/control precedence; protocol handlers receive only the
/// resulting ordinary action and cannot reinterpret system traffic.
pub fn decide(
    hello: &ClientHello,
    config: &DefaultListenerConfig,
    control_hostname: Option<&str>,
    challenge_is_active: impl FnOnce(&str) -> bool,
) -> ConnectionRoute {
    match classify(
        Some(&hello.sni_host),
        &hello.alpn_protocols,
        control_hostname,
        challenge_is_active,
        |host| config.ordinary_traffic.select_route(host).is_some(),
    ) {
        DefaultRoute::AcmeChallenge { domain } => ConnectionRoute::AcmeChallenge { domain },
        DefaultRoute::ControlPlane { hostname } => ConnectionRoute::ControlPlane { hostname },
        DefaultRoute::Proxy { sni } => {
            let action = config
                .ordinary_traffic
                .select_route(&sni)
                .expect("classifier only returns Proxy for an available route")
                .clone();
            ConnectionRoute::Ordinary { sni, action }
        }
        DefaultRoute::Reject(reason) => ConnectionRoute::Reject(reason),
    }
}

/// Executes every non-control branch selected by [`decide`]. Control-plane
/// connections deliberately remain the responsibility of the admin service
/// adapter so this module cannot accidentally proxy the reserved hostname.
pub(crate) async fn dispatch_non_control(
    route: ConnectionRoute,
    hello: ClientHello,
    client: Extensible<TcpStream>,
    remote_address: SocketAddr,
    name: Arc<String>,
    config: &DefaultListenerConfig,
    context: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
    ca: LocalCa,
    certificate_cache: crate::managed_tls::ManagedCertificateCache,
    certificate_fallback: crate::runtime_config::CertificateFallbackPolicy,
) -> Result<()> {
    match route {
        ConnectionRoute::AcmeChallenge { domain } => {
            let certificate = crate::acme_challenge::global()
                .resolve(&domain)
                .context("ACME challenge expired before its TLS handshake")?;
            crate::acme_challenge::accept_buffered(hello.buffered, client, certificate).await
        }
        ConnectionRoute::Ordinary { action, .. } => {
            // Covers every route kind: a ClientHello random this proxy
            // recently sent upstream (passthrough-forwarded or originated by
            // our own TLS connector) arriving back here is a self-connection
            // loop, whether it re-enters a passthrough, terminate, or
            // reverse-proxy route.
            if crate::hello_cache::is_looped(&hello.random) {
                warn!("inbound ClientHello was recently forwarded by this proxy; closing self-connection loop");
                bail!("detected self-connection loop");
            }
            let limits = compatibility_listener(config, &action);
            match action {
                TlsRouteAction::Passthrough { target_port, target, load_balancing } => {
                    crate::listener::tls_passthrough::run(
                        name,
                        client,
                        limits,
                        context,
                        controller,
                        Some(hello),
                        Some((target, target_port, load_balancing, remote_address.ip())),
                    )
                    .await
                }
                TlsRouteAction::Terminate {
                    target_port,
                    target,
                    upstream,
                    load_balancing,
                } => {
                    crate::managed_tls::request_automatic_for_sni(&hello.sni_host);
                    let certified_key = certificate_cache
                        .resolve_with_fallback(&hello.sni_host, &ca, certificate_fallback)
                        .await?;
                    crate::listener::tls_terminate::run_inspected(
                        name,
                        client,
                        hello,
                        limits,
                        context,
                        controller,
                        ca,
                        target,
                        target_port,
                        upstream == UpstreamTransport::Tls,
                        load_balancing,
                        remote_address.ip(),
                        certified_key,
                    )
                    .await
                }
                TlsRouteAction::ReverseProxy { action } => {
                    crate::managed_tls::request_automatic_for_sni(&hello.sni_host);
                    let certified_key = certificate_cache
                        .resolve_with_fallback(&hello.sni_host, &ca, certificate_fallback)
                        .await?;
                    let sni = hello.sni_host.clone();
                    let request_id = client.get_extension::<RequestId>().await;
                    let replay = crate::acme_challenge::ReplayStream::new(hello.buffered, client);
                    let acceptor = tokio_rustls::LazyConfigAcceptor::new(
                        rustls::server::Acceptor::default(),
                        replay,
                    );
                    let start = tokio::time::timeout(Duration::from_secs(5), acceptor).await??;
                    let mut server = rustls::ServerConfig::builder()
                        .with_no_client_auth()
                        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));
                    server.alpn_protocols = vec![b"http/1.1".to_vec()];
                    let tls = tokio::time::timeout(
                        Duration::from_secs(5),
                        start.into_stream(Arc::new(server)),
                    ).await??;
                    let mut client = Extensible::of(tls);
                    if let Some(request_id) = request_id {
                        client.extend((*request_id).clone()).await;
                    }
                    let head = crate::http_header::read_http_head(
                        &mut client,
                        Duration::from_secs(10),
                        crate::http_header::DEFAULT_MAX_HTTP_HEADER_SIZE,
                    ).await?;
                    let route_key = format!("{name}:{}", sni.to_ascii_lowercase());
                    crate::listener::http_passthrough::run(
                        name, client, remote_address, limits, context, controller,
                        Some(head), Some((route_key, action)), true, Some(sni),
                    ).await
                }
                TlsRouteAction::Reject => bail!("default-listener route explicitly rejected SNI"),
            }
        }
        ConnectionRoute::Reject(reason) => bail!("default-listener rejected connection: {reason:?}"),
        ConnectionRoute::ControlPlane { .. } => {
            bail!("control-plane connection passed to ordinary dispatcher")
        }
    }
}

pub(crate) fn compatibility_listener(
    config: &DefaultListenerConfig,
    action: &TlsRouteAction,
) -> Arc<Listener> {
    let (mode, upstream_tls) = match action {
        TlsRouteAction::Passthrough { .. } => (ListenerMode::Passthrough, true),
        TlsRouteAction::Terminate { upstream, .. } => {
            (ListenerMode::Terminate, *upstream == UpstreamTransport::Tls)
        }
        TlsRouteAction::ReverseProxy { .. } => (ListenerMode::Http, false),
        TlsRouteAction::Reject => (ListenerMode::Passthrough, false),
    };
    Arc::new(Listener {
        bind: config.bind.clone(),
        target: None,
        target_port: action.target_port().unwrap_or(443),
        policy: Policy::DENY,
        rules: Rules {
            static_hosts: Vec::new(),
            patterns: Vec::new(),
        },
        max_idle_time_ms: config.ordinary_traffic.max_idle_time_ms,
        speed_limit: config.ordinary_traffic.speed_limit,
        mode,
        upstream_tls,
    })
}

/// Pure routing policy for the required public :443 listener.
///
/// Ordering is security-sensitive: ACME ALPN is never allowed to fall through
/// to a proxy handler, and the reserved control hostname bypasses ordinary ACL
/// evaluation but can never become an upstream route.
pub fn classify(
    sni: Option<&str>,
    alpn_protocols: &[Vec<u8>],
    control_hostname: Option<&str>,
    challenge_is_active: impl FnOnce(&str) -> bool,
    ordinary_sni_is_allowed: impl FnOnce(&str) -> bool,
) -> DefaultRoute {
    let Some(sni) = sni.and_then(|value| normalize_domain(value).ok()) else {
        return DefaultRoute::Reject(RejectReason::MissingSni);
    };
    let is_acme = alpn_protocols
        .iter()
        .any(|protocol| protocol.as_slice() == TLS_ALPN_PROTOCOL);
    if is_acme {
        return if challenge_is_active(&sni) {
            DefaultRoute::AcmeChallenge { domain: sni }
        } else {
            DefaultRoute::Reject(RejectReason::UnmatchedAcmeChallenge)
        };
    }
    if control_hostname
        .and_then(|value| normalize_domain(value).ok())
        .is_some_and(|control| control == sni)
    {
        return DefaultRoute::ControlPlane { hostname: sni };
    }
    if ordinary_sni_is_allowed(&sni) {
        DefaultRoute::Proxy { sni }
    } else {
        DefaultRoute::Reject(RejectReason::PolicyDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::{HostMatcher, OrdinaryTlsConfig, TlsHostRoute};

    fn acme() -> Vec<Vec<u8>> {
        vec![TLS_ALPN_PROTOCOL.to_vec()]
    }

    #[test]
    fn control_host_short_circuits_acl() {
        assert_eq!(
            classify(
                Some("TLS.Example."),
                &[],
                Some("tls.example"),
                |_| false,
                |_| false,
            ),
            DefaultRoute::ControlPlane {
                hostname: "tls.example".into()
            }
        );
    }

    #[test]
    fn unmatched_acme_never_falls_through() {
        assert_eq!(
            classify(
                Some("site.example"),
                &acme(),
                Some("tls.example"),
                |_| false,
                |_| true,
            ),
            DefaultRoute::Reject(RejectReason::UnmatchedAcmeChallenge)
        );
    }

    #[test]
    fn active_acme_bypasses_ordinary_acl() {
        assert_eq!(
            classify(
                Some("site.example"),
                &acme(),
                Some("tls.example"),
                |_| true,
                |_| false,
            ),
            DefaultRoute::AcmeChallenge {
                domain: "site.example".into()
            }
        );
    }

    #[test]
    fn complete_decision_returns_the_configured_per_host_action() {
        let config = DefaultListenerConfig {
            bind: "0.0.0.0:443".into(),
            ordinary_traffic: OrdinaryTlsConfig {
                routes: vec![TlsHostRoute {
                    name: "application".into(),
                    matcher: HostMatcher {
                        exact: vec!["app.example".into()],
                        ..Default::default()
                    },
                    action: TlsRouteAction::Terminate {
                        target_port: 8080,
                        target: Some("backend.internal".into()),
                        upstream: UpstreamTransport::Plaintext,
                        load_balancing: crate::runtime_config::HttpLoadBalancing::RoundRobin,
                    },
                }],
                ..Default::default()
            },
        };
        let hello = ClientHello {
            sni_host: "APP.EXAMPLE".into(),
            alpn_protocols: vec![b"h2".to_vec()],
            random: [0; 32],
            buffered: Vec::new(),
        };
        assert!(matches!(
            decide(&hello, &config, Some("tls.example"), |_| false),
            ConnectionRoute::Ordinary {
                action: TlsRouteAction::Terminate {
                    target_port: 8080,
                    upstream: UpstreamTransport::Plaintext,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn complete_decision_never_uses_control_host_route_table_entry() {
        let mut config = DefaultListenerConfig::default();
        config.ordinary_traffic.routes.push(TlsHostRoute {
            name: "must-not-proxy".into(),
            matcher: HostMatcher {
                exact: vec!["tls.example".into()],
                ..Default::default()
            },
            action: TlsRouteAction::Passthrough {
                target_port: 443,
                target: Some("upstream.example".into()),
                load_balancing: crate::runtime_config::HttpLoadBalancing::RoundRobin,
            },
        });
        let hello = ClientHello {
            sni_host: "tls.example".into(),
            alpn_protocols: Vec::new(),
            random: [0; 32],
            buffered: Vec::new(),
        };
        assert!(matches!(
            decide(&hello, &config, Some("tls.example"), |_| false),
            ConnectionRoute::ControlPlane { .. }
        ));
    }
}
