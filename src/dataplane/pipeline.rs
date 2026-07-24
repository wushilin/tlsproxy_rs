//! Core pipeline abstraction shared by every data-plane layer.
//!
//! A connection is modeled as a sequence of *interception points*. At each
//! point a layer parses one protocol artifact (a TLS ClientHello, an HTTP
//! request head) and hands that artifact together with the continuation stream
//! to the next layer. The continuation stream keeps its concrete type so the
//! byte-copy relay and the extension map ride through without erasure; only the
//! artifact type varies per layer.

use anyhow::Result;

/// The product of an interception: the parsed protocol artifact plus the
/// stream positioned to continue the conversation.
pub struct Intercepted<Artifact, Stream> {
    pub artifact: Artifact,
    pub stream: Stream,
}

/// A connection positioned at a protocol interception point. Consuming it
/// parses up to and including that point and yields the artifact together with
/// the continuation stream — the artifact's own bytes are either consumed from,
/// or replayable at the head of, that stream as documented by each impl.
#[allow(async_fn_in_trait)]
pub trait Intercept {
    /// The parsed protocol artifact (e.g. a ClientHello or an HTTP head).
    type Artifact;
    /// The stream handed to the next layer once the artifact is parsed.
    type Stream;

    async fn intercept(self) -> Result<Intercepted<Self::Artifact, Self::Stream>>;
}
