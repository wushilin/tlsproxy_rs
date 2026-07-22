pub mod control_plane;
pub mod default;
pub mod forward;
pub mod http_passthrough;
pub mod tls_passthrough;
pub mod tls_terminate;

/// Result of the mandatory listener's protocol- and SNI-level routing. The
/// downstream protocol handlers never make control-plane or ACME decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultRoute {
    AcmeChallenge { domain: String },
    ControlPlane { hostname: String },
    Proxy { sni: String },
    Reject(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    MissingSni,
    UnmatchedAcmeChallenge,
    PolicyDenied,
}
