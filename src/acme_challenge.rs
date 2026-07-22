use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rcgen::{CertificateParams, CustomExtension, KeyPair};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub const TLS_ALPN_PROTOCOL: &[u8] = b"acme-tls/1";
pub const DEFAULT_GATE_TTL: Duration = Duration::from_secs(10 * 60);
const ACME_IDENTIFIER_OID: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 1, 31];

#[derive(Clone, Default)]
pub struct ChallengeRegistry {
    inner: Arc<Mutex<HashMap<String, ChallengeEntry>>>,
}

lazy_static::lazy_static! {
    static ref GLOBAL_REGISTRY: ChallengeRegistry = ChallengeRegistry::default();
}

pub fn global() -> &'static ChallengeRegistry {
    &GLOBAL_REGISTRY
}

#[derive(Clone)]
struct ChallengeEntry {
    certificate: Arc<CertifiedKey>,
    expires_at: Instant,
    nonce: u64,
}

pub fn server_config(certificate: Arc<CertifiedKey>) -> Arc<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certificate)));
    config.alpn_protocols = vec![TLS_ALPN_PROTOCOL.to_vec()];
    Arc::new(config)
}

/// Completes a challenge handshake after a passthrough listener has already
/// buffered the ClientHello for inspection.
pub async fn accept_buffered<S>(
    buffered: Vec<u8>,
    stream: S,
    certificate: Arc<CertifiedKey>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let replay = ReplayStream::new(buffered, stream);
    let acceptor = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), replay);
    let start = acceptor.await.context("invalid ACME TLS ClientHello")?;
    let _stream = start
        .into_stream(server_config(certificate))
        .await
        .context("ACME TLS-ALPN handshake failed")?;
    Ok(())
}

