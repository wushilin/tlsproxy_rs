use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use serde::Serialize;
use tokio::sync::RwLock;

use crate::listener_stats::ListenerStats;

/// Named broadcast topics. Deltas are published straight to the topic their
/// consumer reads, so no consumer-side filtering is needed:
///
/// - the global topic carries coarse aggregates the overview needs
///   (per-listener connection counts and transfer totals, plus the global
///   roll-ups);
/// - each listener has its own topic carrying that listener's fine-grained
///   per-connection events and the periodic authoritative connection-list
///   snapshot the live view reconciles against.
///
/// The overview WebSocket subscribes to the global topic; a listener-live
/// WebSocket subscribes to exactly one listener topic.
const GLOBAL_TOPIC: &str = "tlsproxy.events.global";
const EVENTS_CAPACITY: usize = 4096;

pub type EventReceiver = named_queue::Receiver<Event>;

fn listener_topic(name: &str) -> String {
    format!("tlsproxy.events.listener.{name}")
}

pub const CONNECTION_COUNT_CHANGED: &str = "CONNECTION_COUNT_CHANGED";
pub const CONNECTION_BYTES_TRANSFERRED_CHANGED: &str = "CONNECTION_BYTES_TRANSFERRED_CHANGED";
pub const CONNECTION_HOST_CHANGED: &str = "CONNECTION_HOST_CHANGED";
pub const LISTENER_TRANSFERRED_CHANGED: &str = "LISTENER_TRANSFERRED_CHANGED";
pub const GLOBAL_TRANSFER_CHANGED: &str = "GLOBAL_TRANSFER_CHANGED";
/// Authoritative full set of a listener's live connections. Because the
/// broadcast is drop-oldest, a lagging subscriber can miss an open/close and
/// drift; this snapshot lets the live view rebuild its table from truth.
pub const LISTENER_CONNECTIONS_SNAPSHOT: &str = "LISTENER_CONNECTIONS_SNAPSHOT";

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    #[serde(rename = "EVENT_TYPE")]
    pub event_type: &'static str,
    #[serde(rename = "EVENT_PAYLOAD")]
    pub event_payload: serde_json::Value,
}

/// Registers a broadcasting topic the first time it is used. Topics are
/// created lazily and never destroyed: a renamed or removed listener leaves an
/// idle topic behind, which is cheap and avoids tearing a topic out from under
/// a live subscriber.
fn ensure_topic(topic: &str) {
    static REGISTERED: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let registered = REGISTERED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut set = registered.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if set.insert(topic.to_owned()) {
        let _ = named_queue::create_broadcasting::<Event>(topic, EVENTS_CAPACITY);
    }
}

fn senders() -> &'static std::sync::RwLock<HashMap<String, named_queue::Sender<Event>>> {
    static SENDERS: OnceLock<std::sync::RwLock<HashMap<String, named_queue::Sender<Event>>>> = OnceLock::new();
    SENDERS.get_or_init(Default::default)
}

