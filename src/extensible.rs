//! A stream tagged with its connection's `RequestId`.
//!
//! Every proxied connection is assigned a `RequestId` at accept, used for
//! logging and as the active-connection tracker key. This wrapper carries that
//! id alongside the stream so downstream layers read it directly — a plain
//! `Arc` clone, no lock and no fallible downcast.
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

pub struct Extensible<T> {
    inner: T,
    request_id: Arc<RequestId>,
}

impl<T> AsyncRead for Extensible<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for Extensible<T>
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

impl<T> DerefMut for Extensible<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T> Deref for Extensible<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> Extensible<T> {
    /// Wraps a stream with a fresh connection id.
    pub fn of(inner: T) -> Self {
        Extensible {
            inner,
            request_id: Arc::new(RequestId::new()),
        }
    }

    /// Rewraps a transformed stream (e.g. the plaintext stream after TLS
    /// termination) while preserving the original connection id.
    pub fn with_request_id(inner: T, request_id: Arc<RequestId>) -> Self {
        Extensible { inner, request_id }
    }

    /// The connection's request id. Cheap `Arc` clone; never fails.
    pub fn request_id(&self) -> Arc<RequestId> {
        Arc::clone(&self.request_id)
    }

    pub fn unwrap(self) -> T {
        self.inner
    }
}