/// Replays bytes consumed during protocol inspection before reading from the
/// underlying stream. The default listener uses this for both ACME and normal
/// TLS termination so a ClientHello is inspected exactly once at its edge.
pub(crate) struct ReplayStream<S> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S> ReplayStream<S> {
    pub(crate) fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: std::io::Cursor::new(prefix),
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ReplayStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let position = self.prefix.position() as usize;
        if position < self.prefix.get_ref().len() {
            let remaining = &self.prefix.get_ref()[position..];
            let count = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..count]);
            self.prefix.set_position((position + count) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ReplayStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Removes exactly the registration it created. A later challenge for the
/// same domain is therefore not accidentally removed by an older task.
pub struct ChallengeGuard {
    registry: ChallengeRegistry,
    domain: String,
    nonce: u64,
}

impl Drop for ChallengeGuard {
    fn drop(&mut self) {
        let mut entries = self.registry.inner.lock().expect("challenge lock poisoned");
        if entries
            .get(&self.domain)
            .is_some_and(|entry| entry.nonce == self.nonce)
        {
            entries.remove(&self.domain);
        }
    }
}

impl ChallengeRegistry {
    pub fn register(
        &self,
        domain: &str,
        key_authorization: &[u8],
        ttl: Duration,
    ) -> Result<ChallengeGuard> {
        let domain = normalize_domain(domain)?;
        let certificate = Arc::new(make_challenge_certificate(&domain, key_authorization)?);
        let mut entries = self.inner.lock().expect("challenge lock poisoned");
        let nonce = entries
            .get(&domain)
            .map_or(1, |entry| entry.nonce.wrapping_add(1));
        entries.insert(
            domain.clone(),
            ChallengeEntry {
                certificate,
                expires_at: Instant::now() + ttl,
                nonce,
            },
        );
        Ok(ChallengeGuard {
            registry: self.clone(),
            domain,
            nonce,
        })
    }

    pub fn resolve(&self, domain: &str) -> Option<Arc<CertifiedKey>> {
        let domain = normalize_domain(domain).ok()?;
        let mut entries = self.inner.lock().expect("challenge lock poisoned");
        let entry = entries.get(&domain)?.clone();
        if entry.expires_at <= Instant::now() {
            entries.remove(&domain);
            return None;
        }
        Some(entry.certificate)
    }

    pub fn purge_expired(&self) {
        let now = Instant::now();
        self.inner
            .lock()
            .expect("challenge lock poisoned")
            .retain(|_, entry| entry.expires_at > now);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

fn normalize_domain(domain: &str) -> Result<String> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || domain.contains('/') || domain.contains('\0') {
        return Err(anyhow!("invalid ACME challenge domain"));
    }
    Ok(domain)
}

fn make_challenge_certificate(domain: &str, key_authorization: &[u8]) -> Result<CertifiedKey> {
    let key = KeyPair::generate().context("failed to generate ACME challenge key")?;
    let mut params = CertificateParams::new(vec![domain.to_owned()])?;
    params.not_before = OffsetDateTime::now_utc() - time::Duration::minutes(1);
    params.not_after = OffsetDateTime::now_utc() + time::Duration::minutes(15);

    // RFC 8737 requires a critical id-pe-acmeIdentifier extension containing
    // the SHA-256 digest of the key authorization as a DER OCTET STRING.
    let digest = Sha256::digest(key_authorization);
    let mut der_value = Vec::with_capacity(34);
    der_value.extend_from_slice(&[0x04, 0x20]);
    der_value.extend_from_slice(&digest);
    let mut extension = CustomExtension::from_oid_content(ACME_IDENTIFIER_OID, der_value);
    extension.set_criticality(true);
    params.custom_extensions.push(extension);

    let cert = params
        .self_signed(&key)
        .context("failed to sign ACME challenge certificate")?;
    let private_key = rustls::pki_types::PrivateKeyDer::from(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let signer = provider
        .key_provider
        .load_private_key(private_key)
        .context("failed to load ACME challenge signing key")?;
    Ok(CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(cert.der().to_vec())],
        signer,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::TlsConnector;

    #[test]
    fn guard_clears_only_its_own_registration() {
        let registry = ChallengeRegistry::default();
        let first = registry
            .register("Example.COM.", b"first", DEFAULT_GATE_TTL)
            .unwrap();
        let second = registry
            .register("example.com", b"second", DEFAULT_GATE_TTL)
            .unwrap();
        drop(first);
        assert!(registry.resolve("EXAMPLE.COM").is_some());
        drop(second);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn expired_registration_is_not_resolved() {
        let registry = ChallengeRegistry::default();
        let _guard = registry
            .register("example.com", b"token", Duration::ZERO)
            .unwrap();
        assert!(registry.resolve("example.com").is_none());
    }

    #[tokio::test]
    async fn local_tls_alpn_handshake_serves_rfc8737_certificate() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();
        let registry = ChallengeRegistry::default();
        let key_authorization = b"token.account-thumbprint";
        let _guard = registry
            .register("alpn.example", key_authorization, DEFAULT_GATE_TTL)
            .unwrap();
        let certificate = registry.resolve("alpn.example").unwrap();
        let (mut server_io, client_io) = tokio::io::duplex(32 * 1024);

        let server = tokio::spawn(async move {
            let hello = crate::tls_header::read_client_hello(
                &mut server_io,
                Duration::from_secs(2),
                crate::tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
            )
            .await
            .unwrap();
            assert_eq!(hello.sni_host, "alpn.example");
            assert!(hello
                .alpn_protocols
                .iter()
                .any(|protocol| protocol == TLS_ALPN_PROTOCOL));
            accept_buffered(hello.buffered, server_io, certificate)
                .await
                .unwrap();
        });

        let mut client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::upstream_tls::TrustAllVerifier))
            .with_no_client_auth();
        client_config.alpn_protocols = vec![TLS_ALPN_PROTOCOL.to_vec()];
        let connector = TlsConnector::from(Arc::new(client_config));
        let server_name = rustls::pki_types::ServerName::try_from("alpn.example".to_owned())
            .unwrap();
        let client = connector.connect(server_name, client_io).await.unwrap();
        assert_eq!(client.get_ref().1.alpn_protocol(), Some(TLS_ALPN_PROTOCOL));

        let peer = client.get_ref().1.peer_certificates().unwrap()[0].clone();
        let (_, parsed) = x509_parser::parse_x509_certificate(peer.as_ref()).unwrap();
        let extension = parsed
            .extensions()
            .iter()
            .find(|extension| extension.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .expect("missing id-pe-acmeIdentifier");
        assert!(extension.critical);
        let digest = Sha256::digest(key_authorization);
        assert_eq!(&extension.value[0..2], &[0x04, 0x20]);
        assert_eq!(&extension.value[2..], digest.as_slice());
        server.await.unwrap();
        drop(client);
    }
}
