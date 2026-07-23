use std::sync::{Arc, OnceLock};

use arc_swap::{ArcSwap, ArcSwapOption};

use crate::runtime_config::RuntimeConfig;
use crate::store::StoredConfig;

fn current() -> &'static ArcSwap<RuntimeConfig> {
    static CURRENT: OnceLock<ArcSwap<RuntimeConfig>> = OnceLock::new();
    CURRENT.get_or_init(|| ArcSwap::from_pointee(RuntimeConfig::default()))
}

pub fn store(config: RuntimeConfig) {
    current().store(Arc::new(config));
}

pub fn load() -> Arc<RuntimeConfig> {
    current().load_full()
}

/// The newest revision proven to work, either by a full runtime reload or by a
/// successful hot apply. The runtime rollback path consumes it so a failing
/// revision reverts to the latest working state, not to the last full reload.
fn last_good() -> &'static ArcSwapOption<StoredConfig> {
    static LAST_GOOD: OnceLock<ArcSwapOption<StoredConfig>> = OnceLock::new();
    LAST_GOOD.get_or_init(ArcSwapOption::empty)
}

pub fn store_last_good(stored: StoredConfig) {
    // Monotonic by revision: when a hot-applied revision proves newer state
    // works, the runtime loop finishing its older revision must not regress
    // the slot.
    if last_good().load().as_ref().is_some_and(|existing| existing.revision >= stored.revision) {
        return;
    }
    last_good().store(Some(Arc::new(stored)));
}

/// Removes and returns the last known-good revision; taking it ensures a
/// rollback that itself fails cannot loop forever.
pub fn take_last_good() -> Option<StoredConfig> {
    last_good().swap(None).map(|stored| (*stored).clone())
}

/// Only listener-internal settings can be swapped without rebuilding sockets
/// and background services. Bind/protocol/start-state or global changes still
/// use the normal revision reload path.
pub fn listener_settings_only(old: &RuntimeConfig, new: &RuntimeConfig) -> bool {
    fn topology(config: &RuntimeConfig) -> serde_json::Value {
        let mut value = serde_json::to_value(config).expect("runtime config serializes");
        let object = value.as_object_mut().expect("runtime config is an object");
        if let Some(default) = object.get_mut("default_listener").and_then(|v| v.as_object_mut()) {
            default.remove("ordinary_traffic");
        }
        if let Some(listeners) = object.get_mut("additional_listeners").and_then(|v| v.as_object_mut()) {
            for listener in listeners.values_mut() {
                let protocol = listener.get("protocol").cloned().unwrap_or_default();
                let bind = listener.get("bind").cloned().unwrap_or_default();
                *listener = serde_json::json!({"protocol": protocol, "bind": bind});
            }
        }
        value
    }
    topology(old) == topology(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_hot_but_bind_and_stopped_state_are_not() {
        let old = RuntimeConfig::default();
        let mut rules = old.clone();
        rules.default_listener.ordinary_traffic.max_idle_time_ms = Some(1000);
        assert!(listener_settings_only(&old, &rules));
        let mut bind = old.clone();
        bind.default_listener.bind = "127.0.0.1:443".into();
        assert!(!listener_settings_only(&old, &bind));
        let mut stopped = old.clone();
        stopped.disabled_listeners.insert("test".into());
        assert!(!listener_settings_only(&old, &stopped));
    }
}
