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

use crate::controller::Controller;
use crate::conn_stream::ConnStream;
use crate::listener_stats::ListenerStats;
use crate::runtime_config::{DefaultListenerConfig, TlsRouteAction, UpstreamTransport};
use crate::store::normalize_domain;
use crate::tls_header::ClientHello;
use crate::dataplane::pipeline::Intercept;

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
        client: ConnStream<TcpStream>,
    ) -> ControlFuture<'a>;
}

/// Runs the already-bound mandatory listener. Binding is kept outside this
/// function so startup can report bind failures synchronously and tests can
/// use an ephemeral port without a find-then-bind race.
pub async fn run(
    listener: tokio::net::TcpListener,
    control_hostname: Option<String>,
    stats: Arc<ListenerStats>,
    mut listener_controller: Controller,
    ca: LocalCa,
    control_service: Arc<dyn ControlPlaneService>,
    certificate_cache: crate::managed_tls::ManagedCertificateCache,
) -> Result<()> {
    let name = Arc::new(crate::runtime_config::DEFAULT_LISTENER_NAME.to_owned());
    loop {
        let (socket, remote_address) = listener.accept().await?;
        let client = ConnStream::of(socket);
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
    client: ConnStream<TcpStream>,
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
    let request_id = client.request_id();
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
        // The mandatory listener's single TLS interception. Its ClientHello
        // artifact (SNI + ALPN) feeds `decide`, the classifier that routes ACME
        // and control-plane traffic ahead of the data-plane interceptor stages.
        let crate::dataplane::pipeline::Intercepted { artifact: hello, stream: client } =
            crate::dataplane::tls::ClientHelloIntercept::new(client, Duration::from_secs(5))
                .intercept()
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
                let ctx = crate::dataplane::ConnCtx { name: name.clone(), remote: remote_address, stats: stats.clone(), controller };
                let tls = crate::dataplane::TlsCtx { ca, cache: certificate_cache, fallback: certificate_fallback };
                dispatch_non_control(ctx, tls, route, hello, client, &config)
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
        DefaultRoute::Proxy { sni } => match config.ordinary_traffic.select_route(&sni) {
            Some(action) => ConnectionRoute::Ordinary { sni, action: action.clone() },
            // The classifier only returns Proxy for an available route; if
            // the tables disagree, reject the connection instead of panicking.
            None => ConnectionRoute::Reject(RejectReason::PolicyDenied),
        },
        DefaultRoute::Reject(reason) => ConnectionRoute::Reject(reason),
    }
}

/// Executes every non-control branch selected by [`decide`]. Control-plane
/// connections deliberately remain the responsibility of the admin service
/// adapter so this module cannot accidentally proxy the reserved hostname.
pub(crate) async fn dispatch_non_control(
    ctx: crate::dataplane::ConnCtx,
    tls: crate::dataplane::TlsCtx,
    route: ConnectionRoute,
    hello: ClientHello,
    client: ConnStream<TcpStream>,
    config: &DefaultListenerConfig,
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
            let policy = crate::dataplane::RelayPolicy::for_tls_route(config, &action);
            match action {
                TlsRouteAction::Passthrough { target_port, target, load_balancing } => {
                    crate::dataplane::tls::passthrough::run(
                        ctx,
                        policy,
                        client,
                        Some(hello),
                        Some(crate::dataplane::tls::passthrough::PassthroughRoute { target, target_port, load_balancing }),
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
                    let certified_key = tls.cache
                        .resolve_with_fallback(&hello.sni_host, &tls.ca, tls.fallback)
                        .await?;
                    crate::dataplane::tls::terminate::run_inspected(
                        ctx,
                        policy,
                        tls.ca,
                        client,
                        hello,
                        crate::dataplane::tls::terminate::TerminateRoute {
                            target,
                            target_port,
                            upstream_tls: upstream == UpstreamTransport::Tls,
                            load_balancing,
                        },
                        certified_key,
                    )
                    .await
                }
                TlsRouteAction::ReverseProxy { action } => {
                    crate::managed_tls::request_automatic_for_sni(&hello.sni_host);
                    let certified_key = tls.cache
                        .resolve_with_fallback(&hello.sni_host, &tls.ca, tls.fallback)
                        .await?;
                    crate::dataplane::tls::terminate::run_reverse_proxy(
                        ctx, policy, client, hello, certified_key, action,
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


/// Pure routing policy for the required public :443 listener.
///
/// Ordering is security-sensitive: an active local ACME challenge takes
/// precedence, while unmatched ACME ALPN follows the ordinary SNI route. The
/// reserved control hostname bypasses ordinary ACL evaluation but can never
/// become an upstream route.
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
    if is_acme && challenge_is_active(&sni) {
        return DefaultRoute::AcmeChallenge { domain: sni };
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
    fn unmatched_acme_follows_ordinary_sni_route() {
        assert_eq!(
            classify(
                Some("site.example"),
                &acme(),
                Some("tls.example"),
                |_| false,
                |_| true,
            ),
            DefaultRoute::Proxy {
                sni: "site.example".into()
            }
        );
    }

    #[test]
    fn unmatched_acme_is_rejected_without_an_ordinary_route() {
        assert_eq!(
            classify(
                Some("site.example"),
                &acme(),
                Some("tls.example"),
                |_| false,
                |_| false,
            ),
            DefaultRoute::Reject(RejectReason::PolicyDenied)
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
    fn unmatched_acme_selects_configured_passthrough_action() {
        let mut config = DefaultListenerConfig::default();
        config.ordinary_traffic.routes.push(TlsHostRoute {
            name: "downstream-acme".into(),
            matcher: HostMatcher {
                exact: vec!["site.example".into()],
                ..Default::default()
            },
            action: TlsRouteAction::Passthrough {
                target_port: 443,
                target: Some("downstream.example".into()),
                load_balancing: crate::runtime_config::HttpLoadBalancing::RoundRobin,
            },
        });
        let hello = ClientHello {
            sni_host: "site.example".into(),
            alpn_protocols: acme(),
            random: [0; 32],
            buffered: vec![1, 2, 3],
        };

        assert!(matches!(
            decide(&hello, &config, Some("tls.example"), |_| false),
            ConnectionRoute::Ordinary {
                action: TlsRouteAction::Passthrough {
                    target_port: 443,
                    ..
                },
                ..
            }
        ));
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
