use std::collections::HashMap;

use lazy_static::lazy_static;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::config::Config;
use log::info;

lazy_static! {
    static ref CONFIG: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));
    static ref SUFFIX: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));
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
    // Convert it to a lower cased key map first
    let new_lc:HashMap<String, String> = new.clone().into_iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect();

    let direct_lc:HashMap<String, String> = new_lc.clone().into_iter()
        .filter(|(k, _v)| !k.starts_with("suffix:"))
        .collect();

    let suffix_lc:HashMap<String, String> = new_lc.clone().into_iter()
        .filter(|(k, _v)| k.starts_with("suffix:"))
        .collect();

    config_1.extend(direct_lc.clone());

    let mut suffix_1 = SUFFIX.write().await;
    suffix_1.clear();
    for(key, value) in &suffix_lc{
        let new_key = &key[7..];
        info!("Adding DNS by suffix {} -> {}", new_key, value);
        suffix_1.insert(new_key.into(), value.clone());
    }
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
            let suffix_1 = SUFFIX.read().await;
            for(key, value) in &*suffix_1 {
                let host_lc = host.to_lowercase();
                if host_lc.ends_with(key) {
                    return Some(value.into());
                }
            }
            return None;
        }
    }
}
