//! The data plane's own vocabulary for per-listener relay policy.
//!
//! Derived from the RocksDB runtime configuration at dispatch time; the data
//! plane never sees the legacy YAML-era `config::Listener`, which survives
//! only inside the explicit migration command.

use crate::runtime_config::{DefaultListenerConfig, TlsRouteAction, UpstreamTransport};

/// What a data-plane connection is allowed to cost and where it may go.
#[derive(Debug, Clone)]
pub struct RelayPolicy {
    /// The listener's bind pattern, used to detect self-connection loops.
    pub bind: String,
    /// Explicit backend list (`host:port,host:port`) for forward/http
    /// listeners; `None` when the listener routes dynamically by SNI/Host.
    pub target: Option<String>,
    /// Default upstream port when the route does not name one.
    pub target_port: u16,
    /// Bytes per second per connection; `None` or `0` means unlimited.
    pub speed_limit: Option<f64>,
    /// Encrypt the upstream leg without authenticating its certificate.
    pub upstream_tls: bool,
}

impl RelayPolicy {
    pub fn speed_limit(&self) -> f64 {
        match self.speed_limit {
            Some(limit) if limit != 0f64 => limit,
            _ => f64::INFINITY,
        }
    }

    /// Policy for one routed connection on the mandatory TLS listener.
    pub fn for_tls_route(config: &DefaultListenerConfig, action: &TlsRouteAction) -> std::sync::Arc<Self> {
        let upstream_tls = match action {
            TlsRouteAction::Passthrough { .. } => true,
            TlsRouteAction::Terminate { upstream, .. } => *upstream == UpstreamTransport::Tls,
            TlsRouteAction::ReverseProxy { .. } | TlsRouteAction::Reject => false,
        };
        std::sync::Arc::new(Self {
            bind: config.bind.clone(),
            target: None,
            target_port: action.target_port().unwrap_or(443),
            speed_limit: config.ordinary_traffic.speed_limit,
            upstream_tls,
        })
    }
}
