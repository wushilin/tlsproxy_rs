use crate::active_tracker;
use crate::ca::LocalCa;
use crate::controller::Controller;
use crate::idle_tracker::IdleTracker;
use anyhow::anyhow;
use anyhow::Result;
use async_speed_limit::Limiter;
use log::{error, info, warn};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadBuf, Result as IoResult};
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::sleep,
};

use crate::extensible::Extensible;
use crate::hello_cache;
use crate::request_id::RequestId;
use crate::{
    config::{Listener, ListenerMode},
    listener_stats::ListenerStats,
    tls_header,
};

#[derive(Debug)]
struct TrustAllVerifier;

impl rustls::client::danger::ServerCertVerifier for TrustAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .expect("rustls crypto provider is installed")
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn connect_trust_all_tls(
    upstream: TcpStream,
    server_name: &str,
) -> Result<tokio_rustls::client::TlsStream<ClientHelloTrackingStream>> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAllVerifier))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|_| anyhow!("invalid upstream TLS server name `{server_name}`"))?;
    Ok(connector
        .connect(server_name, ClientHelloTrackingStream::new(upstream))
        .await?)
}

struct ClientHelloTrackingStream {
    inner: TcpStream,
    buffered: Vec<u8>,
    registered: bool,
}

impl ClientHelloTrackingStream {
    fn new(inner: TcpStream) -> Self {
        Self {
            inner,
            buffered: Vec::with_capacity(512),
            registered: false,
        }
    }

    fn observe_write(&mut self, bytes: &[u8]) {
        if self.registered || self.buffered.len() >= tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE {
            return;
        }
        let remaining = tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE - self.buffered.len();
        self.buffered
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if let Some(random) = tls_header::extract_client_random(&self.buffered) {
            hello_cache::insert(random);
            self.registered = true;
        }
    }
}

