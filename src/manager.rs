use std::{collections::HashMap, time::Instant};

use crate::controller::Controller;
use crate::listener_stats::StatsSerde;
use crate::runner::Runner;
use crate::{active_tracker, forward};
use crate::{config::Config, listener_stats::ListenerStats, resolver};
use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
#[derive(Debug, PartialEq, Clone)]
pub enum Status {
    STARTING,
    STARTED,
    STOPPING,
    STOPPED,
}

lazy_static! {
    static ref STATUS: Arc<RwLock<Status>> = Arc::new(RwLock::new(Status::STOPPED));
    static ref STARTED_AT: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));
    static ref LISTENERS: Arc<RwLock<Vec<Arc<ListenerStats>>>> = Arc::new(RwLock::new(Vec::new()));
    static ref LISTENERS_STATUS: Arc<RwLock<HashMap<String, Result<bool, anyhow::Error>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref LISTENER_CONTROLLERS: Arc<RwLock<HashMap<String, Controller>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref CONTROLLER: Arc<RwLock<Controller>> = Arc::new(RwLock::new(Controller::new()));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerStatusSerde {
    pub status: String,
    pub uptime_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerStatusSerde {
    pub running: bool,
    pub backends: Vec<forward::BackendStatusSerde>,
}

pub async fn cancel() {
    info!("attempting to cancel all tasks");
    let mut w = CONTROLLER.write().await;
    w.cancel();
}

pub async fn get_stats(name: &str) -> Option<Arc<ListenerStats>> {
    let r = LISTENERS.read().await;
    for i in r.iter() {
        if i.name == name {
            return Some(Arc::clone(i));
        }
    }
    return None;
}

pub async fn is_running(name: &str) -> bool {
    get_stats(name).await.is_some()
}

pub async fn get_listener_stats() -> HashMap<String, StatsSerde> {
    let mut result = HashMap::new();
    let r = LISTENERS.read().await;
    for i in r.iter() {
        result.insert(i.name.clone(), StatsSerde::from(i));
    }
    return result;
}

pub async fn stop() {
    info!("stopping manager");
    let mut status = STATUS.write().await;
    if *status == Status::STOPPED {
        info!("stopping manager: succeeded (already stopped)");
        return;
    }
    let mut listeners = LISTENERS.write().await;
    let mut listener_status = LISTENERS_STATUS.write().await;
    let mut listener_controllers = LISTENER_CONTROLLERS.write().await;
    info!(
        "transitioning from `{status:?}` to `{:?}`",
        Status::STOPPING
    );
    *status = Status::STOPPING;
    *STARTED_AT.write().await = None;
    listeners.clear();
    listener_controllers.clear();
    active_tracker::reset().await;
    forward::reset().await;
    listener_status.clear();
    info!("cancelling all tasks");
    cancel().await;
    info!("all tasks cancelled by controller");
    *status = Status::STOPPED;
    info!("stopping manager: succeeded");
}

pub async fn get_run_status() -> Status {
    let r = STATUS.read().await;
    return r.clone();
}

pub async fn get_manager_status() -> ManagerStatusSerde {
    let status = get_run_status().await;
    let uptime_ms = STARTED_AT
        .read()
        .await
        .as_ref()
        .map(|started_at| started_at.elapsed().as_millis());
    ManagerStatusSerde {
        status: format!("{status:?}"),
        uptime_ms,
    }
}

pub async fn get_listener_status() -> HashMap<String, Result<ListenerStatusSerde, anyhow::Error>> {
    let status_read = LISTENERS_STATUS.read().await;
    let mut result = HashMap::new();
    for (k, v) in status_read.iter() {
        let v_real = match v {
            Ok(running) => Ok(ListenerStatusSerde {
                running: *running,
                backends: forward::statuses(k).await,
            }),
            Err(some_cause) => Err(anyhow!(format!("{some_cause}"))),
        };
        result.insert(k.clone(), v_real);
    }
    return result;
}

pub async fn start(config: Config) -> Result<HashMap<String, Result<ListenerStatusSerde>>> {
    info!("starting manager");
    let mut status = STATUS.write().await;
    if *status != Status::STOPPED {
        warn!("starting manager: failed (still running)");
        return Err(anyhow!("failed to start, still running"));
    }
    if config.listeners.is_empty() {
        warn!("starting manager: failed (no listeners defined)");
        return Err(anyhow!("failed to start, no listener"));
    }
    {
        let mut listeners = LISTENERS.write().await;
        let mut listener_status = LISTENERS_STATUS.write().await;
        let mut listener_controllers = LISTENER_CONTROLLERS.write().await;
        listeners.clear();
        listener_status.clear();
        listener_controllers.clear();
        forward::reset().await;
    }
    // mark starting...
    *status = Status::STARTING;

    resolver::init(&config).await;
    active_tracker::reset().await;

    let ca = match crate::ca::LocalCa::from_config(&config) {
        Ok(source) => source,
        Err(cause) => {
            *status = Status::STOPPED;
            warn!("starting manager: failed ({cause})");
            return Err(cause);
        }
    };
    {
        let mut controller = CONTROLLER.write().await;
        ca.spawn_eviction_job(&mut controller);
        forward::spawn_global_health_checks(&mut controller);
    }
    for (name, listener) in &config.listeners {
        start_listener_runtime(name, listener.clone(), ca.clone()).await;
    }
    let listener_status = get_listener_status().await;
    let started_count = listener_status
        .values()
        .filter(|result| matches!(result, Ok(status) if status.running))
        .count();
    if started_count == 0 {
        warn!("starting manager: failed (all listeners failed)");
        *status = Status::STOPPED;
        *STARTED_AT.write().await = None;
        LISTENERS.write().await.clear();
        active_tracker::reset().await;
        forward::reset().await;
        cancel().await;
        return Err(anyhow!("failed to start, all listeners failed"));
    }
    info!("starting manager: succeeded ({started_count} listener(s) started)");
    *status = Status::STARTED;
    *STARTED_AT.write().await = Some(Instant::now());
    return Ok(listener_status);
}

async fn start_listener_runtime(
    name: &str,
    listener: crate::config::Listener,
    ca: crate::ca::LocalCa,
) {
    let controller_local = Arc::clone(&CONTROLLER);
    let r = Runner::new(name.to_string(), listener, controller_local, ca);
    let mut root_controller = CONTROLLER.write().await;
    let mut listener_controller = root_controller.child();
    drop(root_controller);

    match r.start_managed(&mut listener_controller).await {
        Ok(stats) => {
            LISTENERS.write().await.push(stats);
            LISTENERS_STATUS
                .write()
                .await
                .insert(name.to_string(), Ok(true));
            LISTENER_CONTROLLERS
                .write()
                .await
                .insert(name.to_string(), listener_controller);
            info!("starting manager: {name} started OK");
        }
        Err(cause) => {
            error!("starting manager: {name} start failed ({cause})");
            LISTENERS_STATUS
                .write()
                .await
                .insert(name.to_string(), Err(cause));
        }
    }
}

pub async fn stop_listener(name: &str) -> Result<bool> {
    let removed = LISTENER_CONTROLLERS.write().await.remove(name);
    let Some(mut controller) = removed else {
        return Ok(false);
    };
    controller.cancel();
    LISTENERS.write().await.retain(|stats| stats.name != name);
    LISTENERS_STATUS
        .write()
        .await
        .insert(name.to_string(), Ok(false));
    forward::clear_listener(name).await;
    if LISTENER_CONTROLLERS.read().await.is_empty() {
        active_tracker::reset().await;
        cancel().await;
        *STATUS.write().await = Status::STOPPED;
        *STARTED_AT.write().await = None;
    }
    Ok(true)
}

pub async fn start_listener(
    name: &str,
    config: Config,
) -> Result<HashMap<String, Result<ListenerStatusSerde>>> {
    if matches!(get_run_status().await, Status::STARTING | Status::STOPPING) {
        return Err(anyhow!("manager is busy"));
    }
    let listener = config
        .listeners
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("listener `{name}` not found"))?;
    if LISTENER_CONTROLLERS.read().await.contains_key(name) {
        return Ok(get_listener_status().await);
    }
    resolver::init(&config).await;
    let ca = crate::ca::LocalCa::from_config(&config)?;
    if LISTENER_CONTROLLERS.read().await.is_empty() {
        let mut controller = CONTROLLER.write().await;
        ca.spawn_eviction_job(&mut controller);
        forward::spawn_global_health_checks(&mut controller);
    }
    *STATUS.write().await = Status::STARTING;
    start_listener_runtime(name, listener, ca).await;
    *STATUS.write().await = Status::STARTED;
    if STARTED_AT.read().await.is_none() {
        *STARTED_AT.write().await = Some(Instant::now());
    }
    Ok(get_listener_status().await)
}

pub async fn restart_listener(
    name: &str,
    config: Config,
) -> Result<HashMap<String, Result<ListenerStatusSerde>>> {
    let _ = stop_listener(name).await?;
    start_listener(name, config).await
}
