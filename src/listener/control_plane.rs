//! Serves an Axum control-plane router on a TLS stream already accepted by
//! the mandatory listener, without a loopback TCP hop.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;

use crate::ca::LocalCa;
use crate::extensible::Extensible;
use crate::listener::default::{ControlFuture, ControlPlaneService};
use crate::tls_header::ClientHello;

#[derive(Clone)]
pub struct AxumControlPlaneService {
    router: Router,
    ca: LocalCa,
    certificate_cache: crate::managed_tls::ManagedCertificateCache,
    fallback: crate::runtime_config::CertificateFallbackPolicy,
}

impl AxumControlPlaneService {
    pub fn new(
        router: Router,
        ca: LocalCa,
        certificate_cache: crate::managed_tls::ManagedCertificateCache,
        fallback: crate::runtime_config::CertificateFallbackPolicy,
    ) -> Self {
        Self { router, ca, certificate_cache, fallback }
    }

    async fn serve_inner(
        &self,
        hostname: String,
        hello: ClientHello,
        client: Extensible<tokio::net::TcpStream>,
    ) -> Result<()> {
        if hello.sni_host != hostname {
            return Err(anyhow!("control-plane SNI changed after classification"));
        }
        let certified_key = self
            .certificate_cache
            .resolve_with_fallback(&hostname, &self.ca, self.fallback)
            .await?;
        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                certified_key,
            )));
        server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let replay = crate::acme_challenge::ReplayStream::new(hello.buffered, client);
        let acceptor = tokio_rustls::LazyConfigAcceptor::new(
            rustls::server::Acceptor::default(),
            replay,
        );
        let start = tokio::time::timeout(Duration::from_secs(5), acceptor)
            .await
            .map_err(|_| anyhow!("control-plane TLS ClientHello timed out"))??;
        let tls = tokio::time::timeout(
            Duration::from_secs(5),
            start.into_stream(Arc::new(server_config)),
        )
        .await
        .map_err(|_| anyhow!("control-plane TLS handshake timed out"))??;
        let io = TokioIo::new(tls);
        let service = TowerToHyperService::new(self.router.clone());
        hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
            .serve_connection(io, service)
            .await
            .map_err(|cause| anyhow!("control-plane HTTP connection failed: {cause}"))?;
        Ok(())
    }
}

impl ControlPlaneService for AxumControlPlaneService {
    fn serve<'a>(
        &'a self,
        hostname: String,
        hello: ClientHello,
        client: Extensible<tokio::net::TcpStream>,
    ) -> ControlFuture<'a> {
        Box::pin(async move { self.serve_inner(hostname, hello, client).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn serves_axum_over_an_already_inspected_control_tls_stream() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();
        let directory = tempfile::tempdir().unwrap();
        let ca = LocalCa::new(
            directory.path().to_str().unwrap(),
            &crate::config::LocalCaConfig::default(),
        )
        .unwrap();
        let app = Router::new().route("/health", get(|| async { "healthy" }));
        let service = AxumControlPlaneService::new(
            app,
            ca,
            crate::managed_tls::ManagedCertificateCache::default(),
            crate::runtime_config::CertificateFallbackPolicy::LocalCa,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut client = Extensible::of(socket);
            let hello = crate::tls_header::read_client_hello(
                &mut client,
                Duration::from_secs(2),
                crate::tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
            )
            .await
            .unwrap();
            service
                .serve("tls.example".into(), hello, client)
                .await
                .unwrap();
        });

        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::upstream_tls::TrustAllVerifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let name = rustls::pki_types::ServerName::try_from("tls.example".to_owned()).unwrap();
        let mut tls = connector.connect(name, socket).await.unwrap();
        tls.write_all(b"GET /health HTTP/1.1\r\nHost: tls.example\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("healthy"));
        server.await.unwrap();
    }
}
