//! RAII ownership of a connection's global lifecycle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::accounting::{self, CdrRecord, ListenerType};
use crate::listener_stats::ListenerStats;
use crate::request_id::RequestId;

/// Owns one connection's presence in the global registries for the duration of
/// its handler task. Construction registers the connection (active-tracker
/// entry and active-count increment); `Drop` deregisters it (CDR emission,
/// active-count decrement, tracker removal).
///
/// Cleanup living in `Drop` is deliberate: a handler task that is force
/// cancelled — a configuration reload or a per-listener restart dropping the
/// task — still runs `Drop`, so counts and the tracker never leak and a CDR is
/// still emitted. This replaces the previous hand-paired track-start/track-end
/// calls, whose end half was skipped on cancellation.
pub struct ConnGuard {
    request_id: Arc<RequestId>,
    listener: Arc<String>,
    stats: Arc<ListenerStats>,
    fallback_type: ListenerType,
    started: Instant,
}

impl ConnGuard {
    /// Registers a freshly accepted connection. `fallback_type` is used for the
    /// CDR when the handler never reaches the point of classifying the
    /// connection (e.g. it fails before a route is chosen, or is cancelled).
    pub fn start(
        request_id: Arc<RequestId>,
        listener: Arc<String>,
        remote: SocketAddr,
        stats: Arc<ListenerStats>,
        fallback_type: ListenerType,
    ) -> Self {
        crate::active_tracker::put(&request_id, &listener, remote);
        stats.increase_conn_count();
        Self {
            request_id,
            listener,
            stats,
            fallback_type,
            started: Instant::now(),
        }
    }

    pub fn request_id(&self) -> &Arc<RequestId> {
        &self.request_id
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(closed) = crate::active_tracker::remove(&self.request_id) {
            if accounting::enabled() {
                let end_unix_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or_default();
                accounting::submit(CdrRecord {
                    listener_type: closed.listener_type.unwrap_or(self.fallback_type),
                    connection_id: self.request_id.to_string(),
                    listener_name: (*self.listener).clone(),
                    sni: closed.sni,
                    target_host: closed.target_host,
                    target_endpoint: closed.target_endpoint,
                    remote_address: closed.remote_address.to_string(),
                    status: closed.status,
                    uploaded_bytes: closed.uploaded_bytes,
                    downloaded_bytes: closed.downloaded_bytes,
                    start_unix_ms: closed.started_at_unix_ms,
                    end_unix_ms,
                });
            }
        }
        self.stats.decrease_conn_count();
    }
}
