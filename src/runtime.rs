use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::acme::backend::StoreRenewalBackend;
use crate::accounting::ListenerType;
use crate::acme::dns::PublicDnsPrerequisite;
use crate::acme::scheduler::RenewalScheduler;
use crate::ca::LocalCa;
use crate::config::{Listener, ListenerMode, Policy, Rules};
use crate::controller::Controller;
use crate::dataplane::pipeline::Intercept;
use crate::extensible::Extensible;
use crate::listener_stats::ListenerStats;
use crate::runtime_config::{
    AdditionalListenerConfig, DefaultListenerConfig, HostRoutedHttpListenerConfig,
    HostRoutedTlsListenerConfig, RawForwardListenerConfig,
    TlsRouteAction, DEFAULT_LISTENER_NAME,
};
use crate::store::Store;

enum BoundAdditional {
    Tls(String, TcpListener, HostRoutedTlsListenerConfig),
    Http(String, TcpListener, HostRoutedHttpListenerConfig),
    Redirect(String, TcpListener, HostRoutedHttpListenerConfig),
    Forward(String, TcpListener, RawForwardListenerConfig),
}

fn restart_senders() -> &'static RwLock<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<()>>> {
    static SENDERS: std::sync::OnceLock<RwLock<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<()>>>> = std::sync::OnceLock::new();
    SENDERS.get_or_init(Default::default)
}

/// Requests an in-place restart of one running listener: its accept loop and
/// active connections are cancelled, the port is rebound, and the loop
/// respawns — without touching configuration revisions or other listeners.
pub async fn request_listener_restart(name: &str) -> bool {
    let key = if name == "__default__" { DEFAULT_LISTENER_NAME } else { name };
    restart_senders().read().await.get(key).is_some_and(|sender| sender.send(()).is_ok())
}

