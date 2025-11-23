use std::collections::{BTreeMap, HashMap};

use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::config::Config;
use std::cmp::Ordering;
use log::info;

lazy_static! {
    static ref CONFIG: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));
    static ref SUFFIX: Arc<RwLock<BTreeMap<LenKey, String>>> = Arc::new(RwLock::new(BTreeMap::new()));
    static ref REGEX: Arc<RwLock<Vec<(Regex, String)>>> = Arc::new(RwLock::new(vec![]));
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct LenKey(String);

impl Ord for LenKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // longest first
        other.0.len().cmp(&self.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for LenKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
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
        .filter(|(k, _v)| 
        !k.starts_with("suffix:") && !k.starts_with("regex:"))
        .collect();

    let suffix_lc:HashMap<String, String> = new_lc.clone().into_iter()
        .filter(|(k, _v)| k.starts_with("suffix:"))
        .collect();

    let regex_lc:HashMap<String, String> = new_lc.clone().into_iter()
        .filter(|(k, _v)| k.starts_with("regex:"))
        .collect();

    config_1.extend(direct_lc.clone());

    let mut suffix_1 = SUFFIX.write().await;
    suffix_1.clear();
    for(key, value) in &suffix_lc{
        let new_key = &key[7..];
        info!("Adding DNS by suffix {} -> {}", new_key, value);
        suffix_1.insert(LenKey(new_key.into()), value.clone());
    }

    let mut regex_1 = REGEX.write().await;
    regex_1.clear();
    for(regex, value) in &regex_lc{
        let new_regex_str = regex["regex:".len()..].to_string();
        let new_regex = RegexBuilder::new(&new_regex_str)
            .case_insensitive(true)
            .build()
            .unwrap();
        info!("Adding DNS by regex {} -> {}", regex, value);
        regex_1.push((new_regex, value.clone()));
    }
}

// Resolve return resolved address, and a boolean indicating if actual resolution happened
pub async fn resolve(host:&str) -> Option<String> {
    let host_lc = host.to_lowercase();
    let result = CONFIG.read().await;
    let direct_result = result.get(&host_lc);
    match direct_result {
        Some(inner) => {
            info!("DNS direct match {} -> {}", host_lc, inner);
            return Some(inner.into());
        },
        None => {
        }
    }

    let suffix_1 = SUFFIX.read().await;
    for(key, value) in &*suffix_1 {
        if host_lc.ends_with(&key.0) {
            info!("DNS suffix match {} -> {} by suffix `{}`", host_lc, value, key.0);
            return Some(value.into());
        }
    }
    drop(suffix_1);

    let regex_1 = REGEX.read().await;
    for(regex, value) in &*regex_1 {
        if regex.is_match(&host_lc) {
            info!("DNS regex match {} -> {} by regex `{}`", host_lc, value, regex);
            return Some(value.into());
        }
    }
    drop(regex_1);
    return None;
}
