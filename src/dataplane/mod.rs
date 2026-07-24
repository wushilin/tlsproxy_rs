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