impl AsyncRead for ClientHelloTrackingStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ClientHelloTrackingStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.observe_write(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct Runner {
    pub name: String,
    pub listener: Listener,
    pub controller: Arc<RwLock<Controller>>,
    pub ca: LocalCa,
}

impl Runner {
    pub fn new(
        name: String,
        listener: Listener,
        root_context: Arc<RwLock<Controller>>,
        ca: LocalCa,
    ) -> Runner {
        Runner {
            name,
            listener,
            controller: root_context,
            ca,
        }
    }

    pub async fn start(self) -> Result<Arc<ListenerStats>> {
        let root_context_clone = Arc::clone(&self.controller);
        let mut root_controller = root_context_clone.write().await;
        let mut listener_controller = root_controller.child();
        drop(root_controller);
        self.start_managed(&mut listener_controller).await
    }

    pub async fn start_managed(
        self,
        listener_controller: &mut Controller,
    ) -> Result<Arc<ListenerStats>> {
        let bind = self.listener.bind.clone();
        let name = self.name.clone();
        let listener_config = self.listener;
        let listener_config = Arc::new(listener_config);
        let ca = self.ca.clone();
        let idle_timeout_ms = listener_config.max_idle_time_ms();
        let stats = ListenerStats::new(&self.name, idle_timeout_ms);
        let stats = Arc::new(stats);
        let stats_clone = Arc::clone(&stats);
        let (tx, mut rx) = mpsc::channel(1);
        let name_clone = name.clone();
        let task_controller = listener_controller.child();
        let listener_task = listener_controller.spawn(async move {
            let mut listener = TcpListener::bind(&bind).await;
            let max_retry = 3;
            for i in 1..max_retry + 1 {
                if listener.is_ok() {
                    break;
                }
                warn!(
                    "Listener: `{}` unable to bind to `{}` yet. retrying({i} of {max_retry})",
                    &name_clone, &bind
                );
                sleep(Duration::from_millis(100)).await;
                listener = TcpListener::bind(&bind).await;
            }
            match listener {
                Ok(inner_listener) => {
                    let _ = tx.send(None).await; // tell listener started successfully
                    let name_clone_result = name_clone.clone();
                    let result = Self::run_listener(
                        name_clone,
                        inner_listener,
                        listener_config,
                        stats_clone,
                        task_controller,
                        ca,
                    )
                    .await;
                    match result {
                        Err(cause) => {
                            error!("listener {name_clone_result} failed with {cause}");
                        }
                        _ => {
                            // it was ok
                        }
                    }
                }
                Err(cause) => {
                    let _ = tx.send(Some(format!("{cause}"))).await; // tell listener stopped successfully
                }
            }
        });
        drop(listener_task);
        let fail_reason = tokio::select! {
            _ = sleep(Duration::from_secs(1)) => {
                // trade off. typically if it fails we won't receive confirmation in rx
                // within 1 second (e.g. a bind typically fails within 1 sec)
                None
            },
            what = rx.recv() => {
                what
            }
        };
        let name = name.clone();
        return match fail_reason {
            None => {
                info!("listener {name} start cancelled");
                Ok(stats)
            }
            Some(fail_reason) => match fail_reason {
                None => {
                    warn!("listener {name} started without error");
                    Ok(stats)
                }
                Some(cause) => {
                    info!("listener {name} start with error {cause}");
                    Err(anyhow!("{}", cause))
                }
            },
        };
    }

    async fn handle_new_socket(
        name: Arc<String>,
        ext: Extensible<TcpStream>,
        remote_address: SocketAddr,
        listener_config: Arc<Listener>,
        stats: Arc<ListenerStats>,
        listener_controller: &mut Controller,
        connection_controller: Controller,
        ca: LocalCa,
    ) -> JoinHandle<Option<()>> {
        let name = Arc::clone(&name);
        let jh = listener_controller.spawn(async move {
            let controller = Arc::new(RwLock::new(connection_controller));
            let stats_local = Arc::clone(&stats);
            let request_id = ext.get_extension::<RequestId>().await.unwrap();
            let mut new_active = stats_local.increase_conn_count();
            let mut new_total = stats_local.total_count();
            info!("{request_id} ({name}) new connection from {remote_address} active {new_active} total {new_total}");
            let start = Instant::now();
            active_tracker::put(&request_id, &name, remote_address).await;
            let stats_local_clone = Arc::clone(&stats_local);
            let rr = Self::worker(
                name,
                ext,
                listener_config,
                stats_local_clone,
                controller,
                ca,
            )
            .await;
            if rr.is_err() {
                let err = rr.err().unwrap();
                warn!("{request_id} connection error: {err}");
            }
            active_tracker::remove(&request_id).await;
            new_active = stats_local.decrease_conn_count();
            new_total = stats_local.total_count();
            let elapsed = start.elapsed();
            info!("{request_id} closing connection: active {new_active} total {new_total} duration {elapsed:?}");
        });

        return jh;
    }

    async fn run_listener(
        name: String,
        listener: TcpListener,
        listener_config: Arc<Listener>,
        stats: Arc<ListenerStats>,
        mut listener_controller: Controller,
        ca: LocalCa,
    ) -> Result<()> {
        let name = Arc::new(name);
        if listener_config.mode == ListenerMode::Forward {
            let targets = listener_config
                .target
                .as_deref()
                .ok_or_else(|| anyhow!("forward mode requires target"))?;
            crate::forward::register_forward_listener(
                (*name).clone(),
                targets,
                listener_config.upstream_tls,
            )
            .await?;
        }
        loop {
            let accept_result = listener.accept().await;
            match accept_result {
                Ok((socket, addr)) => {
                    let conn_id = RequestId::new();
                    let ext = Extensible::of(socket);
                    ext.extend(conn_id).await;
                    let connection_controller = listener_controller.child();
                    let join_handle = Self::handle_new_socket(
                        Arc::clone(&name),
                        ext,
                        addr,
                        Arc::clone(&listener_config),
                        Arc::clone(&stats),
                        &mut listener_controller,
                        connection_controller,
                        ca.clone(),
                    )
                    .await;
                    // We don't nee to wait it to complete
                    drop(join_handle);
                }
                Err(cause) => {
                    warn!("listener {name} accept error: {cause}");
                    // continue processing
                    continue;
                }
            }
        }
    }

    async fn worker(
        name: Arc<String>,
        ext: Extensible<TcpStream>,
        listener_config: Arc<Listener>,
        context: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
        ca: LocalCa,
    ) -> Result<()> {
        match listener_config.mode {
            ListenerMode::Passthrough => {
                Self::worker_passthrough(name, ext, listener_config, context, controller).await
            }
            ListenerMode::Terminate => {
                Self::worker_terminate(name, ext, listener_config, context, controller, ca).await
            }
            ListenerMode::Forward => {
                Self::worker_forward(name, ext, listener_config, context, controller).await
            }
        }
    }

    async fn worker_passthrough(
        name: Arc<String>,
        ext: Extensible<TcpStream>,
        listener_config: Arc<Listener>,
        context: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
    ) -> Result<()> {
        let conn_id = ext.get_extension::<RequestId>().await.unwrap();
        info!("{conn_id} {name} passthrough worker started");
        let mut ext = ext;
        let client_hello = tls_header::read_client_hello(
            &mut ext,
            Duration::from_secs(3),
            tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
        )
        .await?;
        // A ClientHello random this proxy forwarded recently means the
        // connection looped back to us; close it before dialing anything.
        if hello_cache::is_looped(&client_hello.random) {
            warn!("{conn_id} inbound ClientHello was recently forwarded by this proxy; closing self-connection loop");
            return Err(anyhow!("detected self-connection loop"));
        }
        let header_len = client_hello.buffered.len();
        let sni_target = client_hello.sni_host;
        info!("{conn_id} sni target is {sni_target}");
        let selected =
            Self::resolve_target(&listener_config, &sni_target, true, &sni_target, &conn_id)
                .await?;
        // Record the random before connecting: in a direct loop the hello can
        // arrive back on our listener before the connect call returns.
        hello_cache::insert(client_hello.random);
        let upstream = tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(&selected.endpoint),
        )
        .await??;
        info!("{conn_id} connected to TLS upstream {}", selected.endpoint);
        let (client_read, client_write) = tokio::io::split(ext);
        let (upstream_read, mut upstream_write) = tokio::io::split(upstream);
        upstream_write.write_all(&client_hello.buffered).await?;
        context.increase_uploaded_bytes(header_len);
        active_tracker::add_uploaded(&conn_id, header_len as u64).await;
        Self::relay(
            conn_id,
            client_read,
            client_write,
            upstream_read,
            upstream_write,
            listener_config,
            context,
            controller,
            header_len as u64,
        )
        .await
    }

    async fn worker_terminate(
        name: Arc<String>,
        ext: Extensible<TcpStream>,
        listener_config: Arc<Listener>,
        context: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
        ca: LocalCa,
    ) -> Result<()> {
        let conn_id = ext.get_extension::<RequestId>().await.unwrap();
        info!("{conn_id} {name} terminating TLS worker started");
        let acceptor =
            tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), ext);
        let start = tokio::time::timeout(Duration::from_secs(5), acceptor)
            .await
            .map_err(|_| anyhow!("TLS ClientHello timed out"))??;
        let sni_target = start
            .client_hello()
            .server_name()
            .ok_or_else(|| anyhow!("TLS client did not provide SNI"))?
            .to_string();
        info!("{conn_id} received TLS ClientHello for SNI target {sni_target}");
        let selected = Self::resolve_target(
            &listener_config,
            &sni_target,
            listener_config.upstream_tls,
            &sni_target,
            &conn_id,
        )
        .await?;
        let certified_key = ca.resolve_or_mint(&sni_target)?;
        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                    certified_key,
                ))),
        );
        let tls_stream =
            tokio::time::timeout(Duration::from_secs(5), start.into_stream(server_config))
                .await
                .map_err(|_| anyhow!("TLS handshake timed out"))??;
        if listener_config.upstream_tls {
            Self::reject_obvious_self_connect(&listener_config, &selected.endpoint, &conn_id)
                .await?;
        }
        let upstream = tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(&selected.endpoint),
        )
        .await??;
        info!("{conn_id} connected to upstream {}", selected.endpoint);
        let (client_read, client_write) = tokio::io::split(tls_stream);
        if listener_config.upstream_tls {
            let upstream = connect_trust_all_tls(upstream, &sni_target).await?;
            info!(
                "{conn_id} wrapped upstream {} in trust-all TLS",
                selected.endpoint
            );
            let (upstream_read, upstream_write) = tokio::io::split(upstream);
            Self::relay(
                conn_id,
                client_read,
                client_write,
                upstream_read,
                upstream_write,
                listener_config,
                context,
                controller,
                0,
            )
            .await
        } else {
            let (upstream_read, upstream_write) = tokio::io::split(upstream);
            Self::relay(
                conn_id,
                client_read,
                client_write,
                upstream_read,
                upstream_write,
                listener_config,
                context,
                controller,
                0,
            )
            .await
        }
    }

    async fn worker_forward(
        name: Arc<String>,
        ext: Extensible<TcpStream>,
        listener_config: Arc<Listener>,
        context: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
    ) -> Result<()> {
        let conn_id = ext.get_extension::<RequestId>().await.unwrap();
        info!("{conn_id} {name} forward worker started");
        let resolved = crate::forward::choose_online(&name)
            .await
            .ok_or_else(|| anyhow!("no online forward backends"))?;
        let upstream = tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(&resolved.endpoint),
        )
        .await??;
        info!(
            "{conn_id} connected to forward upstream {}",
            resolved.endpoint
        );
        let (client_read, client_write) = tokio::io::split(ext);
        if listener_config.upstream_tls {
            let upstream = connect_trust_all_tls(upstream, &resolved.tls_server_name).await?;
            info!(
                "{conn_id} wrapped forward upstream {} in trust-all TLS",
                resolved.endpoint
            );
            let (upstream_read, upstream_write) = tokio::io::split(upstream);
            Self::relay(
                conn_id,
                client_read,
                client_write,
                upstream_read,
                upstream_write,
                listener_config,
                context,
                controller,
                0,
            )
            .await
        } else {
            let (upstream_read, upstream_write) = tokio::io::split(upstream);
            Self::relay(
                conn_id,
                client_read,
                client_write,
                upstream_read,
                upstream_write,
                listener_config,
                context,
                controller,
                0,
            )
            .await
        }
    }

    async fn reject_obvious_self_connect(
        listener_config: &Listener,
        resolved: &str,
        conn_id: &RequestId,
    ) -> Result<()> {
        let Ok(bind_addr) = listener_config.bind.parse::<SocketAddr>() else {
            return Ok(());
        };
        let Ok(upstream_addrs) = tokio::net::lookup_host(resolved).await else {
            return Ok(());
        };
        for upstream_addr in upstream_addrs {
            if upstream_addr.port() != bind_addr.port() {
                continue;
            }
            let same_bound_ip = upstream_addr.ip() == bind_addr.ip();
            let unspecified_to_loopback =
                bind_addr.ip().is_unspecified() && upstream_addr.ip().is_loopback();
            if same_bound_ip || unspecified_to_loopback {
                warn!("{conn_id} upstream TLS target {resolved} appears to point back to this listener; closing self-connection loop");
                return Err(anyhow!("detected self-connection loop"));
            }
        }
        Ok(())
    }

    async fn resolve_target(
        listener_config: &Listener,
        sni_target: &str,
        upstream_tls: bool,
        tls_server_name: &str,
        conn_id: &RequestId,
    ) -> Result<crate::forward::SelectedTarget> {
        let check_result = listener_config.is_allowed(sni_target);
        match check_result {
            true => {
                info!("{conn_id} {sni_target} allowed by ACL");
            }
            false => {
                info!("{conn_id} {sni_target} denied by ACL");
                return Err(anyhow!("{sni_target} denied by ACL"));
            }
        }
        let selected = crate::forward::select_runtime_target(
            sni_target,
            listener_config.target_port,
            upstream_tls,
            tls_server_name,
        )
        .await?;
        info!("{conn_id} final target: {}", selected.endpoint);
        Ok(selected)
    }

    async fn relay<CR, CW, UR, UW>(
        conn_id: Arc<RequestId>,
        client_read: CR,
        client_write: CW,
        upstream_read: UR,
        upstream_write: UW,
        listener_config: Arc<Listener>,
        context: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
        initial_uploaded: u64,
    ) -> Result<()>
    where
        CR: AsyncRead + Unpin + Send + 'static,
        CW: AsyncWrite + Unpin + Send + 'static,
        UR: AsyncRead + Unpin + Send + 'static,
        UW: AsyncWrite + Unpin + Send + 'static,
    {
        let idle_tracker = Arc::new(Mutex::new(IdleTracker::new(context.idle_timeout_ms)));
        let context_clone = Arc::clone(&context);
        let uploaded = Arc::new(AtomicU64::new(initial_uploaded));
        let downloaded = Arc::new(AtomicU64::new(0));
        let controller_clone = Arc::clone(&controller);
        let limiter = <Limiter>::new(listener_config.speed_limit());
        let limiter1 = limiter.clone();
        let limiter2 = limiter.clone();
        let conn_id1 = Arc::clone(&conn_id);
        let conn_id2 = Arc::clone(&conn_id);
        let conn_id3 = Arc::clone(&conn_id);

        let jh1 = Self::pipe(
            conn_id1,
            client_read,
            upstream_write,
            context_clone,
            Arc::clone(&idle_tracker),
            true,
            Arc::clone(&uploaded),
            controller_clone,
            limiter1,
        )
        .await;
        let context_clone = Arc::clone(&context);
        let controller_clone = Arc::clone(&controller);
        let jh2 = Self::pipe(
            conn_id2,
            upstream_read,
            client_write,
            context_clone,
            Arc::clone(&idle_tracker),
            false,
            Arc::clone(&downloaded),
            controller_clone,
            limiter2,
        )
        .await;

        let controller_clone = Arc::clone(&controller);
        let jh = Self::run_idle_tracker(
            conn_id3,
            jh1,
            jh2,
            Arc::clone(&idle_tracker),
            controller_clone,
        )
        .await;
        let _ = jh.await;
        let uploaded_total = uploaded.load(Ordering::SeqCst);
        let downloaded_total = downloaded.load(Ordering::SeqCst);
        info!("{conn_id} end uploaded {uploaded_total} downloaded {downloaded_total}");
        Ok(())
    }
    async fn run_idle_tracker(
        conn_id: Arc<RequestId>,
        jh1: JoinHandle<Option<()>>,
        jh2: JoinHandle<Option<()>>,
        idle_tracker: Arc<Mutex<IdleTracker>>,
        root_context: Arc<RwLock<Controller>>,
    ) -> JoinHandle<Option<()>> {
        root_context.write().await.spawn(async move {
            loop {
                if jh1.is_finished() || jh2.is_finished() {
                    if !jh1.is_finished() {
                        info!("{conn_id} abort upload as download stopped");
                        jh1.abort();
                    }
                    if !jh2.is_finished() {
                        info!("{conn_id} abort download as upload stopped");
                        jh2.abort();
                    }
                    break;
                }
                if idle_tracker.lock().await.is_expired() {
                    info!("{conn_id} idle time out. aborting.");
                    if !jh1.is_finished() {
                        jh1.abort();
                    }
                    if !jh2.is_finished() {
                        jh2.abort();
                    }
                    break;
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
    }
    async fn pipe<R, W>(
        request_id: Arc<RequestId>,
        reader_i: R,
        writer_i: W,
        context: Arc<ListenerStats>,
        idle_tracker: Arc<Mutex<IdleTracker>>,
        is_upload: bool,
        counter: Arc<AtomicU64>,
        controller: Arc<RwLock<Controller>>,
        limiter: Limiter,
    ) -> JoinHandle<Option<()>>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mut reader = reader_i;
        let mut writer = writer_i;
        let direction = match is_upload {
            true => "upload",
            false => "download",
        };
        controller.write().await.spawn(async move {
            let mut buf = vec![0; 4096];

            loop {
                let nr = reader.read(&mut buf).await;
                if nr.is_err() {
                    break;
                }

                let n = nr.unwrap();
                if n == 0 {
                    break;
                }

                limiter.consume(n).await;
                let write_result = writer.write_all(&buf[0..n]).await;
                match write_result {
                    Err(_) => {
                        break;
                    }
                    Ok(_) => {
                        counter.fetch_add(n as u64, Ordering::SeqCst);
                        if is_upload {
                            context.increase_uploaded_bytes(n);
                            active_tracker::add_uploaded(&request_id, n as u64).await;
                        } else {
                            context.increase_downloaded_bytes(n);
                            active_tracker::add_downloaded(&request_id, n as u64).await;
                        }
                        idle_tracker.lock().await.mark();
                    }
                }
            }
            info!("{request_id} {direction} ended");
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ca::LocalCa,
        certificate::{self, load_server_config, prepare_local_identity, LocalIdentityPaths},
        config::{LocalCaConfig, Policy, Rules},
    };
    use rustls::{
        pki_types::ServerName, ClientConfig as RustlsClientConfig, ClientConnection, RootCertStore,
    };
    use std::io::Cursor;
    use std::{fs::File, io::BufReader};
    use tempfile::{tempdir, TempDir};
    use tokio::time::timeout;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    fn test_identity() -> (TempDir, certificate::CertificatePaths, RootCertStore) {
        let directory = tempdir().unwrap();
        let identity_paths = LocalIdentityPaths {
            ca_cert: directory.path().join("CA.pem"),
            ca_key: directory.path().join("CA.key"),
            cert: directory.path().join("server.pem"),
            key: directory.path().join("server.key"),
        };
        let paths = prepare_local_identity(&identity_paths, &["localhost".to_string()]).unwrap();
        let mut roots = RootCertStore::empty();
        let mut reader = BufReader::new(File::open(directory.path().join("CA.pem")).unwrap());
        for certificate in rustls_pemfile::certs(&mut reader) {
            roots.add(certificate.unwrap()).unwrap();
        }
        (directory, paths, roots)
    }

    fn test_listener(mode: ListenerMode, port: u16) -> Arc<Listener> {
        Arc::new(Listener {
            bind: "127.0.0.1:0".into(),
            target: None,
            target_port: port,
            policy: Policy::ALLOW,
            rules: Rules {
                static_hosts: vec!["localhost".into()],
                patterns: vec![],
            },
            max_idle_time_ms: Some(5_000),
            speed_limit: Some(0.0),
            mode,
            upstream_tls: false,
        })
    }

    fn root_store_from_ca(ca: &LocalCa) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        let mut reader = BufReader::new(File::open(ca.ca_cert_path()).unwrap());
        for certificate in rustls_pemfile::certs(&mut reader) {
            roots.add(certificate.unwrap()).unwrap();
        }
        roots
    }

    fn test_client(roots: RootCertStore) -> TlsConnector {
        let config = RustlsClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    }

    #[tokio::test]
    async fn terminating_mode_relays_plaintext_and_counts_bytes() {
        let runtime = tempdir().unwrap();
        let ca =
            LocalCa::new(&runtime.path().to_string_lossy(), &LocalCaConfig::default()).unwrap();
        let roots = root_store_from_ca(&ca);
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let payload = b"termination works";

        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut received = vec![0; payload.len()];
            socket.read_exact(&mut received).await.unwrap();
            assert_eq!(received, payload);
            socket.write_all(&received).await.unwrap();
        });

        let listener = test_listener(ListenerMode::Terminate, upstream_port);
        let stats = Arc::new(ListenerStats::new("terminate-test", 5_000));
        let stats_for_worker = Arc::clone(&stats);
        let proxy_task = tokio::spawn(async move {
            let (socket, _) = proxy.accept().await.unwrap();
            let ext = Extensible::of(socket);
            ext.extend(RequestId::new()).await;
            Runner::worker_terminate(
                Arc::new("terminate-test".into()),
                ext,
                listener,
                stats_for_worker,
                Arc::new(RwLock::new(Controller::new())),
                ca,
            )
            .await
            .unwrap();
        });

        let socket = TcpStream::connect(proxy_address).await.unwrap();
        let mut client = test_client(roots)
            .connect(ServerName::try_from("localhost").unwrap(), socket)
            .await
            .unwrap();
        client.write_all(payload).await.unwrap();
        let mut echoed = vec![0; payload.len()];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, payload);
        drop(client);

        timeout(Duration::from_secs(2), upstream_task)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.uploaded_bytes_count(), payload.len());
        assert_eq!(stats.downloaded_bytes_count(), payload.len());
    }

    #[tokio::test]
    async fn passthrough_mode_relays_tls_to_tls_upstream() {
        let (_directory, identity, roots) = test_identity();
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let payload = b"passthrough works";
        let upstream_tls = TlsAcceptor::from(Arc::new(
            load_server_config(&identity.cert, &identity.key).unwrap(),
        ));

        let upstream_task = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await.unwrap();
            let mut socket = upstream_tls.accept(socket).await.unwrap();
            let mut received = vec![0; payload.len()];
            socket.read_exact(&mut received).await.unwrap();
            assert_eq!(received, payload);
            socket.write_all(&received).await.unwrap();
        });

        let listener = test_listener(ListenerMode::Passthrough, upstream_port);
        let stats = Arc::new(ListenerStats::new("passthrough-test", 5_000));
        let stats_for_worker = Arc::clone(&stats);
        let proxy_task = tokio::spawn(async move {
            let (socket, _) = proxy.accept().await.unwrap();
            let ext = Extensible::of(socket);
            ext.extend(RequestId::new()).await;
            Runner::worker_passthrough(
                Arc::new("passthrough-test".into()),
                ext,
                listener,
                stats_for_worker,
                Arc::new(RwLock::new(Controller::new())),
            )
            .await
            .unwrap();
        });

        let socket = TcpStream::connect(proxy_address).await.unwrap();
        let mut client = test_client(roots)
            .connect(ServerName::try_from("localhost").unwrap(), socket)
            .await
            .unwrap();
        client.write_all(payload).await.unwrap();
        let mut echoed = vec![0; payload.len()];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, payload);
        drop(client);

        timeout(Duration::from_secs(2), upstream_task)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .unwrap()
            .unwrap();
        assert!(stats.uploaded_bytes_count() > payload.len());
        assert!(stats.downloaded_bytes_count() > payload.len());
    }

    #[tokio::test]
    async fn forward_mode_relays_plaintext_to_online_backend() {
        crate::forward::reset().await;
        let plaintext_as_tls = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plaintext_as_tls_addr = plaintext_as_tls.local_addr().unwrap();
        let plaintext_as_tls_task = tokio::spawn(async move {
            let (mut socket, _) = plaintext_as_tls.accept().await.unwrap();
            let _ = socket.write_all(b"not tls").await;
        });
        crate::forward::register_forward_listener(
            "forward-tls-mismatch".into(),
            &plaintext_as_tls_addr.to_string(),
            true,
        )
        .await
        .unwrap();
        let mut check_controller = Controller::new();
        crate::forward::check_all_once(&mut check_controller).await;
        assert!(crate::forward::choose_online("forward-tls-mismatch")
            .await
            .is_none());
        timeout(Duration::from_secs(2), plaintext_as_tls_task)
            .await
            .unwrap()
            .unwrap();
        crate::forward::reset().await;

        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let payload = b"forward works";

        let upstream_task = tokio::spawn(async move {
            let (_health_socket, _) = upstream.accept().await.unwrap();
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut received = vec![0; payload.len()];
            socket.read_exact(&mut received).await.unwrap();
            assert_eq!(received, payload);
            socket.write_all(&received).await.unwrap();
        });

        let listener = Arc::new(Listener {
            bind: "127.0.0.1:0".into(),
            target: Some(upstream_addr.to_string()),
            target_port: 443,
            policy: Policy::DENY,
            rules: Rules {
                static_hosts: vec![],
                patterns: vec![],
            },
            max_idle_time_ms: Some(5_000),
            speed_limit: Some(0.0),
            mode: ListenerMode::Forward,
            upstream_tls: false,
        });
        crate::forward::register_forward_listener(
            "forward-test".into(),
            listener.target.as_deref().unwrap(),
            listener.upstream_tls,
        )
        .await
        .unwrap();
        let mut check_controller = Controller::new();
        crate::forward::check_all_once(&mut check_controller).await;
        let stats = Arc::new(ListenerStats::new("forward-test", 5_000));
        let stats_for_worker = Arc::clone(&stats);
        let proxy_task = tokio::spawn(async move {
            let (socket, _) = proxy.accept().await.unwrap();
            let ext = Extensible::of(socket);
            ext.extend(RequestId::new()).await;
            Runner::worker_forward(
                Arc::new("forward-test".into()),
                ext,
                listener,
                stats_for_worker,
                Arc::new(RwLock::new(Controller::new())),
            )
            .await
            .unwrap();
        });

        let mut client = TcpStream::connect(proxy_address).await.unwrap();
        client.write_all(payload).await.unwrap();
        let mut echoed = vec![0; payload.len()];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, payload);
        drop(client);

        timeout(Duration::from_secs(2), upstream_task)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.uploaded_bytes_count(), payload.len());
        assert_eq!(stats.downloaded_bytes_count(), payload.len());
        crate::forward::reset().await;
    }

    #[tokio::test]
    async fn passthrough_rejects_looped_client_hello() {
        // craft a raw ClientHello for "localhost" the same way a real client would
        let config = RustlsClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut connection = ClientConnection::new(Arc::new(config), server_name).unwrap();
        let mut hello_bytes = Vec::new();
        connection
            .write_tls(&mut Cursor::new(&mut hello_bytes))
            .unwrap();
        let parsed = tls_header::read_client_hello(
            &mut Cursor::new(hello_bytes.clone()),
            Duration::from_secs(1),
            tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
        )
        .await
        .unwrap();

        // pretend this proxy forwarded that hello upstream moments ago
        hello_cache::insert(parsed.random);

        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let listener = test_listener(ListenerMode::Passthrough, 1);
        let stats = Arc::new(ListenerStats::new("loop-test", 5_000));
        let proxy_task = tokio::spawn(async move {
            let (socket, _) = proxy.accept().await.unwrap();
            let ext = Extensible::of(socket);
            ext.extend(RequestId::new()).await;
            Runner::worker_passthrough(
                Arc::new("loop-test".into()),
                ext,
                listener,
                stats,
                Arc::new(RwLock::new(Controller::new())),
            )
            .await
        });

        let mut socket = TcpStream::connect(proxy_address).await.unwrap();
        socket.write_all(&hello_bytes).await.unwrap();

        let error = timeout(Duration::from_secs(2), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("self-connection loop"));
    }
}
