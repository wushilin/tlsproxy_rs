use crate::config::{Listener, ListenerMode};
use crate::runtime_config::HttpLoadBalancing;
use crate::controller::Controller;
use crate::hostutil::HostAndPort;
use crate::resolver;
use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use log::info;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use tokio::net::{lookup_host, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, RwLock};
use tokio_rustls::TlsConnector;

const RUNTIME_TTL_MS: u128 = 600_000;
const RUNTIME_GROUP_MAX: usize = 1000;
const DNS_CACHE_TTL_MS: u128 = 60_000;
const DNS_NEGATIVE_CACHE_TTL_MS: u128 = 5_000;

lazy_static! {
    static ref LISTENER_BACKENDS: Arc<RwLock<HashMap<String, Vec<BackendStatusSerde>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref FORWARD_LISTENERS: Arc<RwLock<HashMap<String, Vec<GroupKey>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref GROUPS: Arc<RwLock<HashMap<GroupKey, Arc<MonitorGroup>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref DNS_CACHE: Arc<RwLock<HashMap<(String, u16), CachedDns>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref HTTP_ROUTE_CURSORS: Arc<RwLock<HashMap<String, u64>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref HEALTH_BINDINGS: Arc<RwLock<Vec<HealthBinding>>> =
        Arc::new(RwLock::new(Vec::new()));
}

#[derive(Debug, Clone)]
struct HealthBinding {
    group: GroupKey,
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

#[derive(Debug, Clone)]
struct CachedDns {
    endpoints: Vec<String>,
    expires_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendStatusSerde {
    pub name: String,
    /// Some(true) = up, Some(false) = down, None = not probed yet.
    pub online: Option<bool>,
    pub since_ms: u128,
}

#[derive(Debug, Clone)]
pub struct SelectedTarget {
    pub endpoint: String,
    pub tls_server_name: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct GroupKey {
    target: String,
    upstream_tls: bool,
    http_health: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Configured,
    Runtime,
}

#[derive(Debug, Clone)]
struct EndpointState {
    endpoint: String,
    online: Option<bool>,
    since_ms: u128,
    last_checked_ms: u128,
}

#[derive(Debug)]
struct MonitorGroup {
    key: GroupKey,
    requested_target: String,
    effective_host: String,
    effective_port: u16,
    tls_server_name: String,
    owner: Owner,
    endpoints: RwLock<Vec<EndpointState>>,
    last_activity_ms: RwLock<u128>,
    rng_state: AtomicU64,
}

impl MonitorGroup {
    async fn new(
        requested_target: String,
        effective: HostAndPort,
        upstream_tls: bool,
        http_health: bool,
        tls_server_name: String,
        owner: Owner,
    ) -> Arc<Self> {
        let now = now_ms();
        let key = GroupKey {
            target: effective.to_string(),
            upstream_tls,
            http_health,
        };
        let group = Arc::new(Self {
            key,
            requested_target,
            effective_host: effective.host().to_string(),
            effective_port: effective.port(),
            tls_server_name,
            owner,
            endpoints: RwLock::new(Vec::new()),
            last_activity_ms: RwLock::new(now),
            rng_state: AtomicU64::new(now as u64),
        });
        group.reconcile_endpoints().await;
        group
    }

    async fn touch(&self) {
        *self.last_activity_ms.write().await = now_ms();
    }

    async fn expired(&self, now: u128) -> bool {
        self.owner == Owner::Runtime
            && now.saturating_sub(*self.last_activity_ms.read().await) > RUNTIME_TTL_MS
    }

