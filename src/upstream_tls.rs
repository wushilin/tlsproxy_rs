//! Outbound TLS connector used by routed listeners. Upstream certificate
//! verification is intentionally disabled because these routes historically
//! supported private/self-signed backends; routing still supplies SNI and
//! loop prevention is handled before connecting.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, Result as IoResult};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::{hello_cache, tls_header};

#[derive(Debug)]
pub(crate) struct TrustAllVerifier;

impl rustls::client::danger::ServerCertVerifier for TrustAllVerifier {
    fn verify_server_cert(&self, _: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>], _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> { Ok(rustls::client::danger::ServerCertVerified::assertion()) }
    fn verify_tls12_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> { Ok(rustls::client::danger::HandshakeSignatureValid::assertion()) }
    fn verify_tls13_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> { Ok(rustls::client::danger::HandshakeSignatureValid::assertion()) }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> { rustls::crypto::CryptoProvider::get_default().expect("rustls provider installed").signature_verification_algorithms.supported_schemes() }
}

pub(crate) async fn connect_trust_all_tls(upstream: TcpStream, server_name: &str) -> Result<tokio_rustls::client::TlsStream<ClientHelloTrackingStream>> {
    let config = rustls::ClientConfig::builder().dangerous().with_custom_certificate_verifier(Arc::new(TrustAllVerifier)).with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(server_name.to_owned()).map_err(|_| anyhow!("invalid upstream TLS server name `{server_name}`"))?;
    Ok(TlsConnector::from(Arc::new(config)).connect(name, ClientHelloTrackingStream::new(upstream)).await?)
}

pub(crate) struct ClientHelloTrackingStream { inner: TcpStream, buffered: Vec<u8>, registered: bool }
impl ClientHelloTrackingStream {
    fn new(inner: TcpStream) -> Self { Self { inner, buffered: Vec::with_capacity(512), registered: false } }
    fn observe(&mut self, bytes: &[u8]) { if self.registered || self.buffered.len() >= tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE { return; } let remaining = tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE - self.buffered.len(); self.buffered.extend_from_slice(&bytes[..bytes.len().min(remaining)]); if let Some(random) = tls_header::extract_client_random(&self.buffered) { hello_cache::insert(random); self.registered = true; } }
}
impl AsyncRead for ClientHelloTrackingStream { fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<IoResult<()>> { Pin::new(&mut self.inner).poll_read(cx, buffer) } }
impl AsyncWrite for ClientHelloTrackingStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<IoResult<usize>> { match Pin::new(&mut self.inner).poll_write(cx, bytes) { Poll::Ready(Ok(count)) => { self.observe(&bytes[..count]); Poll::Ready(Ok(count)) }, other => other } }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> { Pin::new(&mut self.inner).poll_flush(cx) }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> { Pin::new(&mut self.inner).poll_shutdown(cx) }
}
