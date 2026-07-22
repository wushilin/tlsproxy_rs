use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use rand::{seq::SliceRandom, RngCore};
use rcgen::generate_simple_self_signed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::auth::{hash_password, UserRecord};
use crate::runtime_config::RuntimeConfig;
use crate::store::{normalize_domain, Store};

pub const RANDOM_PORT_START: u16 = 40_000;
pub const RANDOM_PORT_END: u16 = 50_000;
pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub bind_address: IpAddr,
    pub requested_port: Option<u16>,
    pub token: Option<String>,
    pub token_lifetime: Duration,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            requested_port: None,
            token: None,
            token_lifetime: DEFAULT_TOKEN_LIFETIME,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub token: String,
    pub username: String,
    pub password: String,
    pub control_hostname: String,
    #[serde(default)]
    pub self_ips: Vec<String>,
    #[serde(default = "default_provider")]
    pub provider_id: String,
}

fn default_provider() -> String {
    "letsencrypt-production".into()
}

#[derive(Debug, Serialize)]
struct SetupResponse {
    success: bool,
    message: String,
}

struct SetupState {
    store: Store,
    token_digest: [u8; 32],
    expires_at: tokio::time::Instant,
    submission_lock: Mutex<()>,
    failed_attempts: Mutex<(u32, tokio::time::Instant)>,
    handle: axum_server::Handle<SocketAddr>,
    completed: tokio::sync::Notify,
}

pub struct SetupServer {
    pub address: SocketAddr,
    pub token: String,
    pub certificate_fingerprint_sha256: String,
    listener: TcpListener,
    tls: RustlsConfig,
    state: Arc<SetupState>,
}

impl SetupServer {
    pub fn prepare(store: Store, options: SetupOptions) -> Result<Self> {
        if store.is_initialized()? {
            bail!("initial setup is already complete");
        }
        let listener = bind_setup_listener(options.bind_address, options.requested_port)?;
        let address = listener.local_addr()?;
        let token = options.token.unwrap_or_else(generate_token);
        if token.as_bytes().len() < 32 {
            bail!("setup token must contain at least 32 bytes");
        }
        let token_digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();

        let sans = vec!["localhost".to_owned(), address.ip().to_string()];
        let identity = generate_simple_self_signed(sans)
            .context("failed to generate ephemeral setup certificate")?;
        let cert_der = identity.cert.der().to_vec();
        let fingerprint = hex::encode(Sha256::digest(&cert_der));
        let certified_key = crate::certificate::certified_key_from_parts(
            vec![rustls::pki_types::CertificateDer::from(cert_der)],
            rustls::pki_types::PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
        )?;
        let tls = RustlsConfig::from_config(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                    Arc::new(certified_key),
                ))),
        ));
        let handle = axum_server::Handle::new();
        let state = Arc::new(SetupState {
            store,
            token_digest,
            expires_at: tokio::time::Instant::now() + options.token_lifetime,
            submission_lock: Mutex::new(()),
            failed_attempts: Mutex::new((0, tokio::time::Instant::now())),
            handle,
            completed: tokio::sync::Notify::new(),
        });
        Ok(Self {
            address,
            token,
            certificate_fingerprint_sha256: fingerprint,
            listener,
            tls,
            state,
        })
    }

    pub async fn run(self) -> Result<()> {
        let app = Router::new()
            .route("/", get(setup_page))
            .route("/setup", get(setup_page).post(complete_setup))
            .route("/tlsproxy_api/setup", post(complete_setup))
            .with_state(self.state.clone());
        let server = axum_server::from_tcp_rustls(self.listener, self.tls)?
            .handle(self.state.handle.clone())
            .serve(app.into_make_service());
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => result.context("temporary setup server failed")?,
            _ = self.state.completed.notified() => {
                self.state.handle.graceful_shutdown(Some(Duration::from_secs(2)));
                server.await.context("temporary setup server failed during shutdown")?;
            }
        }
        Ok(())
    }
}

fn bind_setup_listener(bind_address: IpAddr, requested_port: Option<u16>) -> Result<TcpListener> {
    if let Some(port) = requested_port {
        let listener = TcpListener::bind(SocketAddr::new(bind_address, port)).with_context(|| {
            format!("failed to bind requested setup address {bind_address}:{port}")
        })?;
        listener.set_nonblocking(true)?;
        return Ok(listener);
    }
    let mut ports: Vec<u16> = (RANDOM_PORT_START..=RANDOM_PORT_END).collect();
    ports.shuffle(&mut rand::rng());
    for port in ports {
        match TcpListener::bind(SocketAddr::new(bind_address, port)) {
            Ok(listener) => { listener.set_nonblocking(true)?; return Ok(listener); },
            Err(cause) if cause.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(cause) => return Err(cause.into()),
        }
    }
    Err(anyhow!(
        "no free setup port in {RANDOM_PORT_START}..={RANDOM_PORT_END}"
    ))
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

async fn setup_page() -> Html<&'static str> {
    Html(include_str!("../static/setup.html"))
}