    async fn choose_endpoint(&self) -> Option<SelectedTarget> {
        let endpoints = self.endpoints.read().await;
        let online: Vec<_> = endpoints
            .iter()
            .filter(|endpoint| endpoint.online == Some(true))
            .collect();
        let unknown: Vec<_> = endpoints
            .iter()
            .filter(|endpoint| endpoint.online.is_none())
            .collect();
        let candidates = if !online.is_empty() {
            online
        } else if !unknown.is_empty() || !endpoints.is_empty() {
            let fallback = if !unknown.is_empty() {
                unknown
            } else {
                endpoints.iter().collect()
            };
            let ipv4: Vec<_> = fallback
                .iter()
                .copied()
                .filter(|endpoint| endpoint_is_ipv4(&endpoint.endpoint))
                .collect();
            if !ipv4.is_empty() {
                ipv4
            } else {
                fallback
            }
        } else {
            return None;
        };
        let index = (self.next_random() as usize) % candidates.len();
        Some(SelectedTarget {
            endpoint: candidates[index].endpoint.clone(),
            tls_server_name: self.tls_server_name.clone(),
        })
    }

    async fn reconcile_endpoints(&self) {
        let resolved = resolve_effective_endpoints(&self.effective_host, self.effective_port).await;
        let now = now_ms();
        let mut endpoints = self.endpoints.write().await;
        let resolved_set: HashSet<String> = resolved.iter().cloned().collect();
        let old: HashMap<String, EndpointState> = endpoints
            .iter()
            .cloned()
            .map(|endpoint| (endpoint.endpoint.clone(), endpoint))
            .collect();
        for endpoint in old.keys() {
            if !resolved_set.contains(endpoint) {
                info!("{} endpoint {} removed", self.key.target, endpoint);
            }
        }
        *endpoints = resolved
            .into_iter()
            .map(|endpoint| {
                old.get(&endpoint).cloned().unwrap_or(EndpointState {
                    endpoint,
                    online: None,
                    since_ms: now,
                    last_checked_ms: 0,
                })
            })
            .collect();
    }

    async fn set_endpoint_state(&self, endpoint: String, online: bool) {
        let mut changed = None;
        {
            let mut endpoints = self.endpoints.write().await;
            if let Some(state) = endpoints
                .iter_mut()
                .find(|state| state.endpoint == endpoint)
            {
                state.last_checked_ms = now_ms();
                if state.online != Some(online) {
                    let before = state.online;
                    state.online = Some(online);
                    state.since_ms = now_ms();
                    changed = Some(before);
                }
            }
        }
        if let Some(before) = changed {
            info!(
                "{} endpoint {} {} -> {}",
                self.key.target,
                endpoint,
                state_name(before),
                state_name(Some(online))
            );
            publish_configured_statuses().await;
        }
    }

    async fn status(&self) -> BackendStatusSerde {
        let endpoints = self.endpoints.read().await;
        // Up if any endpoint is up; unknown while unprobed endpoints could
        // still turn out up; down only when every endpoint is confirmed down
        // (or DNS resolution produced no endpoints at all).
        let online = if endpoints.iter().any(|endpoint| endpoint.online == Some(true)) {
            Some(true)
        } else if endpoints.iter().any(|endpoint| endpoint.online.is_none()) {
            None
        } else {
            Some(false)
        };
        let since_ms = endpoints
            .iter()
            .filter(|endpoint| endpoint.online == online)
            .map(|endpoint| endpoint.since_ms)
            .min()
            .unwrap_or_else(now_ms);
        BackendStatusSerde {
            name: self.requested_target.clone(),
            online,
            since_ms,
        }
    }