/// Runs one listener's accept loop and cycles it whenever a restart is
/// requested. Dropping the in-flight round future drops its `Controller`
/// child, which cancels the accept loop and every connection task under it.
async fn run_restartable<F, Fut>(
    name: String,
    bind_pattern: String,
    socket: TcpListener,
    mut controller: Controller,
    mut run: F,
) where
    F: FnMut(TcpListener, Controller) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (sender, mut restart) = tokio::sync::mpsc::unbounded_channel();
    restart_senders().write().await.insert(name.clone(), sender);
    let mut socket = Some(socket);
    loop {
        let active = socket.take().expect("restartable listener socket is bound");
        tokio::select! {
            _ = run(active, controller.child()) => return,
            _ = restart.recv() => {
                info!("listener `{name}` restarting: dropping its connections and rebinding {bind_pattern}");
            }
        }
        // The dropped round force-cancelled its connection tasks; each task's
        // ConnGuard cleans up its tracker entry and active count in Drop, so no
        // explicit purge is needed here.
        let mut rebound = None;
        for _ in 0..20 {
            match bind(&bind_pattern).await {
                Ok(value) => { rebound = Some(value); break; }
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
        match rebound {
            Some(value) => socket = Some(value),
            None => {
                log::error!("listener `{name}` failed to rebind {bind_pattern} after restart; it stays down until the next configuration reload");
                return;
            }
        }
    }
}

pub async fn run(runtime_dir: &Path, store: Store, mut stored: crate::store::StoredConfig) -> Result<()> {
    loop {
        match run_revision(runtime_dir, store.clone(), stored.clone()).await {
            Ok(false) => return Ok(()),
            // The shared last-good slot (rather than a local variable) also
            // absorbs revisions proven by successful hot applies, so rollback
            // never silently reverts working hot-applied changes.
            Ok(true) => crate::runtime_live::store_last_good(stored.clone()),
            Err(cause) => {
                let Some(previous) = crate::runtime_live::take_last_good() else {
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
    crate::runtime_live::store(config.clone());
    store.ensure_builtin_providers()?;
    store.migrate_certificates_to_single_domain_ids()?;
    store.ensure_automatic_certificates(&config)?;
    crate::resolver::init_dns(&config.dns_overrides).await;
    if let Some(accounting) = &config.accounting {
        crate::accounting::init(accounting).await?;
    }
    crate::active_tracker::reset();
    crate::events_hub::reset().await;
    restart_senders().write().await.clear();
    crate::forward::reset().await;
    crate::forward::configure_health_checks(&config).await?;

    // Bind everything before spawning. In particular, no scheduler/order can
    // run unless the mandatory listener has successfully claimed port 443.
    let default_listener = bind(&config.default_listener.bind)
        .await
        .with_context(|| format!("failed to bind mandatory listener {}", config.default_listener.bind))?;
    let mut additional = Vec::new();
    for (name, listener) in &config.additional_listeners {
        if config.disabled_listeners.contains(name) {
            info!("listener `{name}` is stopped by configuration");
            continue;
        }
        let socket = bind(listener.bind())
            .await
            .with_context(|| format!("failed to bind listener `{name}` at {}", listener.bind()))?;
        additional.push(match listener.clone() {
            AdditionalListenerConfig::Tls(value) => BoundAdditional::Tls(name.clone(), socket, value),
            AdditionalListenerConfig::Http(value) => BoundAdditional::Http(name.clone(), socket, value),
            AdditionalListenerConfig::Redirect(value) => BoundAdditional::Redirect(name.clone(), socket, value),
            AdditionalListenerConfig::Forward(value) => BoundAdditional::Forward(name.clone(), socket, value),
        });
    }

    // Snapshot this revision's self endpoints for loop prevention: every
    // bound listener socket, the machine's interface addresses, and the
    // configured public self IPs (NAT hairpins that interfaces cannot show).
    {
        let mut bound = vec![default_listener.local_addr()?];
        for listener in &additional {
            let socket = match listener {
                BoundAdditional::Tls(_, socket, _)
                | BoundAdditional::Http(_, socket, _)
                | BoundAdditional::Redirect(_, socket, _)
                | BoundAdditional::Forward(_, socket, _) => socket,
            };
            bound.push(socket.local_addr()?);
        }
        let local_ips = if_addrs::get_if_addrs()
            .map(|interfaces| interfaces.into_iter().map(|interface| interface.ip()).collect())
            .unwrap_or_default();
        let self_ips = config
            .control_plane
            .self_ips
            .iter()
            .filter_map(|value| value.parse().ok())
            .collect();
        crate::runtime_live::store_self_endpoints(crate::runtime_live::SelfEndpoints { bound, local_ips, self_ips });
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
    let scheduler_for_routes = scheduler.clone();
    crate::managed_tls::configure_auto_registration(store.clone(), move || scheduler_for_routes.request_scan());
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
    crate::events_hub::spawn_sampler(&mut root);
    crate::events_hub::spawn_connection_reconciler(&mut root);
    ca.spawn_eviction_job(&mut root);
    crate::forward::spawn_global_health_checks(&mut root, store.clone());
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

    let default_idle_ms = config.default_listener.ordinary_traffic.max_idle_time_ms.unwrap_or(u64::MAX);
    let default_config = Arc::new(config.default_listener.clone());
    let control_hostname = (!config.control_plane.hostname.is_empty())
        .then(|| config.control_plane.hostname.clone());
    let default_bind = config.default_listener.bind.clone();
    let default_child = root.child();
    let default_ca = ca.clone();
    let default_cache = cache.clone();
    let fallback = config.certificate_fallback;
    drop(root.spawn(async move {
        run_restartable(
            DEFAULT_LISTENER_NAME.to_string(),
            default_bind,
            default_listener,
            default_child,
            move |socket, controller| {
                let config = default_config.clone();
                let hostname = control_hostname.clone();
                let ca = default_ca.clone();
                let service = control_service.clone();
                let cache = default_cache.clone();
                async move {
                    // Fresh per round, like the other listeners, so a restart
                    // does not inherit active counts from cancelled tasks.
                    let stats = Arc::new(ListenerStats::new(DEFAULT_LISTENER_NAME, default_idle_ms));
                    crate::events_hub::register_listener(&stats).await;
                    if let Err(cause) = crate::listener::default::run(
                        socket, config, hostname, stats, controller, ca, service, cache, fallback,
                    )
                    .await
                    {
                        log::error!("mandatory listener stopped: {cause:#}");
                    }
                }
            },
        )
        .await;
    }));

    for listener in additional {
        match listener {
            BoundAdditional::Tls(name, socket, listener) => {
                let child = root.child();
                let ca = ca.clone();
                let cache = cache.clone();
                let fallback = config.certificate_fallback;
                let bind_pattern = listener.bind.clone();
                drop(root.spawn(async move {
                    let listener_name = name.clone();
                    run_restartable(name, bind_pattern, socket, child, move |socket, controller| {
                        run_tls_listener(listener_name.clone(), socket, listener.clone(), controller, ca.clone(), cache.clone(), fallback)
                    })
                    .await;
                }));
            }
            BoundAdditional::Http(name, socket, listener) | BoundAdditional::Redirect(name, socket, listener) => {
                let child = root.child();
                let bind_pattern = listener.bind.clone();
                drop(root.spawn(async move {
                    let listener_name = name.clone();
                    run_restartable(name, bind_pattern, socket, child, move |socket, controller| {
                        run_http_listener(listener_name.clone(), socket, listener.clone(), controller)
                    })
                    .await;
                }));
            }
            BoundAdditional::Forward(name, socket, listener) => {
                let child = root.child();
                let bind_pattern = listener.bind.clone();
                drop(root.spawn(async move {
                    let listener_name = name.clone();
                    run_restartable(name, bind_pattern, socket, child, move |socket, controller| {
                        run_forward_listener(listener_name.clone(), socket, listener.clone(), controller)
                    })
                    .await;
                }));
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
    crate::events_hub::register_listener(&stats).await;
    let default_config = Arc::new(DefaultListenerConfig { bind: config.bind.clone(), ordinary_traffic: config.routing.clone() });
    let name = Arc::new(name);
    loop {
        let Ok((socket, remote)) = listener.accept().await else { continue };
        let client = Extensible::of(socket);
       
        let request_id = client.request_id();
        let task_name = name.clone();
        let task_stats = stats.clone();
        let task_config = crate::runtime_live::load().additional_listeners.get(name.as_str()).and_then(|listener| match listener {
            AdditionalListenerConfig::Tls(config) => Some(Arc::new(DefaultListenerConfig { bind: config.bind.clone(), ordinary_traffic: config.routing.clone() })),
            _ => None,
        }).unwrap_or_else(|| default_config.clone());
        stats.set_idle_timeout_ms(task_config.ordinary_traffic.max_idle_time_ms.unwrap_or(u64::MAX));
        let task_ca = ca.clone();
        let task_cache = cache.clone();
        let connection_controller = Arc::new(RwLock::new(controller.child()));
        drop(controller.spawn(async move {
            let _guard = crate::dataplane::ConnGuard::start(request_id.clone(), task_name.clone(), remote, task_stats.clone(), ListenerType::TlsPassthrough);
            let result: Result<()> = async {
                let crate::dataplane::pipeline::Intercepted { artifact: hello, stream: client } =
                    crate::dataplane::tls::ClientHelloIntercept::new(client, Duration::from_secs(5)).intercept().await?;
                let action = task_config.ordinary_traffic.select_route(&hello.sni_host).cloned().context("SNI denied by listener policy")?;
                let listener_type = match action {
                    TlsRouteAction::Passthrough { .. } => ListenerType::TlsPassthrough,
                    TlsRouteAction::Terminate { .. } | TlsRouteAction::ReverseProxy { .. } => ListenerType::TlsTerminate,
                    TlsRouteAction::Reject => ListenerType::TlsPassthrough,
                };
                crate::active_tracker::set_listener_type(&request_id, listener_type);
                let ctx = crate::dataplane::ConnCtx { name: task_name.clone(), remote, stats: task_stats.clone(), controller: connection_controller };
                let tls = crate::dataplane::TlsCtx { ca: task_ca, cache: task_cache, fallback };
                crate::listener::default::dispatch_non_control(
                    ctx, tls,
                    crate::listener::default::ConnectionRoute::Ordinary { sni: hello.sni_host.clone(), action },
                    hello, client, &task_config,
                ).await?;
                Ok(())
            }.await;
            if let Err(cause) = &result { warn!("listener {task_name} connection failed: {cause:#}"); }
        }));
    }
}

async fn run_http_listener(name: String, listener: TcpListener, config: HostRoutedHttpListenerConfig, mut controller: Controller) {
    let stats = Arc::new(ListenerStats::new(&name, config.max_idle_time_ms.unwrap_or(u64::MAX)));
    crate::events_hub::register_listener(&stats).await;
    let config = Arc::new(config);
    let name = Arc::new(name);
    loop {
        let Ok((socket, remote)) = listener.accept().await else { continue };
        let client = Extensible::of(socket);
        let request_id = client.request_id();
        let live = crate::runtime_live::load();
        let task_config = live.additional_listeners.get(name.as_str()).and_then(|listener| match listener {
            AdditionalListenerConfig::Http(config) | AdditionalListenerConfig::Redirect(config) => Some(Arc::new(config.clone())),
            _ => None,
        }).unwrap_or_else(|| config.clone());
        stats.set_idle_timeout_ms(task_config.max_idle_time_ms.unwrap_or(u64::MAX));
        let task_legacy = Arc::new(Listener { bind: task_config.bind.clone(), target: None, target_port: 80, policy: Policy::DENY, rules: empty_rules(), max_idle_time_ms: task_config.max_idle_time_ms, speed_limit: task_config.speed_limit, mode: ListenerMode::Http, upstream_tls: false });
        let (task_name, task_stats) = (name.clone(), stats.clone());
        let connection_controller = Arc::new(RwLock::new(controller.child()));
        drop(controller.spawn(async move {
            let _guard = crate::dataplane::ConnGuard::start(request_id.clone(), task_name.clone(), remote, task_stats.clone(), ListenerType::HttpPassthrough);
            crate::active_tracker::set_listener_type(&request_id, ListenerType::HttpPassthrough);
            let result: Result<()> = async {
                let crate::dataplane::pipeline::Intercepted { artifact: head, stream: client } =
                    crate::dataplane::http::HeadIntercept::new(client, Duration::from_secs(10)).intercept().await?;
                let action = task_config.select_route(&head.host).cloned().context("HTTP host denied by reverse-proxy listener policy")?;
                if action.behavior == crate::runtime_config::HttpBehavior::RedirectHttps {
                    crate::dataplane::http::redirect_https(client, head, action.redirect.as_ref(), task_config.redirect_https_port).await?;
                    return Ok(());
                }
                let route_key = format!("{task_name}:{}", head.host.to_ascii_lowercase());
                let ctx = crate::dataplane::ConnCtx { name: task_name.clone(), remote, stats: task_stats.clone(), controller: connection_controller };
                crate::dataplane::http::run(ctx, task_legacy, client, Some(head), Some((route_key, action)), false, None).await?;
                Ok(())
            }.await;
            if let Err(cause) = &result { warn!("listener {task_name} connection failed: {cause:#}"); }
        }));
    }
}

async fn run_forward_listener(name: String, listener: TcpListener, config: RawForwardListenerConfig, mut controller: Controller) {
    let stats = Arc::new(ListenerStats::new(&name, config.max_idle_time_ms.unwrap_or(u64::MAX)));
    crate::events_hub::register_listener(&stats).await;
    let load_balancing = config.load_balancing;
    let legacy = Arc::new(Listener { bind: config.bind, target: Some(config.targets), target_port: 0, policy: Policy::DENY, rules: empty_rules(), max_idle_time_ms: config.max_idle_time_ms, speed_limit: config.speed_limit, mode: ListenerMode::Forward, upstream_tls: config.upstream_tls });
    let name = Arc::new(name);
    loop {
        let Ok((socket, remote)) = listener.accept().await else { continue };
        let client = Extensible::of(socket);
        let request_id = client.request_id();
        let live = crate::runtime_live::load();
        let live_forward = live.additional_listeners.get(name.as_str()).and_then(|listener| match listener { AdditionalListenerConfig::Forward(config) => Some(config), _ => None });
        let task_load_balancing = live_forward.map(|config| config.load_balancing).unwrap_or(load_balancing);
        stats.set_idle_timeout_ms(live_forward.map(|config| config.max_idle_time_ms).unwrap_or(legacy.max_idle_time_ms).unwrap_or(u64::MAX));
        let task_legacy = live_forward.map(|config| Arc::new(Listener { bind: config.bind.clone(), target: Some(config.targets.clone()), target_port: 0, policy: Policy::DENY, rules: empty_rules(), max_idle_time_ms: config.max_idle_time_ms, speed_limit: config.speed_limit, mode: ListenerMode::Forward, upstream_tls: config.upstream_tls })).unwrap_or_else(|| legacy.clone());
        let (task_name, task_stats) = (name.clone(), stats.clone());
        let connection_controller = Arc::new(RwLock::new(controller.child()));
        drop(controller.spawn(async move {
            let _guard = crate::dataplane::ConnGuard::start(request_id.clone(), task_name.clone(), remote, task_stats.clone(), ListenerType::PortForward);
            crate::active_tracker::set_listener_type(&request_id, ListenerType::PortForward);
            if let Err(cause) = crate::dataplane::l4::run(task_name.clone(), client, task_legacy, task_stats.clone(), connection_controller, remote.ip(), task_load_balancing).await {
                warn!("listener {task_name} connection failed: {cause:#}");
            }
        }));
    }
}

fn empty_rules() -> Rules { Rules { static_hosts: Vec::new(), patterns: Vec::new() } }