async fn complete_setup(
    State(state): State<Arc<SetupState>>,
    Json(request): Json<SetupRequest>,
) -> Response {
    let _guard = state.submission_lock.lock().await;
    {
        let mut attempts = state.failed_attempts.lock().await;
        if attempts.1.elapsed() >= Duration::from_secs(5 * 60) {
            *attempts = (0, tokio::time::Instant::now());
        }
        if attempts.0 >= 20 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(SetupResponse {
                    success: false,
                    message: "too many failed setup attempts; retry later".into(),
                }),
            )
                .into_response();
        }
    }
    let response = match complete_setup_inner(&state, request) {
        Ok(()) => {
            state.completed.notify_one();
            let handle = state.handle.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                handle.graceful_shutdown(Some(Duration::from_secs(2)));
            });
            (
                StatusCode::CREATED,
                Json(SetupResponse {
                    success: true,
                    message: "Initial setup completed; the temporary setup server is closing"
                        .into(),
                }),
            )
                .into_response()
        }
        Err(cause) => {
            state.failed_attempts.lock().await.0 += 1;
            (
                StatusCode::BAD_REQUEST,
                Json(SetupResponse {
                    success: false,
                    message: cause.to_string(),
                }),
            )
                .into_response()
        }
    };
    response
}

fn complete_setup_inner(state: &SetupState, request: SetupRequest) -> Result<()> {
    if state.store.is_initialized()? {
        bail!("initial setup is already complete");
    }
    if tokio::time::Instant::now() >= state.expires_at {
        bail!("setup token has expired; restart the service to generate another token");
    }
    let supplied: [u8; 32] = Sha256::digest(request.token.as_bytes()).into();
    if !bool::from(state.token_digest.ct_eq(&supplied)) {
        bail!("invalid setup token");
    }
    let username = request.username.trim();
    if username.is_empty() || username.len() > 128 {
        bail!("administrator username is required and must not exceed 128 characters");
    }
    let control_hostname = normalize_domain(&request.control_hostname)?;
    let self_ips = request
        .self_ips
        .iter()
        .map(|value| value.parse().map(|ip: IpAddr| ip.to_string()))
        .collect::<std::result::Result<_, _>>()?;
    let mut config = RuntimeConfig::default();
    config.control_plane.hostname = control_hostname;
    config.control_plane.self_ips = self_ips;
    config.control_plane.provider_id = request.provider_id;
    let administrator = UserRecord {
        username: username.into(),
        password_hash: hash_password(&request.password)?,
        administrator: true,
        created_at: Some(OffsetDateTime::now_utc()),
        disabled: false,
    };
    state.store.bootstrap(&config, &administrator)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn requested_port_is_used_and_random_port_stays_in_range() {
        let requested = bind_setup_listener(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), Some(0))
            .unwrap();
        assert_ne!(requested.local_addr().unwrap().port(), 0);
        let random = bind_setup_listener(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), None).unwrap();
        assert!((RANDOM_PORT_START..=RANDOM_PORT_END)
            .contains(&random.local_addr().unwrap().port()));
    }

    #[test]
    fn setup_is_one_time_and_stores_argon2_password() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let server = SetupServer::prepare(
            store.clone(),
            SetupOptions {
                requested_port: Some(0),
                token: Some("01234567890123456789012345678901".into()),
                ..Default::default()
            },
        )
        .unwrap();
        complete_setup_inner(
            &server.state,
            SetupRequest {
                token: server.token.clone(),
                username: "admin".into(),
                password: "correct horse battery staple".into(),
                control_hostname: "TLS.Example.".into(),
                self_ips: vec!["192.0.2.1".into()],
                provider_id: default_provider(),
            },
        )
        .unwrap();
        assert!(store.is_initialized().unwrap());
        let user = store.user("admin").unwrap().unwrap();
        assert!(crate::auth::verify_password(
            &user.password_hash,
            "correct horse battery staple"
        ));
        let control = store.certificate_for_domain("tls.example").unwrap().unwrap();
        assert_eq!(control.id, "control-plane");
        assert_eq!(control.provider_id, "letsencrypt-production");
        assert!(store.active_generation(&control.id).unwrap().is_none());
        assert!(SetupServer::prepare(store, SetupOptions::default()).is_err());
    }
}
