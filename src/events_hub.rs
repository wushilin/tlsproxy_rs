use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use serde::Serialize;
use tokio::sync::{broadcast, RwLock};

use crate::listener_stats::ListenerStats;

pub const CONNECTION_COUNT_CHANGED: &str = "CONNECTION_COUNT_CHANGED";
pub const CONNECTION_BYTES_TRANSFERRED_CHANGED: &str = "CONNECTION_BYTES_TRANSFERRED_CHANGED";
pub const CONNECTION_HOST_CHANGED: &str = "CONNECTION_HOST_CHANGED";
pub const LISTENER_TRANSFERRED_CHANGED: &str = "LISTENER_TRANSFERRED_CHANGED";
pub const GLOBAL_TRANSFER_CHANGED: &str = "GLOBAL_TRANSFER_CHANGED";

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    #[serde(rename = "EVENT_TYPE")]
    pub event_type: &'static str,
    #[serde(rename = "EVENT_PAYLOAD")]
    pub event_payload: serde_json::Value,
}

fn sender() -> &'static broadcast::Sender<Event> {
    static SENDER: OnceLock<broadcast::Sender<Event>> = OnceLock::new();
    SENDER.get_or_init(|| broadcast::channel(4096).0)
}

fn listeners() -> &'static RwLock<HashMap<String, Weak<ListenerStats>>> {
    static LISTENERS: OnceLock<RwLock<HashMap<String, Weak<ListenerStats>>>> = OnceLock::new();
    LISTENERS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Cumulative per-listener transfer counters that survive listener object
/// lifetimes. Relay hot paths hold an `Arc` to their listener's totals and
/// update them with plain atomics; the map lock is only taken on listener
/// registration and once-per-second sampling.
#[derive(Debug, Default)]
pub struct TransferTotals {
    uploaded: AtomicU64,
    downloaded: AtomicU64,
}

impl TransferTotals {
    pub fn add_uploaded(&self, count: u64) { self.uploaded.fetch_add(count, Ordering::Relaxed); }
    pub fn add_downloaded(&self, count: u64) { self.downloaded.fetch_add(count, Ordering::Relaxed); }
    fn snapshot(&self) -> (u64, u64) {
        (self.uploaded.load(Ordering::Relaxed), self.downloaded.load(Ordering::Relaxed))
    }
}

fn transferred() -> &'static std::sync::RwLock<HashMap<String, Arc<TransferTotals>>> {
    static TRANSFERRED: OnceLock<std::sync::RwLock<HashMap<String, Arc<TransferTotals>>>> = OnceLock::new();
    TRANSFERRED.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

pub fn transfer_totals(listener: &str) -> Arc<TransferTotals> {
    if let Some(existing) = transferred()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(listener)
    {
        return Arc::clone(existing);
    }
    Arc::clone(
        transferred()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(listener.to_owned())
            .or_default(),
    )
}

pub fn subscribe() -> broadcast::Receiver<Event> { sender().subscribe() }

pub fn publish(event_type: &'static str, event_payload: serde_json::Value) {
    let _ = sender().send(Event { event_type, event_payload });
}

pub fn publish_connection_count(key: &str, was: usize, now: usize) {
    publish(CONNECTION_COUNT_CHANGED, serde_json::json!({"key": key, "was": was, "now": now}));
}

pub async fn reset() { listeners().write().await.clear(); }

pub async fn register_listener(stats: &Arc<ListenerStats>) {
    listeners().write().await.insert(stats.name.clone(), Arc::downgrade(stats));
    transfer_totals(&stats.name);
}

