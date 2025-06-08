use std::collections::HashMap;

use lazy_static::lazy_static;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::config::Config;
use log::info;

lazy_static! {
    static ref CONFIG: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));
}

pub async fn init(config:&Config) {
    info!("initializing DNS override");
    let dns = &config.dns;
    init_inner(dns.clone()).await;
    info!("initialized DNS override. {} entries loaded", dns.len());
}

async fn init_inner(new:HashMap<String, String>) {
    let mut config_1 = CONFIG.write().await;
    config_1.clear();
    config_1.extend(new.clone());
}

// Resolve return resolved address, and a boolean indicating if actual resolution happened
pub async fn resolve(host:&str) -> Option<String> {
    let result = CONFIG.read().await;
    let direct_result = result.get(host);
    match direct_result {
        Some(inner) => {
            return Some(inner.into());
        },
        None => {
            let default_result = result.get("default")?;
            return Some(default_result.into());
        }
    }
}
