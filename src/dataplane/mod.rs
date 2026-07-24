//! Connection data plane: the layered pipeline a connection flows through
//! after a listener accepts it.
//!
//! The shape is a chain of interception points (see [`pipeline`]): a TLS
//! connection is intercepted at its ClientHello ([`tls`]), and if it
//! terminates into HTTP it is re-intercepted at its request head ([`http`]).
//! Terminal backends — passthrough and terminate forwarders, the HTTP reverse
//! proxy / static server / redirect, and the raw L4 [`l4`] — consume an
//! intercepted artifact plus its continuation stream. Cross-cutting lifecycle
//! (tracking, accounting, active counts) is owned by [`lifecycle`] so every
//! path, including force-cancelled ones, is accounted uniformly.

pub mod l4;
pub mod http;
pub mod lifecycle;
pub mod pipeline;
pub mod tls;

pub use lifecycle::ConnGuard;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Connection-invariant handles every data-plane backend needs, known the
/// moment a connection is accepted. Bundled so handler signatures don't
/// balloon; each backend destructures it at entry, leaving its body written
/// against the individual fields. Route-derived values (the per-route
/// `Listener` limits, certificate keys) stay explicit parameters because they
/// are not known until the route is chosen.
pub struct ConnCtx {
    pub name: Arc<String>,
    pub remote: SocketAddr,
    pub stats: Arc<crate::listener_stats::ListenerStats>,
    pub controller: Arc<RwLock<crate::controller::Controller>>,
}

/// Certificate handles the TLS terminate / reverse-proxy dispatch needs.
pub struct TlsCtx {
    pub ca: crate::ca::LocalCa,
    pub cache: crate::managed_tls::ManagedCertificateCache,
    pub fallback: crate::runtime_config::CertificateFallbackPolicy,
}
