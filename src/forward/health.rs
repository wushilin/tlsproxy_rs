//! Active health checking for backend endpoint groups: the global probe
//! loop, per-endpoint TCP/HTTP probes, configured-target registration, and
//! the status views the control plane serves. Shares the endpoint-group state
//! owned by the parent module.

use super::*;

lazy_static! {
    pub(super) static ref HEALTH_BINDINGS: Arc<RwLock<Vec<HealthBinding>>> =
        Arc::new(RwLock::new(Vec::new()));
}

#[derive(Debug, Clone)]
pub(super) struct HealthBinding {
    pub(super) group: GroupKey,
    listener: String,
    host: String,
    path: String,
    backend: String,
    transport: String,
    load_balancing: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheckTarget {
    pub listener: String,
    pub host: String,
    pub path: String,
    pub backend: String,
    pub endpoint: String,
    pub transport: String,
    pub load_balancing: String,
    pub online: Option<bool>,
    pub since_ms: u128,
    pub last_checked_ms: u128,
}

fn matcher_label(matcher: &crate::runtime_config::HostMatcher) -> String {
    matcher.exact.iter().cloned()
        .chain(matcher.suffix.iter().map(|value| format!("*.{}", value.trim_start_matches('.'))))
        .chain(matcher.patterns.iter().map(|value| format!("/{value}/")))
        .collect::<Vec<_>>().join(", ")
}

async fn bind_http_action(listener: &str, host: &str, path: &str, action: &crate::runtime_config::HttpRouteAction) -> Result<()> {
    for backend in &action.backends {
        let requested = HostAndPort::parse_or_default(&backend.address, action.target_port);
        let tls = backend.transport == crate::runtime_config::UpstreamTransport::Tls;
        let tls_name = backend.tls_server_name.clone().unwrap_or_else(|| requested.host().to_string());
        let group = ensure_group(requested.to_string(), requested.host(), requested.port(), tls, true, tls_name, Owner::Configured).await?;
        HEALTH_BINDINGS.write().await.push(HealthBinding {
            group: group.key.clone(), listener: listener.into(), host: host.into(), path: path.into(),
            backend: backend.address.clone(), transport: if tls { "tls" } else { "plaintext" }.into(),
            load_balancing: match action.load_balancing { HttpLoadBalancing::RoundRobin => "round_robin", HttpLoadBalancing::ClientIpHash => "client_ip_hash" }.into(),
        });
    }
    for route in &action.paths {
        if let crate::runtime_config::HttpPathAction::ReverseProxy { action } = &route.action {
            Box::pin(bind_http_action(listener, host, &route.prefix, action)).await?;
        }
    }
    Ok(())
}


pub async fn configure_health_checks(config: &crate::runtime_config::RuntimeConfig) -> Result<()> {
    HEALTH_BINDINGS.write().await.clear();
    for route in &config.default_listener.ordinary_traffic.routes {
        if let crate::runtime_config::TlsRouteAction::ReverseProxy { action } = &route.action {
            bind_http_action(crate::runtime_config::DEFAULT_LISTENER_NAME, &matcher_label(&route.matcher), "/", action).await?;
        }
    }
    for (listener, configured) in &config.additional_listeners {
        if config.disabled_listeners.contains(listener) {
            continue;
        }
        match configured {
            crate::runtime_config::AdditionalListenerConfig::Tls(value) => {
                for route in &value.routing.routes {
                    if let crate::runtime_config::TlsRouteAction::ReverseProxy { action } = &route.action {
                        bind_http_action(listener, &matcher_label(&route.matcher), "/", action).await?;
                    }
                }
            }
            crate::runtime_config::AdditionalListenerConfig::Http(value) => {
                for route in &value.routes { bind_http_action(listener, &matcher_label(&route.matcher), "/", &route.action).await?; }
            }
            crate::runtime_config::AdditionalListenerConfig::Redirect(_) | crate::runtime_config::AdditionalListenerConfig::Forward(_) => {}
        }
    }
    Ok(())
}

pub async fn health_check_targets() -> Vec<HealthCheckTarget> {
    let bindings = HEALTH_BINDINGS.read().await.clone();
    let groups = GROUPS.read().await;
    let mut values = Vec::new();
    for binding in bindings {
        let Some(group) = groups.get(&binding.group) else { continue };
        for endpoint in group.endpoints.read().await.iter() {
            values.push(HealthCheckTarget {
                listener: binding.listener.clone(), host: binding.host.clone(), path: binding.path.clone(),
                backend: binding.backend.clone(), endpoint: endpoint.endpoint.clone(), transport: binding.transport.clone(),
                load_balancing: binding.load_balancing.clone(), online: endpoint.online, since_ms: endpoint.since_ms,
                last_checked_ms: endpoint.last_checked_ms,
            });
        }
    }
    values.sort_by(|a, b| (&a.listener, &a.host, &a.backend, &a.endpoint).cmp(&(&b.listener, &b.host, &b.backend, &b.endpoint)));
    values
}

/// Spawns the background probe loop that keeps every registered endpoint
/// group's online/offline state fresh and persists status samples.
pub fn spawn_global_health_checks(controller: &mut Controller, store: crate::store::Store) {
    let health_controller = controller.child();
    controller.spawn(async move {
        run_global_health_checks(health_controller, store).await;
    });
}

async fn check_all_once(check_controller: &mut Controller, store: &crate::store::Store) {
    evict_expired_runtime_groups().await;
    evict_excess_runtime_groups().await;
    let groups: Vec<_> = GROUPS.read().await.values().cloned().collect();
    let mut jobs = Vec::new();
    for group in groups {
        group.reconcile_endpoints().await;
        let endpoints: Vec<String> = group
            .endpoints
            .read()
            .await
            .iter()
            .map(|endpoint| endpoint.endpoint.clone())
            .collect();
        for endpoint in endpoints {
            jobs.push((Arc::clone(&group), endpoint));
        }
    }
    check_jobs(jobs, check_controller).await;
    let checked_at = OffsetDateTime::now_utc();
    let samples = health_check_targets().await.into_iter().filter_map(|value| Some(crate::store::HealthCheckSample {
        listener: value.listener, host: value.host, path: value.path, backend: value.backend,
        endpoint: value.endpoint, transport: value.transport, load_balancing: value.load_balancing,
        online: value.online?, checked_at,
    })).collect::<Vec<_>>();
    if let Err(cause) = store.save_health_check_samples_async(samples).await {
        log::warn!("failed to persist health-check status: {cause:#}");
    }
}

async fn run_global_health_checks(mut health_controller: Controller, store: crate::store::Store) {
    loop {
        let mut check_controller = health_controller.child();
        check_all_once(&mut check_controller, &store).await;
        check_controller.cancel();
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn check_jobs(jobs: Vec<(Arc<MonitorGroup>, String)>, check_controller: &mut Controller) {
    let expected_count = jobs.len();
    let (tx, mut rx) = mpsc::channel(expected_count.max(1));
    for (group, endpoint) in &jobs {
        let tx = tx.clone();
        let group = Arc::clone(group);
        let endpoint = endpoint.clone();
        check_controller.spawn(async move {
            let online = probe(&endpoint, group.key.upstream_tls, group.key.http_health, &group.tls_server_name).await;
            let _ = tx.send((group, endpoint, online)).await;
        });
    }
    drop(tx);

    let mut checked = HashSet::new();
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                break;
            }
            result = rx.recv() => {
                let Some((group, endpoint, online)) = result else {
                    break;
                };
                checked.insert((group.key.clone(), endpoint.clone()));
                group.set_endpoint_state(endpoint, online).await;
                if checked.len() == expected_count {
                    break;
                }
            }
        }
    }