fn send_to(topic: &str, event: Event) {
    if let Some(sender) = senders().read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(topic) {
        let _ = sender.send(event);
        return;
    }
    ensure_topic(topic);
    let sender = named_queue::acquire_sender::<Event>(topic).expect("topic is registered");
    let _ = sender.send(event);
    senders().write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(topic.to_owned(), sender);
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

/// Subscribes to the global (overview) topic. Each call is an independent
/// broadcast stream; callers must not clone the receiver to fan out (clones
/// share one subscription) — subscribe again instead.
pub fn subscribe_global() -> EventReceiver {
    ensure_topic(GLOBAL_TOPIC);
    named_queue::acquire_receiver::<Event>(GLOBAL_TOPIC).expect("global topic is registered")
}

/// Subscribes to one listener's topic (its per-connection events + snapshots).
pub fn subscribe_listener(listener: &str) -> EventReceiver {
    let topic = listener_topic(listener);
    ensure_topic(&topic);
    named_queue::acquire_receiver::<Event>(&topic).expect("listener topic is registered")
}

/// Publishes a coarse aggregate to the global (overview) topic.
pub fn publish_global(event_type: &'static str, event_payload: serde_json::Value) {
    send_to(GLOBAL_TOPIC, Event { event_type, event_payload });
}

/// Publishes a fine-grained event to one listener's topic.
pub fn publish_listener(listener: &str, event_type: &'static str, event_payload: serde_json::Value) {
    send_to(&listener_topic(listener), Event { event_type, event_payload });
}

/// Publishes to both the global topic (for the overview) and the listener's
/// topic (for its live view) — used by events that update an aggregate and a
/// live row at once, e.g. a connection opening or closing.
pub fn publish_both(listener: &str, event_type: &'static str, event_payload: serde_json::Value) {
    let event = Event { event_type, event_payload };
    send_to(GLOBAL_TOPIC, event.clone());
    send_to(&listener_topic(listener), event);
}

pub fn publish_connection_count(listener: &str, was: usize, now: usize) {
    publish_global(CONNECTION_COUNT_CHANGED, serde_json::json!({"key": listener, "was": was, "now": now}));
}

/// Builds the authoritative connection-list snapshot for one listener.
fn listener_connections_event(listener: &str, connections: &[crate::active_tracker::ActiveConnectionSerde]) -> Event {
    let items: Vec<_> = connections.iter().filter(|item| item.listener == listener).map(|item| serde_json::json!({
        "connection_id": item.request_id, "remote_address": item.remote_address, "host": item.host,
        "started_at_unix_ms": item.started_at_unix_ms, "uploaded": item.uploaded_bytes, "downloaded": item.downloaded_bytes
    })).collect();
    Event { event_type: LISTENER_CONNECTIONS_SNAPSHOT, event_payload: serde_json::json!({
        "listener_name": listener, "connections": items, "snapshot": true
    }) }
}

pub async fn reset() { listeners().write().await.clear(); }

pub async fn register_listener(stats: &Arc<ListenerStats>) {
    listeners().write().await.insert(stats.name.clone(), Arc::downgrade(stats));
    transfer_totals(&stats.name);
    ensure_topic(&listener_topic(&stats.name));
}

/// Initial state for a freshly connected overview socket: per-listener counts
/// and transfer totals plus the global roll-ups.
pub async fn snapshot_global() -> Vec<Event> {
    let connections = crate::active_tracker::get_active_list();
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

/// Initial state for a freshly connected listener-live socket: the
/// authoritative connection list plus that listener's transfer total.
pub fn snapshot_listener(listener: &str) -> Vec<Event> {
    let connections = crate::active_tracker::get_active_list();
    let (uploaded, downloaded) = transferred().read().unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(listener).map(|totals| totals.snapshot()).unwrap_or_default();
    vec![
        listener_connections_event(listener, &connections),
        Event { event_type: LISTENER_TRANSFERRED_CHANGED, event_payload: serde_json::json!({
            "key": listener, "uploaded": uploaded, "downloaded": downloaded, "snapshot": true
        }) },
    ]
}

/// Periodically republishes each listener's authoritative connection list so
/// live views self-correct from any events dropped under broadcast lag.
pub fn spawn_connection_reconciler(controller: &mut crate::controller::Controller) {
    controller.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let connections = crate::active_tracker::get_active_list();
            let registered = listeners().read().await.clone();
            for (name, weak) in &registered {
                if weak.strong_count() == 0 { continue; }
                send_to(&listener_topic(name), listener_connections_event(name, &connections));
            }
        }
    });
}

pub fn spawn_sampler(controller: &mut crate::controller::Controller) {
    controller.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let connections = crate::active_tracker::get_active_list();
            let mut connection_counts = HashMap::<String, usize>::new();
            for connection in &connections {
                *connection_counts.entry(connection.listener.clone()).or_default() += 1;
                // Per-connection detail: only its listener's live view wants it.
                publish_listener(&connection.listener, CONNECTION_BYTES_TRANSFERRED_CHANGED, serde_json::json!({
                    "key": format!("{}:{}", connection.listener, connection.request_id),
                    "listener_name": connection.listener, "connection_id": connection.request_id,
                    "uploaded": connection.uploaded_bytes, "downloaded": connection.downloaded_bytes
                }));
            }
            let registered = listeners().read().await.clone();
            for (name, weak) in &registered {
                if weak.strong_count() == 0 { continue; }
                let now = connection_counts.get(name).copied().unwrap_or_default();
                publish_global(CONNECTION_COUNT_CHANGED, serde_json::json!({
                    "key": name, "was": now, "now": now, "snapshot": true
                }));
            }
            publish_global(CONNECTION_COUNT_CHANGED, serde_json::json!({
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
                // Overview column (global topic) and the listener's live metrics.
                publish_both(name, LISTENER_TRANSFERRED_CHANGED, serde_json::json!({
                    "key": name, "uploaded": uploaded, "downloaded": downloaded
                }));
            }
            publish_global(GLOBAL_TRANSFER_CHANGED, serde_json::json!({
                "key": "__global__", "uploaded": global_uploaded, "downloaded": global_downloaded
            }));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn topic_routing_separates_global_and_listener_streams() {
        let listener_name = "route-test-listener";
        // Subscribe before publishing — broadcast has no history.
        let global = subscribe_global();
        let listener = subscribe_listener(listener_name);
        // Coarse per-listener count is overview-only → global topic.
        publish_global(CONNECTION_COUNT_CHANGED, serde_json::json!({"key": listener_name, "was": 0, "now": 1}));
        // Per-connection host change is live-view-only → listener topic.
        publish_listener(listener_name, CONNECTION_HOST_CHANGED, serde_json::json!({"connection_id": "rc1", "host": "h"}));

        // The listener topic's first event is the fine one: the coarse count
        // published earlier went to the global topic only, proving the split.
        let first = listener.recv_async().await.unwrap();
        assert_eq!(first.event_type, CONNECTION_HOST_CHANGED);
        assert_eq!(first.event_payload["connection_id"], "rc1");

        // The global topic carries the coarse count. Match by key to ignore
        // events other parallel tests publish to the shared global topic.
        loop {
            let event = global.recv_async().await.unwrap();
            if event.event_payload["key"] == listener_name {
                assert_eq!(event.event_type, CONNECTION_COUNT_CHANGED);
                break;
            }
        }
    }

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
