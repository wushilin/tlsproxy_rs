use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::acme::backend::StoreRenewalBackend;
use crate::accounting::{CdrRecord, ListenerType};
use crate::acme::dns::PublicDnsPrerequisite;
use crate::acme::scheduler::RenewalScheduler;
use crate::ca::LocalCa;
use crate::config::{Listener, ListenerMode, Policy, Rules};
use crate::controller::Controller;
use crate::extensible::Extensible;
use crate::listener_stats::ListenerStats;
use crate::request_id::RequestId;
use crate::runtime_config::{
    AdditionalListenerConfig, DefaultListenerConfig, HostRoutedHttpListenerConfig,
    HostRoutedTlsListenerConfig, RawForwardListenerConfig,
    TlsRouteAction, UpstreamTransport, DEFAULT_LISTENER_NAME,
};
use crate::store::Store;

enum BoundAdditional {
    Tls(String, TcpListener, HostRoutedTlsListenerConfig),
    Http(String, TcpListener, HostRoutedHttpListenerConfig),
    Forward(String, TcpListener, RawForwardListenerConfig),
}

pub async fn run(runtime_dir: &Path, store: Store, mut stored: crate::store::StoredConfig) -> Result<()> {
    let mut last_good: Option<crate::store::StoredConfig> = None;
    loop {
        match run_revision(runtime_dir, store.clone(), stored.clone()).await {
            Ok(false) => return Ok(()),
            Ok(true) => last_good = Some(stored.clone()),
            Err(cause) => {
                let Some(previous) = last_good.take() else {
                    return Err(cause);
                };
                log::error!(
                    "configuration revision {} failed to apply: {cause:#}; rolling back to revision {}",
                    stored.revision,
                    previous.revision
                );
                stored = store
                    .save_config_async(previous.config, "runtime-auto-rollback".into())
                    .await?;
                continue;
            }
        }
        stored = store
            .load_config_async()
            .await?
            .context("configuration disappeared during runtime reload")?;
        info!("applying RocksDB configuration revision {}", stored.revision);
    }
}

async fn run_revision(runtime_dir: &Path, store: Store, stored: crate::store::StoredConfig) -> Result<bool> {
    let config = stored.config;
    config.validate()?;
    store.ensure_builtin_providers()?;
    crate::resolver::init_dns(&config.dns_overrides).await;
    if let Some(accounting) = &config.accounting {
        crate::accounting::init(accounting).await?;
    }
    crate::active_tracker::reset().await;
    crate::forward::reset().await;

    // Bind everything before spawning. In particular, no scheduler/order can
    // run unless the mandatory listener has successfully claimed port 443.
    let default_listener = bind(&config.default_listener.bind)
        .await
        .with_context(|| format!("failed to bind mandatory listener {}", config.default_listener.bind))?;
    let mut additional = Vec::new();
    for (name, listener) in &config.additional_listeners {
        let socket = bind(listener.bind())
            .await
            .with_context(|| format!("failed to bind listener `{name}` at {}", listener.bind()))?;
        additional.push(match listener.clone() {
            AdditionalListenerConfig::Tls(value) => BoundAdditional::Tls(name.clone(), socket, value),
            AdditionalListenerConfig::Http(value) => BoundAdditional::Http(name.clone(), socket, value),
            AdditionalListenerConfig::Forward(value) => BoundAdditional::Forward(name.clone(), socket, value),
        });
    }

    let runtime_dir = runtime_dir.to_string_lossy().to_string();
    let ca = LocalCa::new(&runtime_dir, &crate::config::LocalCaConfig::default())?;
    let cache = crate::managed_tls::ManagedCertificateCache::default();
    cache.reload(&store).await?;
    let backend = Arc::new(
        StoreRenewalBackend::new(
            store.clone(),
            crate::acme_challenge::global().clone(),
            config.acme.clone(),
            config.control_plane.clone(),
            Arc::new(PublicDnsPrerequisite::default().with_store(store.clone())),
        )
        .with_certificate_cache(cache.clone()),
    );
    let scheduler = RenewalScheduler::with_timing(
        backend,
        Duration::from_secs(u64::from(config.acme.scan_interval_hours) * 3600),
        crate::acme::scheduler::DEFAULT_RENEWAL_DEADLINE,
    );
    let scheduler_for_api = scheduler.clone();
    let reload = Arc::new(tokio::sync::Notify::new());
    let reload_for_api = reload.clone();
    let control_state = crate::control_api::ControlState::new(
        store.clone(),
        move || scheduler_for_api.request_scan(),
    )
    .with_configuration_changed(move || {
        let reload = reload_for_api.clone();
        tokio::spawn(async move {
            // Let the HTTPS response flush before its listener is cancelled.
            tokio::time::sleep(Duration::from_millis(250)).await;
            reload.notify_one();
        });
    });
    let control_router = crate::control_api::router(control_state);
    let control_service = Arc::new(crate::listener::control_plane::AxumControlPlaneService::new(
        control_router,
        ca.clone(),
        cache.clone(),
        config.certificate_fallback,
    ));

    let mut root = Controller::new();
    ca.spawn_eviction_job(&mut root);
    crate::forward::spawn_global_health_checks(&mut root);
    scheduler.spawn(&mut root);
    let maintenance_store = store.clone();
    drop(root.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match maintenance_store.cleanup_retention(time::OffsetDateTime::now_utc(), 3, 90) {
                Ok(summary) => info!("automatic retention completed: {summary:?}"),
                Err(cause) => warn!("automatic retention failed: {cause:#}"),
            }
        }
    }));

    for listener in &additional {
        if let BoundAdditional::Forward(name, _, config) = listener {
            crate::forward::register_forward_listener(
                name.clone(),
                &config.targets,
                config.upstream_tls,
            )
            .await?;
        }
    }

    let default_stats = Arc::new(ListenerStats::new(
        DEFAULT_LISTENER_NAME,
        config.default_listener.ordinary_traffic.max_idle_time_ms.unwrap_or(u64::MAX),
    ));
    let default_config = Arc::new(config.default_listener.clone());
    let control_hostname = (!config.control_plane.hostname.is_empty())
        .then(|| config.control_plane.hostname.clone());
    let default_child = root.child();
    let default_ca = ca.clone();
    let default_cache = cache.clone();
    let fallback = config.certificate_fallback;
    drop(root.spawn(async move {
        if let Err(cause) = crate::listener::default::run(
            default_listener,
            default_config,
            control_hostname,
            default_stats,
            default_child,
            default_ca,
            control_service,
            default_cache,
            fallback,
        )
        .await
        {
            log::error!("mandatory listener stopped: {cause:#}");
        }
    }));

    for listener in additional {
        match listener {
            BoundAdditional::Tls(name, socket, listener) => {
                let child = root.child();
                let ca = ca.clone();
                let cache = cache.clone();
                let fallback = config.certificate_fallback;
                drop(root.spawn(async move {
                    run_tls_listener(name, socket, listener, child, ca, cache, fallback).await;
                }));
            }
            BoundAdditional::Http(name, socket, listener) => {
                let child = root.child();
                drop(root.spawn(async move { run_http_listener(name, socket, listener, child).await }));
            }
            BoundAdditional::Forward(name, socket, listener) => {
                let child = root.child();
                drop(root.spawn(async move { run_forward_listener(name, socket, listener, child).await }));
            }
        }
    }
    info!("RocksDB runtime started at configuration revision {}", stored.revision);
    let should_reload = tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            false
        }
        _ = reload.notified() => true,
    };
    root.cancel();
    crate::accounting::shutdown().await;
    Ok(should_reload)
}