    for (group, endpoint) in jobs {
        if !checked.contains(&(group.key.clone(), endpoint.clone())) {
            group.set_endpoint_state(endpoint, false).await;
        }
    }
    publish_configured_statuses().await;
}

async fn probe(endpoint: &str, upstream_tls: bool, http_health: bool, tls_server_name: &str) -> bool {
    let mut stream = match TcpStream::connect(endpoint).await {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    if !upstream_tls {
        return !http_health || probe_http(&mut stream, tls_server_name).await;
    }

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAllVerifier))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let Ok(server_name) = rustls::pki_types::ServerName::try_from(tls_server_name.to_string())
    else {
        return false;
    };
    let Ok(mut tls) = connector.connect(server_name, stream).await else { return false };
    !http_health || probe_http(&mut tls, tls_server_name).await
}

pub(super) async fn probe_http<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(stream: &mut S, host: &str) -> bool {
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() { return false; }
    let mut response = [0u8; 64];
    let Ok(count) = stream.read(&mut response).await else { return false };
    let first_line = String::from_utf8_lossy(&response[..count]);
    let Some(status) = first_line.split_ascii_whitespace().nth(1).and_then(|value| value.parse::<u16>().ok()) else { return false };
    // Authentication and authorization failures still prove that an HTTP
    // backend is reachable and serving requests. Mark only invalid responses
    // and server-side failures as unhealthy; otherwise protected backends
    // become unavailable immediately after their first unauthenticated probe.
    (100..500).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn http_probe_accepts_authentication_challenge_as_healthy() {
        let (mut probe_side, mut backend_side) = tokio::io::duplex(1024);
        let backend = tokio::spawn(async move {
            let mut request = [0u8; 1024];
            let count = backend_side.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET / HTTP/1.1\r\n"));
            backend_side
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        assert!(probe_http(&mut probe_side, "vpnman.local").await);
        backend.await.unwrap();
    }
}
