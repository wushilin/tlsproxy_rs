use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::accounting::{ConnStatus, ListenerType};
use crate::request_id::RequestId;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};

// The tracker uses a std (blocking) RwLock, not a tokio one: its critical
// sections are tiny map operations, and being synchronous lets the RAII
// connection guard clean up from `Drop` (which cannot await) on any exit,
// including forced task cancellation.

#[derive(Debug, Clone)]
struct ActiveConnection {
    listener: String,
    remote_address: SocketAddr,
    started_at: Instant,
    started_at_unix_ms: u128,
    uploaded_bytes: u64,
    downloaded_bytes: u64,
    sni: String,
    target_host: String,
    target_endpoint: String,
    status: ConnStatus,
    listener_type: Option<ListenerType>,
}

fn active() -> std::sync::RwLockWriteGuard<'static, HashMap<RequestId, ActiveConnection>> {
    ACTIVE.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveConnectionSerde {
    pub request_id: String,
    pub listener: String,
    pub remote_address: String,
    pub host: String,
    pub uptime_ms: u128,
    pub started_at_unix_ms: u128,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
}

lazy_static! {
    static ref ACTIVE: Arc<RwLock<HashMap<RequestId, ActiveConnection>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

pub fn reset() {
    active().clear()
}

pub fn put(request_id: &RequestId, listener: &str, addr: SocketAddr) {
    let mut w = active();
    let global_was = w.len();
    let listener_was = w.values().filter(|active| active.listener == listener).count();
    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    w.insert(
        request_id.clone(),
        ActiveConnection {
            listener: listener.into(),
            remote_address: addr,
            started_at: Instant::now(),
            started_at_unix_ms,
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            sni: String::new(),
            target_host: String::new(),
            target_endpoint: String::new(),
            status: ConnStatus::default(),
            listener_type: None,
        },
    );
    let global_now = w.len();
    let listener_now = w.values().filter(|active| active.listener == listener).count();
    drop(w);
    // Coarse per-listener count → overview (global topic). The open event
    // carries a connection_id and goes to both: the global count for the
    // overview and the listener's topic to add the live-view row.
    crate::events_hub::publish_connection_count(listener, listener_was, listener_now);
    crate::events_hub::publish_both(listener, crate::events_hub::CONNECTION_COUNT_CHANGED, serde_json::json!({
        "key": "__global__", "was": global_was, "now": global_now, "state": "open",
        "listener_name": listener, "connection_id": request_id.to_string(),
        "remote_address": addr.to_string(), "started_at_unix_ms": started_at_unix_ms,
        "uploaded": 0, "downloaded": 0
    }));
}

pub fn add_uploaded(request_id: &RequestId, count: u64) {
    if let Some(connection) = active().get_mut(request_id) {
        connection.uploaded_bytes = connection.uploaded_bytes.saturating_add(count);
    }
}

pub fn add_downloaded(request_id: &RequestId, count: u64) {
    if let Some(connection) = active().get_mut(request_id) {
        connection.downloaded_bytes = connection.downloaded_bytes.saturating_add(count);
    }
}

pub fn set_sni(request_id: &RequestId, sni: &str) {
    let listener = {
        let mut active = active();
        let Some(connection) = active.get_mut(request_id) else { return };
        connection.sni = sni.to_string();
        connection.listener.clone()
    };
    // Per-connection host detail → the listener's live view only.
    crate::events_hub::publish_listener(&listener, crate::events_hub::CONNECTION_HOST_CHANGED, serde_json::json!({
        "key": format!("{listener}:{}", request_id), "listener_name": listener,
        "connection_id": request_id.to_string(), "host": sni
    }));
}

pub fn set_target(request_id: &RequestId, target_host: &str, target_endpoint: &str) {
    if let Some(connection) = active().get_mut(request_id) {
        connection.target_host = target_host.to_string();
        connection.target_endpoint = target_endpoint.to_string();
    }
}

pub fn set_status(request_id: &RequestId, status: ConnStatus) {
    if let Some(connection) = active().get_mut(request_id) {
        connection.status = status;
    }
}

/// Records the accounting listener type so the connection guard can emit an
/// accurate CDR from Drop without the exit path threading it through.
pub fn set_listener_type(request_id: &RequestId, listener_type: ListenerType) {
    if let Some(connection) = active().get_mut(request_id) {
        connection.listener_type = Some(listener_type);
    }
}

/// Final connection details handed to accounting when a connection ends.
#[derive(Debug, Clone)]
pub struct ClosedConnection {
    pub listener: String,
    pub remote_address: SocketAddr,
    pub started_at_unix_ms: u128,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub sni: String,
    pub target_host: String,
    pub target_endpoint: String,
    pub status: ConnStatus,
    pub listener_type: Option<ListenerType>,
}

pub fn remove(request_id: &RequestId) -> Option<ClosedConnection> {
    let mut w = active();
    let global_was = w.len();
    let listener = w.get(request_id).map(|active| active.listener.clone())?;
    let listener_was = w.values().filter(|active| active.listener == listener).count();
    let active = w.remove(request_id)?;
    let global_now = w.len();
    let listener_now = w.values().filter(|active| active.listener == listener).count();
    drop(w);
    crate::events_hub::publish_connection_count(&listener, listener_was, listener_now);
    crate::events_hub::publish_both(&listener, crate::events_hub::CONNECTION_COUNT_CHANGED, serde_json::json!({
        "key": "__global__", "was": global_was, "now": global_now, "state": "closed",
        "listener_name": listener, "connection_id": request_id.to_string(),
        "remote_address": active.remote_address.to_string(),
        "started_at_unix_ms": active.started_at_unix_ms,
        "host": active.sni.clone(), "uploaded": active.uploaded_bytes,
        "downloaded": active.downloaded_bytes
    }));
    Some(ClosedConnection {
        listener: active.listener,
        remote_address: active.remote_address,
        started_at_unix_ms: active.started_at_unix_ms,
        uploaded_bytes: active.uploaded_bytes,
        downloaded_bytes: active.downloaded_bytes,
        sni: active.sni,
        target_host: active.target_host,
        target_endpoint: active.target_endpoint,
        status: active.status,
        listener_type: active.listener_type,
    })
}

pub fn get_active_list() -> Vec<ActiveConnectionSerde> {
    let r = ACTIVE.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut result: Vec<_> = r
        .iter()
        .map(|(id, active)| ActiveConnectionSerde {
            request_id: id.to_string(),
            listener: active.listener.clone(),
            remote_address: active.remote_address.to_string(),
            host: active.sni.clone(),
            uptime_ms: active.started_at.elapsed().as_millis(),
            started_at_unix_ms: active.started_at_unix_ms,
            uploaded_bytes: active.uploaded_bytes,
            downloaded_bytes: active.downloaded_bytes,
        })
        .collect();
    result.sort_by(|a, b| {
        a.listener
            .cmp(&b.listener)
            .then(a.started_at_unix_ms.cmp(&b.started_at_unix_ms))
            .then(a.request_id.cmp(&b.request_id))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_returns_accumulated_connection_details() {
        let request_id = RequestId::new();
        let addr: SocketAddr = "10.1.2.3:44444".parse().unwrap();
        put(&request_id, "listener-a", addr);
        add_uploaded(&request_id, 100);
        add_downloaded(&request_id, 200);
        set_sni(&request_id, "example.com");
        set_target(&request_id, "example.com", "1.2.3.4:443");
        set_status(&request_id, ConnStatus::Ok);
        let closed = remove(&request_id)
            .expect("connection should be tracked");
        assert_eq!(closed.listener, "listener-a");
        assert_eq!(closed.remote_address, addr);
        assert_eq!(closed.uploaded_bytes, 100);
        assert_eq!(closed.downloaded_bytes, 200);
        assert_eq!(closed.sni, "example.com");
        assert_eq!(closed.target_host, "example.com");
        assert_eq!(closed.target_endpoint, "1.2.3.4:443");
        assert_eq!(closed.status, ConnStatus::Ok);
        assert!(closed.started_at_unix_ms > 0);
        assert!(
            remove(&request_id).is_none(),
            "second remove returns None"
        );
    }

    #[test]
    fn new_connections_default_to_connect_failed_with_empty_dimensions() {
        let request_id = RequestId::new();
        let addr: SocketAddr = "10.1.2.3:44445".parse().unwrap();
        put(&request_id, "listener-b", addr);
        let closed = remove(&request_id).unwrap();
        assert_eq!(closed.status, ConnStatus::ConnectFailed);
        assert_eq!(closed.sni, "");
        assert_eq!(closed.target_host, "");
        assert_eq!(closed.target_endpoint, "");
    }

    #[test]
    fn setters_ignore_unknown_request_ids() {
        let request_id = RequestId::new();
        set_sni(&request_id, "ghost");
        set_status(&request_id, ConnStatus::Denied);
        assert!(remove(&request_id).is_none());
    }

    #[tokio::test]
    async fn connection_open_and_close_publish_listener_and_global_counts() {
        async fn next_matching(
            events: &crate::events_hub::EventReceiver,
            predicate: impl Fn(&serde_json::Value) -> bool,
        ) -> crate::events_hub::Event {
            loop {
                let event = events.recv_async().await.unwrap();
                if predicate(&event.event_payload) { return event; }
            }
        }
        // No global reset here: the tracker is shared process state and a
        // reset wipes entries of concurrently running tests. The assertions
        // use a listener name unique to this test instead.
        let events = crate::events_hub::subscribe_global();
        let request_id = RequestId::new();
        put(&request_id, "events-listener", "127.0.0.1:12345".parse().unwrap());
        let listener_open = next_matching(&events, |payload| payload["key"] == "events-listener" && payload["now"] == 1).await;
        let global_open = next_matching(&events, |payload| payload["key"] == "__global__" && payload["now"].as_u64() == payload["was"].as_u64().map(|was| was + 1)).await;
        assert_eq!(listener_open.event_payload, serde_json::json!({"key":"events-listener","was":0,"now":1}));
        assert_eq!(global_open.event_type, crate::events_hub::CONNECTION_COUNT_CHANGED);
        remove(&request_id).unwrap();
        let listener_close = next_matching(&events, |payload| payload["key"] == "events-listener" && payload["now"] == 0).await;
        let global_close = next_matching(&events, |payload| payload["key"] == "__global__" && payload["was"].as_u64() == payload["now"].as_u64().map(|now| now + 1)).await;
        assert_eq!(listener_close.event_payload, serde_json::json!({"key":"events-listener","was":1,"now":0}));
        assert_eq!(global_close.event_type, crate::events_hub::CONNECTION_COUNT_CHANGED);
    }
}