async fn bind(pattern: &str) -> Result<TcpListener> {
    let address = crate::bindaddr::resolve_bind_addr(pattern)?;
    let mut last = None;
    for _ in 0..3 {
        match TcpListener::bind(&address).await {
            Ok(listener) => return Ok(listener),
            Err(cause) => {
                last = Some(cause);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(last.expect("bind attempted").into())
}

async fn run_tls_listener(
    name: String,
    listener: TcpListener,
    config: HostRoutedTlsListenerConfig,
    mut controller: Controller,
    ca: LocalCa,
    cache: crate::managed_tls::ManagedCertificateCache,
    fallback: crate::runtime_config::CertificateFallbackPolicy,
) {
    let stats = Arc::new(ListenerStats::new(&name, config.routing.max_idle_time_ms.unwrap_or(u64::MAX)));
    let default_config = Arc::new(DefaultListenerConfig { bind: config.bind.clone(), ordinary_traffic: config.routing.clone() });
    let name = Arc::new(name);
    loop {
        let Ok((socket, remote)) = listener.accept().await else { continue };
        let client = Extensible::of(socket);
        client.extend(RequestId::new()).await;
        let request_id = client.get_extension::<RequestId>().await.unwrap();
        let task_name = name.clone();
        let task_stats = stats.clone();
        let task_config = default_config.clone();
        let task_ca = ca.clone();
        let task_cache = cache.clone();
        let connection_controller = Arc::new(RwLock::new(controller.child()));
        drop(controller.spawn(async move {
            track_start(&request_id, &task_name, remote, &task_stats).await;
            let result: Result<ListenerType> = async {
                let mut client = client;
                let hello = crate::tls_header::read_client_hello(&mut client, Duration::from_secs(5), crate::tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE).await?;
                let action = task_config.ordinary_traffic.select_route(&hello.sni_host).cloned().context("SNI denied by listener policy")?;
                let listener_type = match action {
                    TlsRouteAction::Passthrough { .. } => ListenerType::TlsPassthrough,
                    TlsRouteAction::Terminate { .. } => ListenerType::TlsTerminate,
                    TlsRouteAction::Reject => ListenerType::TlsPassthrough,
                };
                crate::listener::default::dispatch_non_control(
                    crate::listener::default::ConnectionRoute::Ordinary { sni: hello.sni_host.clone(), action },
                    hello, client, task_name.clone(), &task_config, task_stats.clone(), connection_controller, task_ca, task_cache, fallback,
                ).await?;
                Ok(listener_type)
            }.await;
            track_end(&request_id, &task_name, &task_stats, result, ListenerType::TlsPassthrough).await;
        }));
    }
}

async fn run_http_listener(name: String, listener: TcpListener, config: HostRoutedHttpListenerConfig, mut controller: Controller) {
    let stats = Arc::new(ListenerStats::new(&name, config.max_idle_time_ms.unwrap_or(u64::MAX)));
    let legacy = Arc::new(Listener { bind: config.bind.clone(), target: None, target_port: 80, policy: Policy::DENY, rules: empty_rules(), max_idle_time_ms: config.max_idle_time_ms, speed_limit: config.speed_limit, mode: ListenerMode::Http, upstream_tls: false });
    let config = Arc::new(config);
    let name = Arc::new(name);
    loop {
        let Ok((socket, remote)) = listener.accept().await else { continue };
        let client = Extensible::of(socket); client.extend(RequestId::new()).await;
        let request_id = client.get_extension::<RequestId>().await.unwrap();
        let (task_name, task_stats, task_config, task_legacy) = (name.clone(), stats.clone(), config.clone(), legacy.clone());
        let connection_controller = Arc::new(RwLock::new(controller.child()));
        drop(controller.spawn(async move {
            track_start(&request_id, &task_name, remote, &task_stats).await;
            let result: Result<ListenerType> = async {
                let mut client = client;
                let head = crate::http_header::read_http_head(&mut client, Duration::from_secs(10), crate::http_header::DEFAULT_MAX_HTTP_HEADER_SIZE).await?;
                let action = task_config.select_route(&head.host).cloned().context("HTTP host denied by listener policy")?;
                crate::listener::http_passthrough::run(task_name.clone(), client, remote, task_legacy, task_stats.clone(), connection_controller, Some(head), Some((action.target, action.target_port, action.upstream == UpstreamTransport::Tls))).await?;
                Ok(ListenerType::HttpPassthrough)
            }.await;
            track_end(&request_id, &task_name, &task_stats, result, ListenerType::HttpPassthrough).await;
        }));
    }
}

async fn run_forward_listener(name: String, listener: TcpListener, config: RawForwardListenerConfig, mut controller: Controller) {
    let stats = Arc::new(ListenerStats::new(&name, config.max_idle_time_ms.unwrap_or(u64::MAX)));
    let legacy = Arc::new(Listener { bind: config.bind, target: Some(config.targets), target_port: 0, policy: Policy::DENY, rules: empty_rules(), max_idle_time_ms: config.max_idle_time_ms, speed_limit: config.speed_limit, mode: ListenerMode::Forward, upstream_tls: config.upstream_tls });
    let name = Arc::new(name);
    loop {
        let Ok((socket, remote)) = listener.accept().await else { continue };
        let client = Extensible::of(socket); client.extend(RequestId::new()).await;
        let request_id = client.get_extension::<RequestId>().await.unwrap();
        let (task_name, task_stats, task_legacy) = (name.clone(), stats.clone(), legacy.clone());
        let connection_controller = Arc::new(RwLock::new(controller.child()));
        drop(controller.spawn(async move {
            track_start(&request_id, &task_name, remote, &task_stats).await;
            let result = crate::listener::forward::run(task_name.clone(), client, task_legacy, task_stats.clone(), connection_controller).await.map(|_| ListenerType::PortForward);
            track_end(&request_id, &task_name, &task_stats, result, ListenerType::PortForward).await;
        }));
    }
}

fn empty_rules() -> Rules { Rules { static_hosts: Vec::new(), patterns: Vec::new() } }

async fn track_start(id: &RequestId, name: &str, remote: SocketAddr, stats: &ListenerStats) {
    crate::active_tracker::put(id, name, remote).await;
    stats.increase_conn_count();
}

async fn track_end(
    id: &RequestId,
    name: &str,
    stats: &ListenerStats,
    result: Result<ListenerType>,
    fallback_type: ListenerType,
) {
    let listener_type = result.as_ref().copied().unwrap_or(fallback_type);
    if let Err(cause) = result { warn!("listener {name} connection failed: {cause:#}"); }
    let closed = crate::active_tracker::remove(id).await;
    if let Some(closed) = closed.filter(|_| crate::accounting::enabled()) {
        let end_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        crate::accounting::submit(CdrRecord {
            listener_type,
            connection_id: id.to_string(),
            listener_name: name.to_owned(),
            sni: closed.sni,
            target_host: closed.target_host,
            target_endpoint: closed.target_endpoint,
            remote_address: closed.remote_address.to_string(),
            status: closed.status,
            uploaded_bytes: closed.uploaded_bytes,
            downloaded_bytes: closed.downloaded_bytes,
            start_unix_ms: closed.started_at_unix_ms,
            end_unix_ms,
        });
    }
    stats.decrease_conn_count();
}
