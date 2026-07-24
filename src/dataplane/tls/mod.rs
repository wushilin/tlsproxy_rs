//! TLS interception layer.
//!
//! An inbound TLS connection is intercepted at its ClientHello. The parsed
//! hello plus the continuation stream are then handed to one of the terminal
//! backends: [`passthrough`] relays the encrypted stream verbatim after
//! replaying the hello upstream, while [`terminate`] completes the handshake
//! locally and either relays the decrypted stream to a TCP/TLS upstream or —
//! when the route is a reverse proxy — re-intercepts it as an HTTP connection.

pub mod passthrough;
pub mod terminate;

use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::dataplane::pipeline::{Intercept, Intercepted};
use crate::extensible::Extensible;
use crate::tls_header::{self, ClientHello};

/// Interception point for an inbound TLS connection: its ClientHello. The
/// continuation stream is the same connection, positioned immediately after
/// the hello (whose raw bytes are retained in [`ClientHello::buffered`] for
/// replay by a terminating or passthrough backend).
pub struct ClientHelloIntercept<S> {
    stream: Extensible<S>,
    timeout: Duration,
    max_size: usize,
}

impl<S> ClientHelloIntercept<S> {
    pub fn new(stream: Extensible<S>, timeout: Duration) -> Self {
        Self {
            stream,
            timeout,
            max_size: tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
        }
    }
}

impl<S> Intercept for ClientHelloIntercept<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    type Artifact = ClientHello;
    type Stream = Extensible<S>;

    async fn intercept(mut self) -> Result<Intercepted<ClientHello, Extensible<S>>> {
        let artifact =
            tls_header::read_client_hello(&mut self.stream, self.timeout, self.max_size).await?;
        Ok(Intercepted {
            artifact,
            stream: self.stream,
        })
    }
}
