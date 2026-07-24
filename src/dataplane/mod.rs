//! Connection data plane: the layered pipeline a connection flows through
//! after a listener accepts it.
//!
//! The shape is a chain of interception points (see [`pipeline`]): a TLS
//! connection is intercepted at its ClientHello, and if it terminates into
//! HTTP it is re-intercepted at its request head. Terminal backends
//! (passthrough, terminate, reverse proxy, static files, raw forward) consume
//! an intercepted artifact plus its continuation stream. Cross-cutting
//! lifecycle (tracking, accounting, active counts) is owned by [`lifecycle`]
//! so every path — including force-cancelled ones — is accounted uniformly.

pub mod lifecycle;

pub use lifecycle::ConnGuard;
