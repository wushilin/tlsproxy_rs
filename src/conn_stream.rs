//! A stream tagged with its connection's `RequestId`, with byte pushback.
//!
//! Every proxied connection is assigned a `RequestId` at accept, used for
//! logging and as the active-connection tracker key. This wrapper carries that
//! id alongside the stream so downstream layers read it directly — a plain
//! `Arc` clone, no lock and no fallible downcast.
//!
//! The wrapper also supports `unread`: an interception layer that consumed
//! bytes while peeking at a protocol (a TLS ClientHello, an HTTP head's body
//! prefix) can push them back, and subsequent reads present the restored
//! bytes before the underlying stream. Downstream consumers therefore see the
//! full, pristine protocol stream — a TLS acceptor or a future h2 engine can
//! be handed the connection as if nothing had been peeked.
//!
//! (This replaced a generic async extension map whose only ever stored value
//! was the `RequestId`; the map's lock, `TypeId` lookup, and unwrap on every
//! read were unjustified for a single fixed field.)

use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};

use crate::request_id::RequestId;

/// Upper bound on restored bytes; interception layers are individually
/// bounded well below this, so hitting it indicates a logic error.
const MAX_PUSHBACK: usize = 1024 * 1024;

pub struct ConnStream<T> {
    inner: T,
    request_id: Arc<RequestId>,
    pushback: Vec<u8>,
    pushback_offset: usize,
}

impl<T> AsyncRead for ConnStream<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pushback_offset < self.pushback.len() {
            let this = self.get_mut();
            let available = &this.pushback[this.pushback_offset..];
            let count = available.len().min(buf.remaining());
            buf.put_slice(&available[..count]);
            this.pushback_offset += count;
            if this.pushback_offset == this.pushback.len() {
                this.pushback = Vec::new();
                this.pushback_offset = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for ConnStream<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<T> DerefMut for ConnStream<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T> Deref for ConnStream<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> ConnStream<T> {
    /// Wraps a stream with a fresh connection id.
    pub fn of(inner: T) -> Self {
        ConnStream {
            inner,
            request_id: Arc::new(RequestId::new()),
            pushback: Vec::new(),
            pushback_offset: 0,
        }
    }

    /// Rewraps a transformed stream (e.g. the plaintext stream after TLS
    /// termination) while preserving the original connection id.
    pub fn with_request_id(inner: T, request_id: Arc<RequestId>) -> Self {
        ConnStream { inner, request_id, pushback: Vec::new(), pushback_offset: 0 }
    }

    /// The connection's request id. Cheap `Arc` clone; never fails.
    pub fn request_id(&self) -> Arc<RequestId> {
        Arc::clone(&self.request_id)
    }

    /// Restores bytes consumed while peeking at the stream. Subsequent reads
    /// present the restored bytes — in the given order — before anything from
    /// the underlying stream, so a downstream protocol engine sees the
    /// connection exactly as it arrived on the wire.
    pub fn unread(&mut self, bytes: impl AsRef<[u8]>) {
        let bytes = bytes.as_ref();
        if bytes.is_empty() {
            return;
        }
        self.pushback.drain(..self.pushback_offset);
        self.pushback_offset = 0;
        assert!(
            self.pushback.len() + bytes.len() <= MAX_PUSHBACK,
            "stream pushback exceeds {MAX_PUSHBACK} bytes"
        );
        self.pushback.splice(0..0, bytes.iter().copied());
    }

    /// Unwraps the inner stream. Any unread pushback is discarded, so this is
    /// only sound before `unread` or after the pushback has been drained.
    pub fn unwrap(self) -> T {
        debug_assert!(
            self.pushback_offset == self.pushback.len(),
            "unwrapping a stream with pending pushback discards bytes"
        );
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn unread_bytes_are_presented_before_the_stream_across_partial_reads() {
        let (mut peer, stream) = tokio::io::duplex(64);
        peer.write_all(b"WIRE").await.unwrap();
        let mut stream = ConnStream::of(stream);
        stream.unread(b"PEEKED");
        let mut small = [0u8; 3];
        stream.read_exact(&mut small).await.unwrap();
        assert_eq!(&small, b"PEE");
        let mut rest = [0u8; 7];
        stream.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"KEDWIRE");
    }

    #[tokio::test]
    async fn later_unread_prepends_to_remaining_pushback() {
        let (mut peer, stream) = tokio::io::duplex(64);
        peer.write_all(b"!").await.unwrap();
        let mut stream = ConnStream::of(stream);
        stream.unread(b"cd");
        let mut first = [0u8; 1];
        stream.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"c");
        stream.unread(b"ab");
        let mut rest = [0u8; 4];
        stream.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"abd!");
    }

    #[tokio::test]
    async fn http_head_read_is_fully_undoable() {
        // The head parse must be reversible: unreading `head.buffered`
        // restores the byte-identical wire stream, so a downstream protocol
        // engine can take over as if nothing had been peeked.
        let raw = b"POST /submit HTTP/1.1\r\nHost: h.example\r\nContent-Length: 9\r\n\r\nBODY";
        let (mut peer, stream) = tokio::io::duplex(256);
        peer.write_all(raw).await.unwrap();
        let mut client = ConnStream::of(stream);
        let head = crate::http_header::read_http_head(
            &mut client,
            std::time::Duration::from_secs(1),
            65536,
        )
        .await
        .unwrap();
        assert_eq!(head.method, "POST");
        client.unread(&head.buffered);
        peer.write_all(b"-tail").await.unwrap();
        drop(peer);
        let mut restored = Vec::new();
        client.read_to_end(&mut restored).await.unwrap();
        assert_eq!(restored, [raw.as_slice(), b"-tail"].concat());
    }

    #[tokio::test]
    async fn split_read_half_still_drains_pushback() {
        let (mut peer, stream) = tokio::io::duplex(64);
        peer.write_all(b"-tail").await.unwrap();
        drop(peer);
        let mut stream = ConnStream::of(stream);
        stream.unread(b"head");
        let (mut read, _write) = tokio::io::split(stream);
        let mut output = String::new();
        read.read_to_string(&mut output).await.unwrap();
        assert_eq!(output, "head-tail");
    }
}
