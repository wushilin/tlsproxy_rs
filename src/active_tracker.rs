use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::accounting::ConnStatus;
use crate::request_id::RequestId;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveConnectionSerde {
    pub request_id: String,
    pub listener: String,
    pub remote_address: String,
    pub uptime_ms: u128,
    pub started_at_unix_ms: u128,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
}

lazy_static! {
    static ref ACTIVE: Arc<RwLock<HashMap<RequestId, ActiveConnection>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

pub async fn reset() {
    ACTIVE.write().await.clear()
}

pub async fn put(request_id: &RequestId, listener: &str, addr: SocketAddr) {
    let mut w = ACTIVE.write().await;
    w.insert(
        request_id.clone(),
        ActiveConnection {
            listener: listener.into(),
            remote_address: addr,
            started_at: Instant::now(),
            started_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            sni: String::new(),
            target_host: String::new(),
            target_endpoint: String::new(),
            status: ConnStatus::default(),
        },
    );
}

pub async fn add_uploaded(request_id: &RequestId, count: u64) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.uploaded_bytes = active.uploaded_bytes.saturating_add(count);
    }
}

pub async fn add_downloaded(request_id: &RequestId, count: u64) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.downloaded_bytes = active.downloaded_bytes.saturating_add(count);
    }
}

pub async fn set_sni(request_id: &RequestId, sni: &str) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.sni = sni.to_string();
    }
}

pub async fn set_target(request_id: &RequestId, target_host: &str, target_endpoint: &str) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.target_host = target_host.to_string();
        active.target_endpoint = target_endpoint.to_string();
    }
}

pub async fn set_status(request_id: &RequestId, status: ConnStatus) {
    if let Some(active) = ACTIVE.write().await.get_mut(request_id) {
        active.status = status;
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
}

pub async fn remove(request_id: &RequestId) -> Option<ClosedConnection> {
    let mut w = ACTIVE.write().await;
    w.remove(request_id).map(|active| ClosedConnection {
        listener: active.listener,
        remote_address: active.remote_address,
        started_at_unix_ms: active.started_at_unix_ms,
        uploaded_bytes: active.uploaded_bytes,
        downloaded_bytes: active.downloaded_bytes,
        sni: active.sni,
        target_host: active.target_host,
        target_endpoint: active.target_endpoint,
        status: active.status,
    })
}

pub async fn get_active_list() -> Vec<ActiveConnectionSerde> {
    let r = ACTIVE.read().await;
    let mut result: Vec<_> = r
        .iter()
        .map(|(id, active)| ActiveConnectionSerde {
            request_id: id.to_string(),
            listener: active.listener.clone(),
            remote_address: active.remote_address.to_string(),
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

    #[tokio::test]
    async fn remove_returns_accumulated_connection_details() {
        let request_id = RequestId::new();
        let addr: SocketAddr = "10.1.2.3:44444".parse().unwrap();
        put(&request_id, "listener-a", addr).await;
        add_uploaded(&request_id, 100).await;
        add_downloaded(&request_id, 200).await;
        set_sni(&request_id, "example.com").await;
        set_target(&request_id, "example.com", "1.2.3.4:443").await;
        set_status(&request_id, ConnStatus::Ok).await;
        let closed = remove(&request_id)
            .await
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
            remove(&request_id).await.is_none(),
            "second remove returns None"
        );
    }

    #[tokio::test]
    async fn new_connections_default_to_connect_failed_with_empty_dimensions() {
        let request_id = RequestId::new();
        let addr: SocketAddr = "10.1.2.3:44445".parse().unwrap();
        put(&request_id, "listener-b", addr).await;
        let closed = remove(&request_id).await.unwrap();
        assert_eq!(closed.status, ConnStatus::ConnectFailed);
        assert_eq!(closed.sni, "");
        assert_eq!(closed.target_host, "");
        assert_eq!(closed.target_endpoint, "");
    }

    #[tokio::test]
    async fn setters_ignore_unknown_request_ids() {
        let request_id = RequestId::new();
        set_sni(&request_id, "ghost").await;
        set_status(&request_id, ConnStatus::Denied).await;
        assert!(remove(&request_id).await.is_none());
    }
}
