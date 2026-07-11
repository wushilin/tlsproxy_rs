use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

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

pub async fn remove(request_id: &RequestId) {
    let mut w = ACTIVE.write().await;
    w.remove(request_id);
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
