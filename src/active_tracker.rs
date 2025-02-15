use std::{collections::HashMap, net::SocketAddr, sync::Arc};

// Active tracker tracks the active connection for debugging purpose.
use crate::request_id::RequestId;
use lazy_static::lazy_static;
use tokio::sync::RwLock;

lazy_static! {
    static ref ACTIVE: Arc<RwLock<HashMap<RequestId, SocketAddr>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

pub async fn reset() {
    ACTIVE.write().await.clear()
}

pub async fn put(request_id: &RequestId, addr: SocketAddr) {
    let mut w = ACTIVE.write().await;
    w.insert(request_id.clone(), addr);
}

pub async fn remove(request_id: &RequestId) {
    let mut w = ACTIVE.write().await;
    w.remove(request_id);
}

pub async fn get_active_list() -> Vec<(RequestId, SocketAddr)> {
    let mut result = Vec::new();
    let r = ACTIVE.read().await;
    for (id, addr) in r.iter() {
        result.push((id.clone(), *addr));
    }
    return result;
}