    fn next_random(&self) -> u64 {
        let mut value = self.rng_state.load(Ordering::Relaxed);
        loop {
            let mut next = value;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            if next == 0 {
                next = 0x9e37_79b9_7f4a_7c15;
            }
            match self.rng_state.compare_exchange_weak(
                value,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(observed) => value = observed,
            }
        }
    }
}

pub async fn register_forward_listener(
    listener_name: String,
    targets: &str,
    upstream_tls: bool,
) -> Result<()> {
    let mut keys = Vec::new();
    for target in parse_targets(targets)? {
        let requested = HostAndPort::parse_or_default(&target, 0);
        let group = ensure_group(
            requested.to_string(),
            requested.host(),
            requested.port(),
            upstream_tls,
            false,
            requested.host().to_string(),
            Owner::Configured,
        )
        .await?;
        keys.push(group.key.clone());
    }
    FORWARD_LISTENERS.write().await.insert(listener_name, keys);
    publish_configured_statuses().await;
    Ok(())
}

pub async fn reconcile_forward_listener(
    listener_name: String,
    targets: &str,
    upstream_tls: bool,
) -> Result<()> {
    register_forward_listener(listener_name, targets, upstream_tls).await
}

pub async fn reconcile_configured_listeners(listeners: &HashMap<String, Listener>) -> Result<()> {
    LISTENER_BACKENDS.write().await.clear();
    FORWARD_LISTENERS.write().await.clear();
    for (name, listener) in listeners {
        if listener.mode == ListenerMode::Forward {
            if let Some(targets) = listener.target.as_deref() {
                register_forward_listener(name.clone(), targets, listener.upstream_tls).await?;
            }
        }
        if listener.mode == ListenerMode::Http {
            if let Some(targets) = http_listener_targets(listener) {
                let normalized = parse_http_targets(&targets)?;
                register_forward_listener(name.clone(), &normalized, false).await?;
            }
        }
    }
    prune_unreferenced_configured_groups().await;
    publish_configured_statuses().await;
    Ok(())
}

/// Explicit backends of an http listener, or None when it routes dynamically
/// by Host header.
pub fn http_listener_targets(listener: &Listener) -> Option<String> {
    listener
        .target
        .as_deref()
        .map(str::trim)
        .filter(|targets| !targets.is_empty())
        .map(str::to_string)
}

/// Parses http-mode backend targets (`http://host` or `http://host:port`,
/// separated by `,` or `;`; the port defaults to 80) into the `host:port`
/// list format `register_forward_listener` accepts.
pub fn parse_http_targets(targets: &str) -> Result<String> {
    let mut parsed = Vec::new();
    for target in targets.split([',', ';']) {
        let target = target.trim();
        if target.is_empty() {
            continue;
        }
        let lower = target.to_ascii_lowercase();
        if lower.starts_with("https://") {
            return Err(anyhow!(
                "http backend `{target}` must be plaintext http://, https backends are not supported"
            ));
        }
        let Some(rest) = lower
            .starts_with("http://")
            .then(|| &target["http://".len()..])
        else {
            return Err(anyhow!("http backend `{target}` must start with http://"));
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#']) || rest.contains(char::is_whitespace) {
            return Err(anyhow!(
                "http backend `{target}` must be http://host or http://host:port with no path"
            ));
        }
        let host_and_port = if let Ok(parsed) = rest.parse::<HostAndPort>() {
            parsed
        } else if !rest.contains(':') {
            HostAndPort::new(rest.to_string(), 80)
        } else {
            return Err(anyhow!("http backend `{target}` has an invalid host:port"));
        };
        if host_and_port.port() == 0 {
            return Err(anyhow!("http backend `{target}` has an invalid port"));
        }
        parsed.push(host_and_port.to_string());
    }
    if parsed.is_empty() {
        return Err(anyhow!("http backend list requires at least one target"));
    }
    Ok(parsed.join(";"))
}

pub async fn clear_listener(listener_name: &str) {
    LISTENER_BACKENDS.write().await.remove(listener_name);
    FORWARD_LISTENERS.write().await.remove(listener_name);
    prune_unreferenced_configured_groups().await;
    publish_configured_statuses().await;
}

pub async fn reset() {
    LISTENER_BACKENDS.write().await.clear();
    FORWARD_LISTENERS.write().await.clear();
    GROUPS.write().await.clear();
    HTTP_ROUTE_CURSORS.write().await.clear();
    HEALTH_BINDINGS.write().await.clear();
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

pub async fn apply_hot_listener_settings(config: &crate::runtime_config::RuntimeConfig) -> Result<()> {
    for (name, listener) in &config.additional_listeners {
        if config.disabled_listeners.contains(name) {
            continue;
        }
        if let crate::runtime_config::AdditionalListenerConfig::Forward(listener) = listener {
            register_forward_listener(name.clone(), &listener.targets, listener.upstream_tls).await?;
        }
    }
    configure_health_checks(config).await
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

/// Selects a healthy endpoint from a reverse-proxy route's backend pool.
/// Legacy target/port routes remain supported when `backends` is empty.
pub async fn select_http_backend(
    route_key: &str,
    host: &str,
    client_ip: IpAddr,
    action: &crate::runtime_config::HttpRouteAction,
) -> Result<(SelectedTarget, bool)> {
    use crate::runtime_config::HttpLoadBalancing;
    if action.backends.is_empty() {
        let tls = action.upstream == crate::runtime_config::UpstreamTransport::Tls;
        return Ok((select_routed_target(host, action.target.as_deref(), action.target_port, tls).await?, tls));
    }
    let mut available = Vec::new();
    for backend in &action.backends {
        let requested: HostAndPort = backend.address.parse()
            .map_err(|cause| anyhow!("invalid HTTP backend `{}`: {cause}", backend.address))?;
        let tls = backend.transport == crate::runtime_config::UpstreamTransport::Tls;
        let tls_name = backend.tls_server_name.clone().unwrap_or_else(|| requested.host().to_string());
        let group = ensure_group(requested.to_string(), requested.host(), requested.port(), tls, true, tls_name, Owner::Runtime).await?;
        group.touch().await;
        let endpoints = group.endpoints.read().await;
        if endpoints.iter().any(|item| item.online == Some(true)) || endpoints.iter().any(|item| item.online.is_none()) {
            available.push((Arc::clone(&group), tls));
        }
    }
    if available.is_empty() { return Err(anyhow!("no healthy reverse-proxy backends for `{host}`")); }
    let index = match action.load_balancing {
        HttpLoadBalancing::ClientIpHash => {
            let mut hasher = DefaultHasher::new();
            client_ip.hash(&mut hasher);
            (hasher.finish() as usize) % available.len()
        }
        HttpLoadBalancing::RoundRobin => {
            let mut cursors = HTTP_ROUTE_CURSORS.write().await;
            let cursor = cursors.entry(route_key.to_string()).or_default();
            let index = (*cursor as usize) % available.len();
            *cursor = cursor.wrapping_add(1);
            index
        }
    };
    let (group, tls) = &available[index];
    let selected = group.choose_endpoint().await.ok_or_else(|| anyhow!("no healthy reverse-proxy endpoints for `{host}`"))?;
    Ok((selected, *tls))
}

pub async fn choose_online(listener_name: &str, client_ip: IpAddr, load_balancing: crate::runtime_config::HttpLoadBalancing) -> Option<SelectedTarget> {
    let groups = GROUPS.read().await;
    let keys = FORWARD_LISTENERS.read().await.get(listener_name).cloned();
    let mut selected = Vec::new();
    if let Some(keys) = keys {
        for key in keys {
            if let Some(group) = groups.get(&key) {
                if let Some(endpoint) = group.choose_endpoint().await {
                    selected.push(endpoint);
                }
            }
        }
    }
    drop(groups);
    if selected.is_empty() {
        return None;
    }
    let index = match load_balancing {
        crate::runtime_config::HttpLoadBalancing::ClientIpHash => { let mut hasher=DefaultHasher::new(); client_ip.hash(&mut hasher); (hasher.finish() as usize)%selected.len() }
        crate::runtime_config::HttpLoadBalancing::RoundRobin => { let mut cursors=HTTP_ROUTE_CURSORS.write().await; let cursor=cursors.entry(listener_name.to_string()).or_default(); let index=(*cursor as usize)%selected.len(); *cursor=cursor.wrapping_add(1); index }
    };
    selected.get(index).cloned()
}

pub async fn select_runtime_target(
    requested_host: &str,
    requested_port: u16,
    upstream_tls: bool,
    tls_server_name: &str,
) -> Result<SelectedTarget> {
    let group = ensure_group(
        format!("{requested_host}:{requested_port}"),
        requested_host,
        requested_port,
        upstream_tls,
        false,
        tls_server_name.to_string(),
        Owner::Runtime,
    )
    .await?;
    group.touch().await;
    let mut selected = group
        .choose_endpoint()
        .await
        .ok_or_else(|| anyhow!("no available upstream endpoint for {}", group.key.target))?;
    selected.tls_server_name = tls_server_name.to_string();
    Ok(selected)
}

/// Resolves a per-host listener route. An explicit route target replaces the
/// network destination but preserves the original SNI as the upstream TLS
/// server name. DNS overrides are applied to either form by `ensure_group`.
pub async fn select_routed_target(
    sni_host: &str,
    explicit_target: Option<&str>,
    default_port: u16,
    upstream_tls: bool,
) -> Result<SelectedTarget> {
    let requested = explicit_target
        .map(|target| HostAndPort::parse_or_default(target, default_port))
        .unwrap_or_else(|| HostAndPort::new(sni_host.to_string(), default_port));
    if requested.port() == 0 {
        return Err(anyhow!("route target port must be non-zero"));
    }
    let group = ensure_group(
        requested.to_string(),
        requested.host(),
        requested.port(),
        upstream_tls,
        false,
        sni_host.to_string(),
        Owner::Runtime,
    )
    .await?;
    group.touch().await;
    let mut selected = group
        .choose_endpoint()
        .await
        .ok_or_else(|| anyhow!("no available upstream endpoint for {}", group.key.target))?;
    selected.tls_server_name = sni_host.to_string();
    Ok(selected)
}

/// Selects from a comma/semicolon-separated Layer-4 route pool. Each named
/// target goes through the configured DNS override and then normal DNS
/// expansion in `ensure_group`, exactly like reverse-proxy backends.
pub async fn select_routed_pool(
    route_key: &str,
    sni_host: &str,
    explicit_targets: Option<&str>,
    default_port: u16,
    upstream_tls: bool,
    client_ip: IpAddr,
    load_balancing: crate::runtime_config::HttpLoadBalancing,
) -> Result<SelectedTarget> {
    use crate::runtime_config::HttpLoadBalancing;

    let target_texts: Vec<&str> = explicit_targets
        .map(|targets| targets.split([',', ';']).map(str::trim).filter(|value| !value.is_empty()).collect())
        .unwrap_or_else(|| vec![sni_host]);
    if target_texts.is_empty() {
        return Err(anyhow!("route backend pool requires at least one target"));
    }

    let mut preferred = Vec::new();
    let mut fallback = Vec::new();
    for target in target_texts {
        let requested = HostAndPort::parse_or_default(target, default_port);
        if requested.port() == 0 {
            return Err(anyhow!("route target `{target}` has an invalid port"));
        }
        let group = ensure_group(
            requested.to_string(), requested.host(), requested.port(), upstream_tls,
            false, sni_host.to_string(), Owner::Runtime,
        ).await?;
        group.touch().await;
        let endpoints = group.endpoints.read().await;
        if endpoints.iter().any(|item| item.online != Some(false)) {
            preferred.push(group.clone());
        } else if !endpoints.is_empty() {
            fallback.push(group.clone());
        }
    }
    let available = if preferred.is_empty() { fallback } else { preferred };
    if available.is_empty() {
        return Err(anyhow!("no available upstream endpoint for route `{route_key}`"));
    }
    let index = match load_balancing {
        HttpLoadBalancing::ClientIpHash => {
            let mut hasher = DefaultHasher::new();
            client_ip.hash(&mut hasher);
            (hasher.finish() as usize) % available.len()
        }
        HttpLoadBalancing::RoundRobin => {
            let mut cursors = HTTP_ROUTE_CURSORS.write().await;
            let cursor = cursors.entry(route_key.to_string()).or_default();
            let index = (*cursor as usize) % available.len();
            *cursor = cursor.wrapping_add(1);
            index
        }
    };
    let mut selected = available[index].choose_endpoint().await
        .ok_or_else(|| anyhow!("no available upstream endpoint for route `{route_key}`"))?;
    selected.tls_server_name = sni_host.to_string();
    Ok(selected)
}

pub fn spawn_global_health_checks(controller: &mut Controller, store: crate::store::Store) {
    let health_controller = controller.child();
    controller.spawn(async move {
        run_global_health_checks(health_controller, store).await;
    });
}

pub async fn check_all_once(check_controller: &mut Controller, store: &crate::store::Store) {
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

async fn ensure_group(
    requested_target: String,
    requested_host: &str,
    requested_port: u16,
    upstream_tls: bool,
    http_health: bool,
    tls_server_name: String,
    owner: Owner,
) -> Result<Arc<MonitorGroup>> {
    let effective = resolve_override_first(requested_host, requested_port).await;
    let key = GroupKey {
        target: effective.to_string(),
        upstream_tls,
        http_health,
    };
    if let Some(group) = GROUPS.read().await.get(&key).cloned() {
        if owner == Owner::Runtime {
            group.touch().await;
        }
        return Ok(group);
    }
    let group = MonitorGroup::new(
        requested_target,
        effective,
        upstream_tls,
        http_health,
        tls_server_name,
        owner,
    )
    .await;
    GROUPS.write().await.insert(key, Arc::clone(&group));
    Ok(group)
}

async fn resolve_override_first(host: &str, port: u16) -> HostAndPort {
    let resolved = resolver::resolve(host, port)
        .await
        .unwrap_or_else(|| format!("{host}:{port}"));
    HostAndPort::parse_or_default(&resolved, port)
}

async fn evict_expired_runtime_groups() {
    let now = now_ms();
    let groups: Vec<_> = GROUPS.read().await.values().cloned().collect();
    let mut expired = HashSet::new();
    for group in groups {
        if group.expired(now).await {
            expired.insert(group.key.clone());
        }
    }
    if expired.is_empty() {
        return;
    }
    GROUPS.write().await.retain(|key, _| !expired.contains(key));
}

async fn evict_excess_runtime_groups() {
    let groups: Vec<_> = GROUPS.read().await.values().cloned().collect();
    let mut runtime_groups = Vec::new();
    for group in groups {
        if group.owner == Owner::Runtime {
            runtime_groups.push((group.key.clone(), *group.last_activity_ms.read().await));
        }
    }
    let remove = excess_runtime_groups_to_evict(runtime_groups);
    if remove.is_empty() {
        return;
    }
    GROUPS.write().await.retain(|key, _| !remove.contains(key));
}

fn excess_runtime_groups_to_evict(mut runtime_groups: Vec<(GroupKey, u128)>) -> HashSet<GroupKey> {
    if runtime_groups.len() <= RUNTIME_GROUP_MAX {
        return HashSet::new();
    }
    runtime_groups.sort_by_key(|(_, last_activity_ms)| *last_activity_ms);
    let remove_count = runtime_groups.len() - RUNTIME_GROUP_MAX;
    runtime_groups
        .into_iter()
        .take(remove_count)
        .map(|(key, _)| key)
        .collect()
}

async fn prune_unreferenced_configured_groups() {
    let mut referenced: HashSet<GroupKey> = FORWARD_LISTENERS
        .read()
        .await
        .values()
        .flat_map(|keys| keys.iter().cloned())
        .collect();
    referenced.extend(HEALTH_BINDINGS.read().await.iter().map(|binding| binding.group.clone()));
    GROUPS
        .write()
        .await
        .retain(|key, group| group.owner == Owner::Runtime || referenced.contains(key));
}

async fn publish_configured_statuses() {
    let listener_groups = FORWARD_LISTENERS.read().await.clone();
    let groups = GROUPS.read().await;
    let mut statuses = HashMap::new();
    for (listener, keys) in listener_groups {
        let mut listener_statuses = Vec::new();
        for key in keys {
            if let Some(group) = groups.get(&key) {
                listener_statuses.push(group.status().await);
            }
        }
        statuses.insert(listener, listener_statuses);
    }
    *LISTENER_BACKENDS.write().await = statuses;
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

pub async fn statuses(listener_name: &str) -> Vec<BackendStatusSerde> {
    LISTENER_BACKENDS
        .read()
        .await
        .get(listener_name)
        .cloned()
        .unwrap_or_default()
}

fn parse_targets(targets: &str) -> Result<Vec<String>> {
    let mut parsed = Vec::new();
    for target in targets.split([',', ';']) {
        let target = target.trim();
        if target.is_empty() {
            continue;
        }
        if target.contains(char::is_whitespace) {
            return Err(anyhow!(
                "forward target `{target}` must not contain whitespace"
            ));
        }
        let host_and_port: HostAndPort = target
            .parse()
            .map_err(|cause| anyhow!("invalid forward target `{target}`: {cause}"))?;
        if host_and_port.port() == 0 {
            return Err(anyhow!("forward target `{target}` has an invalid port"));
        }
        parsed.push(host_and_port.to_string());
    }
    if parsed.is_empty() {
        return Err(anyhow!("forward mode requires at least one target"));
    }
    Ok(parsed)
}

async fn resolve_effective_endpoints(host: &str, port: u16) -> Vec<String> {
    if let Some(ip) = parse_ip_literal(host) {
        return vec![SocketAddr::new(ip, port).to_string()];
    }
    let key = (host.to_ascii_lowercase(), port);
    let now = now_ms();
    if let Some(cached) = DNS_CACHE.read().await.get(&key).cloned() {
        if cached.expires_at_ms > now {
            return cached.endpoints;
        }
    }
    let endpoints = match lookup_host(format!("{host}:{port}")).await {
        Ok(addrs) => {
            let mut addrs: Vec<_> = addrs.collect();
            addrs.sort_by_key(|addr| if addr.ip().is_ipv4() { 0 } else { 1 });
            addrs.into_iter().map(|addr| addr.to_string()).collect()
        }
        Err(_) => Vec::new(),
    };
    let ttl = if endpoints.is_empty() {
        DNS_NEGATIVE_CACHE_TTL_MS
    } else {
        DNS_CACHE_TTL_MS
    };
    DNS_CACHE.write().await.insert(
        key,
        CachedDns {
            endpoints: endpoints.clone(),
            expires_at_ms: now_ms().saturating_add(ttl),
        },
    );
    endpoints
}

fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok()
}

fn endpoint_is_ipv4(endpoint: &str) -> bool {
    endpoint
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().is_ipv4())
        .unwrap_or(false)
}

#[derive(Debug)]
struct TrustAllVerifier;

impl rustls::client::danger::ServerCertVerifier for TrustAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .expect("rustls crypto provider is installed")
            .signature_verification_algorithms
            .supported_schemes()
    }
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

async fn probe_http<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(stream: &mut S, host: &str) -> bool {
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn state_name(online: Option<bool>) -> &'static str {
    match online {
        Some(true) => "up",
        Some(false) => "down",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    fn test_group(owner: Owner, target: &str, endpoints: Vec<EndpointState>) -> MonitorGroup {
        MonitorGroup {
            key: GroupKey {
                target: target.into(),
                upstream_tls: false,
                http_health: false,
            },
            requested_target: target.into(),
            effective_host: target
                .rsplit_once(':')
                .map(|(host, _)| host)
                .unwrap_or(target)
                .into(),
            effective_port: 443,
            tls_server_name: target
                .rsplit_once(':')
                .map(|(host, _)| host)
                .unwrap_or(target)
                .into(),
            owner,
            endpoints: RwLock::new(endpoints),
            last_activity_ms: RwLock::new(now_ms()),
            rng_state: AtomicU64::new(1),
        }
    }

    #[tokio::test]
    async fn choose_endpoint_falls_back_to_down_endpoints_when_status_is_stale() {
        let group = test_group(
            Owner::Runtime,
            "example.com:443",
            vec![
                EndpointState {
                    endpoint: "[2606:4700:4700::1111]:443".into(),
                    online: Some(false),
                    since_ms: now_ms(),
                    last_checked_ms: now_ms(),
                },
                EndpointState {
                    endpoint: "104.16.248.249:443".into(),
                    online: Some(false),
                    since_ms: now_ms(),
                    last_checked_ms: now_ms(),
                },
            ],
        );

        let selected = group
            .choose_endpoint()
            .await
            .expect("stale down status should not block all candidates");

        assert_eq!(selected.endpoint, "104.16.248.249:443");
        assert_eq!(selected.tls_server_name, "example.com");
    }

    #[tokio::test]
    async fn choose_endpoint_returns_none_only_when_no_endpoints_exist() {
        let group = test_group(Owner::Runtime, "example.com:443", Vec::new());
        assert!(group.choose_endpoint().await.is_none());
    }

    #[tokio::test]
    async fn configured_forward_endpoint_falls_back_to_down_endpoint_when_status_is_stale() {
        let group = test_group(
            Owner::Configured,
            "example.com:443",
            vec![EndpointState {
                endpoint: "127.0.0.1:443".into(),
                online: Some(false),
                since_ms: now_ms(),
                last_checked_ms: now_ms(),
            }],
        );

        let selected = group
            .choose_endpoint()
            .await
            .expect("stale down status should not block configured forward candidates");

        assert_eq!(selected.endpoint, "127.0.0.1:443");
    }

    #[test]
    fn runtime_group_cap_evicts_oldest_last_used_entries() {
        let runtime_groups: Vec<_> = (0..(RUNTIME_GROUP_MAX + 5))
            .map(|index| {
                let key = GroupKey {
                    target: format!("runtime-{index}.example:443"),
                    upstream_tls: false,
                    http_health: false,
                };
                (key, index as u128)
            })
            .collect();

        let evicted = excess_runtime_groups_to_evict(runtime_groups);

        assert_eq!(evicted.len(), 5);
        assert!(evicted.contains(&GroupKey {
            target: "runtime-0.example:443".into(),
            upstream_tls: false,
            http_health: false,
        }));
        assert!(!evicted.contains(&GroupKey {
            target: "runtime-204.example:443".into(),
            upstream_tls: false,
            http_health: false,
        }));
    }

    #[test]
    fn parse_http_targets_normalizes_and_defaults_port() {
        assert_eq!(
            parse_http_targets("http://a.example; http://b.example:8080, HTTP://c.example/").unwrap(),
            "a.example:80;b.example:8080;c.example:80"
        );
        assert!(parse_http_targets("https://a.example").is_err());
        assert!(parse_http_targets("a.example:80").is_err());
        assert!(parse_http_targets("http://a.example/path").is_err());
        assert!(parse_http_targets("http://a.example:notaport").is_err());
        assert!(parse_http_targets("http://a.example:0").is_err());
        assert!(parse_http_targets(" ; , ").is_err());
    }

    #[tokio::test]
    async fn resolve_effective_endpoints_uses_fresh_dns_cache_entry() {
        DNS_CACHE.write().await.clear();
        DNS_CACHE.write().await.insert(
            ("cached.example".into(), 443),
            CachedDns {
                endpoints: vec!["203.0.113.10:443".into()],
                expires_at_ms: now_ms() + 60_000,
            },
        );

        let endpoints = resolve_effective_endpoints("cached.example", 443).await;

        assert_eq!(endpoints, vec!["203.0.113.10:443"]);
        DNS_CACHE.write().await.clear();
    }
}