pub async fn snapshot_events() -> Vec<Event> {
    let connections = crate::active_tracker::get_active_list().await;
    let mut counts = HashMap::<String, usize>::new();
    for connection in &connections {
        *counts.entry(connection.listener.clone()).or_default() += 1;
    }
    let registered = listeners().read().await.clone();
    let totals = transferred().read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let mut events = Vec::new();
    let mut global_uploaded = 0u64;
    let mut global_downloaded = 0u64;
    for (name, weak) in &registered {
        if weak.strong_count() == 0 { continue; }
        let now = counts.get(name).copied().unwrap_or_default();
        events.push(Event { event_type: CONNECTION_COUNT_CHANGED, event_payload: serde_json::json!({
            "key": name, "was": now, "now": now, "snapshot": true
        }) });
        let (uploaded, downloaded) = totals.get(name).map(|totals| totals.snapshot()).unwrap_or_default();
        global_uploaded = global_uploaded.saturating_add(uploaded);
        global_downloaded = global_downloaded.saturating_add(downloaded);
        events.push(Event { event_type: LISTENER_TRANSFERRED_CHANGED, event_payload: serde_json::json!({
            "key": name, "uploaded": uploaded, "downloaded": downloaded, "snapshot": true
        }) });
    }
    events.push(Event { event_type: CONNECTION_COUNT_CHANGED, event_payload: serde_json::json!({
        "key": "__global__", "was": connections.len(), "now": connections.len(), "snapshot": true
    }) });
    events.push(Event { event_type: GLOBAL_TRANSFER_CHANGED, event_payload: serde_json::json!({
        "key": "__global__", "uploaded": global_uploaded, "downloaded": global_downloaded, "snapshot": true
    }) });
    events
}

pub fn spawn_sampler(controller: &mut crate::controller::Controller) {
    controller.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let connections = crate::active_tracker::get_active_list().await;
            let mut connection_counts = HashMap::<String, usize>::new();
            for connection in &connections {
                *connection_counts.entry(connection.listener.clone()).or_default() += 1;
                publish(CONNECTION_BYTES_TRANSFERRED_CHANGED, serde_json::json!({
                    "key": format!("{}:{}", connection.listener, connection.request_id),
                    "listener_name": connection.listener, "connection_id": connection.request_id,
                    "uploaded": connection.uploaded_bytes, "downloaded": connection.downloaded_bytes
                }));
            }
            let registered = listeners().read().await.clone();
            for (name, weak) in &registered {
                if weak.strong_count() == 0 { continue; }
                let now = connection_counts.get(name).copied().unwrap_or_default();
                publish(CONNECTION_COUNT_CHANGED, serde_json::json!({
                    "key": name, "was": now, "now": now, "snapshot": true
                }));
            }
            publish(CONNECTION_COUNT_CHANGED, serde_json::json!({
                "key": "__global__", "was": connections.len(), "now": connections.len(), "snapshot": true
            }));
            let totals = transferred().read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            let mut global_uploaded = 0u64;
            let mut global_downloaded = 0u64;
            for (name, totals) in &totals {
                if !registered.get(name).is_some_and(|weak| weak.strong_count() > 0) { continue; }
                let (uploaded, downloaded) = totals.snapshot();
                global_uploaded = global_uploaded.saturating_add(uploaded);
                global_downloaded = global_downloaded.saturating_add(downloaded);
                publish(LISTENER_TRANSFERRED_CHANGED, serde_json::json!({
                    "key": name, "uploaded": uploaded, "downloaded": downloaded
                }));
            }
            publish(GLOBAL_TRANSFER_CHANGED, serde_json::json!({
                "key": "__global__", "uploaded": global_uploaded, "downloaded": global_downloaded
            }));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_envelope_uses_required_uppercase_fields() {
        let value = serde_json::to_value(Event {
            event_type: GLOBAL_TRANSFER_CHANGED,
            event_payload: serde_json::json!({"key":"__global__","uploaded":1,"downloaded":2}),
        }).unwrap();
        assert_eq!(value["EVENT_TYPE"], GLOBAL_TRANSFER_CHANGED);
        assert_eq!(value["EVENT_PAYLOAD"]["key"], "__global__");
        assert!(value.get("event_type").is_none());
    }
}
