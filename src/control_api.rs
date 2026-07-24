use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Form, Path, Query, Request, State};
use futures_util::{SinkExt, StreamExt};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use time::OffsetDateTime;

use crate::acme_types::{AcmeProvider, ManagedCertificate, RetrievalToken};
use crate::auth::{random_token, token_hash, verify_password, SessionRecord};
use crate::runtime_config::RuntimeConfig;
use crate::store::Store;

const SESSION_COOKIE: &str = "tlsproxy_session";
const SESSION_HOURS: i64 = 12;

#[derive(Clone)]
pub struct ControlState {
    store: Store,
    request_renewal_scan: Arc<dyn Fn() + Send + Sync>,
    login_attempts: Arc<tokio::sync::Mutex<HashMap<String, (u32, Instant)>>>,
    config_write: Arc<tokio::sync::Mutex<()>>,
    configuration_changed: Arc<dyn Fn() + Send + Sync>,
    retrieval_attempts: Arc<tokio::sync::Mutex<HashMap<String, (u32, Instant)>>>,
}

impl ControlState {
    pub fn new(store: Store, request_renewal_scan: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            store,
            request_renewal_scan: Arc::new(request_renewal_scan),
            login_attempts: Default::default(),
            config_write: Default::default(),
            configuration_changed: Arc::new(|| {}),
            retrieval_attempts: Default::default(),
        }
    }

    pub fn with_configuration_changed(
        mut self,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        self.configuration_changed = Arc::new(callback);
        self
    }
}

pub fn router(state: ControlState) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/logout", post(logout))
        .route("/tlsproxy_api/password", put(change_password))
        .route("/tlsproxy_api/session", get(session_info))
        .route("/tlsproxy_api/config", get(get_config).put(put_config))
        .route("/tlsproxy_api/config/history", get(config_history))
        .route("/tlsproxy_api/config/rollback/{revision}", post(rollback_config))
        .route("/tlsproxy_api/providers", get(list_providers).put(save_provider))
        .route("/tlsproxy_api/providers/{id}", delete(delete_provider))
        .route("/tlsproxy_api/managed_certs", get(list_certificates).post(save_certificate))
        .route("/tlsproxy_api/managed_certs/{id}", delete(delete_certificate))
        .route("/tlsproxy_api/managed_certs/{id}/publish", put(update_certificate_publish))
        .route("/tlsproxy_api/renew", post(request_renewal))
        .route("/tlsproxy_api/retrieval_tokens", get(list_retrieval_tokens).post(create_retrieval_token))
        .route("/tlsproxy_api/retrieval_tokens/{id}", delete(delete_retrieval_token))
        .route("/tlsproxy_api/audit", get(list_audit))
        .route("/tlsproxy_api/dns_diagnostics", get(list_dns_diagnostics))
        .route("/tlsproxy_api/status", get(runtime_status))
        .route("/tlsproxy_api/active_connections", get(active_connections))
        .route("/tlsproxy_api/events/ws", get(events_websocket))
        .route("/tlsproxy_api/events/snapshot", get(events_snapshot))
        .route("/tlsproxy_api/listeners/{listener}/events/ws", get(listener_events_websocket))
        .route("/tlsproxy_api/listeners/{listener}/active_connections", get(listener_active_connections))
        .route("/tlsproxy_api/listeners/{listener}/{operation}", post(listener_operation))
        .route("/tlsproxy_api/health_checks", get(health_checks))
        .route("/tlsproxy_api/health_checks/history", get(health_check_history))
        .route("/tlsproxy_api/database/export", get(export_database))
        // Database exports carry full certificate/audit history, so the
        // default 2 MB body limit would reject realistic restores.
        .route("/tlsproxy_api/database/import", post(import_database).layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024)))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_session));
    Router::new()
        .route("/login", get(login_page).post(login))
        .route("/styles.css", get(stylesheet))
        .route("/tlsproxy_api/certs/{domain}", get(publish_certificate))
        .merge(protected)
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct PublishQuery {
    #[serde(default)]
    private_key: bool,
}

#[derive(Debug, Serialize)]
struct PublishedCertificate {
    certificate_id: String,
    generation_id: String,
    domains: Vec<String>,
    certificate_pem: String,
    chain_pem: String,
    private_key_pem: Option<String>,
    fingerprint_sha256: String,
    not_before: Option<OffsetDateTime>,
    not_after: Option<OffsetDateTime>,
}

async fn publish_certificate(
    State(state): State<ControlState>,
    Path(domain): Path<String>,
    Query(query): Query<PublishQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let bearer = authorization.and_then(|v| v.strip_prefix("Bearer "));
    let Some(bearer) = bearer else {
        if retrieval_rate_limited(&state, "unauthorized").await { audit_retrieval(&state.store, &domain, None, false, "rate_limited"); return (StatusCode::TOO_MANY_REQUESTS, "retrieval rate limit exceeded").into_response(); }
        audit_retrieval(&state.store, &domain, None, false, "missing_token");
        return (StatusCode::UNAUTHORIZED, "Bearer token required").into_response();
    };
    let token = match state.store.retrieval_token_by_hash(&token_hash(bearer)) {
        Ok(Some(token)) if !token.disabled && token.expires_at.is_none_or(|at| at > OffsetDateTime::now_utc()) => token,
        _ => {
            if retrieval_rate_limited(&state, "unauthorized").await { audit_retrieval(&state.store, &domain, None, false, "rate_limited"); return (StatusCode::TOO_MANY_REQUESTS, "retrieval rate limit exceeded").into_response(); }
            audit_retrieval(&state.store, &domain, None, false, "invalid_token");
            return (StatusCode::UNAUTHORIZED, "Invalid or expired bearer token").into_response();
        }
    };
    if retrieval_rate_limited(&state, &format!("token:{}", token.id)).await {
        audit_retrieval(&state.store, &domain, Some(&token.id), false, "rate_limited");
        return (StatusCode::TOO_MANY_REQUESTS, "retrieval rate limit exceeded").into_response();
    }
    let managed = match state.store.certificate_for_domain(&domain) {
        Ok(Some(value)) if value.enabled && value.publish.enabled => value,
        Ok(_) => {
            audit_retrieval(&state.store, &domain, Some(&token.id), false, "not_published");
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(cause) => return internal(cause),
    };
    if !token.certificate_ids.is_empty() && !token.certificate_ids.contains(&managed.id) {
        audit_retrieval(&state.store, &domain, Some(&token.id), false, "scope_denied");
        return StatusCode::FORBIDDEN.into_response();
    }
    if query.private_key && (!token.allow_private_key || !managed.publish.allow_private_key) {
        audit_retrieval(&state.store, &domain, Some(&token.id), false, "key_denied");
        return (StatusCode::FORBIDDEN, "private-key export is not permitted").into_response();
    }
    let generation = match state.store.active_generation(&managed.id) {
        Ok(Some(value)) if crate::managed_tls::validate_generation(&managed, &value).is_ok() => value,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(cause) => return internal(cause),
    };
    let etag = format!("\"{}:{}:{}\"", managed.id, generation.id, query.private_key);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        audit_retrieval(&state.store, &domain, Some(&token.id), true, "not_modified");
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    audit_retrieval(&state.store, &domain, Some(&token.id), true, "published");
    let body = PublishedCertificate {
        certificate_id: managed.id,
        generation_id: generation.id,
        domains: managed.domains,
        certificate_pem: generation.certificate_pem,
        chain_pem: generation.chain_pem,
        private_key_pem: query.private_key.then_some(generation.private_key_pem),
        fingerprint_sha256: generation.fingerprint_sha256,
        not_before: generation.not_before,
        not_after: generation.not_after,
    };
    ([(header::ETAG, etag), (header::CACHE_CONTROL, "private, no-cache".into())], Json(body)).into_response()
}

async fn retrieval_rate_limited(state: &ControlState, key: &str) -> bool {
    let mut attempts = state.retrieval_attempts.lock().await;
    attempts.retain(|_, (_, start)| start.elapsed() < Duration::from_secs(60));
    let value = attempts.entry(key.to_owned()).or_insert((0, Instant::now()));
    value.0 += 1;
    value.0 > 60
}

fn audit_retrieval(store: &Store, domain: &str, token_id: Option<&str>, accepted: bool, outcome: &str) {
    if let Err(cause) = store.append_audit("certificate_retrieval", serde_json::json!({
        "domain": domain, "token_id": token_id, "accepted": accepted, "outcome": outcome
    })) {
        log::warn!("failed to audit certificate retrieval: {cause:#}");
    }
}

#[derive(Deserialize)]
struct CreateRetrievalTokenRequest {
    label: String,
    #[serde(default)] certificate_ids: std::collections::BTreeSet<String>,
    #[serde(default)] allow_private_key: bool,
    expires_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
struct CreatedRetrievalToken { id: String, bearer_token: String }

async fn create_retrieval_token(State(state): State<ControlState>, Json(request): Json<CreateRetrievalTokenRequest>) -> Response {
    if request.label.trim().is_empty() || request.expires_at.is_some_and(|at| at <= OffsetDateTime::now_utc()) {
        return (StatusCode::BAD_REQUEST, "label and a future expiry are required").into_response();
    }
    for id in &request.certificate_ids {
        match state.store.managed_certificate(id) {
            Ok(Some(_)) => {}, Ok(None) => return (StatusCode::BAD_REQUEST, format!("unknown certificate `{id}`")).into_response(),
            Err(cause) => return internal(cause),
        }
    }
    let bearer_token = random_token();
    let id = hex::encode(&sha2::Sha256::digest(bearer_token.as_bytes())[..12]);
    let token = RetrievalToken { id: id.clone(), label: request.label, token_hash: token_hash(&bearer_token), bearer_token: bearer_token.clone(), certificate_ids: request.certificate_ids, allow_private_key: request.allow_private_key, created_at: Some(OffsetDateTime::now_utc()), expires_at: request.expires_at, disabled: false };
    match state.store.save_retrieval_token(&token) {
        Ok(()) => (StatusCode::CREATED, Json(CreatedRetrievalToken { id, bearer_token })).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn list_retrieval_tokens(State(state): State<ControlState>) -> Response {
    match state.store.retrieval_tokens() {
        Ok(mut values) => { for token in &mut values { token.token_hash.clear(); } Json(values).into_response() },
        Err(cause) => internal(cause),
    }
}

async fn delete_retrieval_token(State(state): State<ControlState>, Path(id): Path<String>) -> Response {
    match state.store.delete_retrieval_token(&id) { Ok(()) => StatusCode::NO_CONTENT.into_response(), Err(cause) => internal(cause) }
}

async fn list_audit(State(state): State<ControlState>) -> Response {
    match state.store.audits(200) { Ok(values) => Json(values).into_response(), Err(cause) => internal(cause) }
}

async fn list_dns_diagnostics(State(state): State<ControlState>) -> Response {
    match state.store.dns_diagnostics() { Ok(values) => Json(values).into_response(), Err(cause) => internal(cause) }
}

async fn runtime_status(State(state): State<ControlState>) -> Response {
    let stored = match state.store.load_config() { Ok(Some(v)) => v, Ok(None) => return StatusCode::SERVICE_UNAVAILABLE.into_response(), Err(cause) => return internal(cause) };
    let certificates = match state.store.managed_certificates() { Ok(v) => v, Err(cause) => return internal(cause) };
    let mut managed = Vec::new();
    for certificate in certificates {
        let active = state.store.active_generation(&certificate.id).ok().flatten();
        let renewal = state.store.renewal_state(&certificate.id).ok().flatten();
        managed.push(serde_json::json!({
            "id": certificate.id,
            "domains": certificate.domains,
            "enabled": certificate.enabled,
            "generation_id": active.as_ref().map(|value| &value.id),
            "fingerprint_sha256": active.as_ref().map(|value| &value.fingerprint_sha256),
            "not_before": active.as_ref().and_then(|value| value.not_before),
            "not_after": active.and_then(|value| value.not_after),
            "renewal": renewal
        }));
    }
    Json(serde_json::json!({
        "revision": stored.revision,
        "control_hostname": stored.config.control_plane.hostname,
        "mandatory_listener": stored.config.default_listener.bind,
        "additional_listeners": stored.config.additional_listeners,
        "managed_certificates": managed,
    })).into_response()
}

async fn active_connections() -> Json<Vec<crate::active_tracker::ActiveConnectionSerde>> {
    Json(crate::active_tracker::get_active_list())
}

async fn listener_active_connections(Path(listener): Path<String>) -> Json<Vec<crate::active_tracker::ActiveConnectionSerde>> {
    Json(crate::active_tracker::get_active_list().into_iter().filter(|connection| connection.listener == listener).collect())
}

async fn listener_operation(
    State(state): State<ControlState>,
    Extension(session): Extension<SessionRecord>,
    Path((listener, operation)): Path<(String, String)>,
) -> Response {
    let _write_guard = state.config_write.lock().await;
    let mut stored = match state.store.load_config_async().await {
        Ok(Some(value)) => value,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(cause) => return internal(cause),
    };
    let mandatory = listener == "__default__" || listener == crate::runtime_config::DEFAULT_LISTENER_NAME;
    if !mandatory && !stored.config.additional_listeners.contains_key(&listener) {
        return (StatusCode::NOT_FOUND, "listener does not exist").into_response();
    }
    match operation.as_str() {
        "start" if mandatory => return (StatusCode::BAD_REQUEST, "Public HTTPS is always running").into_response(),
        "stop" if mandatory => return (StatusCode::BAD_REQUEST, "Public HTTPS cannot be stopped").into_response(),
        "start" => { stored.config.disabled_listeners.remove(&listener); }
        "stop" => { stored.config.disabled_listeners.insert(listener.clone()); }
        // Restart cycles just the one listener in place: no configuration
        // change is saved (a no-op save would pollute revision history and
        // reload every listener) and other listeners keep their connections.
        "restart" => {
            if stored.config.disabled_listeners.contains(&listener) {
                return (StatusCode::CONFLICT, "listener is stopped; start it instead").into_response();
            }
            let name = listener.clone();
            tokio::spawn(async move {
                // Let the HTTPS response flush before its listener may be cycled.
                tokio::time::sleep(Duration::from_millis(250)).await;
                if !crate::runtime::request_listener_restart(&name).await {
                    log::warn!("restart requested for `{name}` but the listener is not running");
                }
            });
            return Json(serde_json::json!({"listener": listener, "operation": operation})).into_response();
        }
        _ => return (StatusCode::BAD_REQUEST, "operation must be start, stop, or restart").into_response(),
    }
    match state.store.save_config_async(stored.config, session.username).await {
        Ok(saved) => {
            (state.configuration_changed)();
            Json(serde_json::json!({"revision": saved.revision, "listener": listener, "operation": operation})).into_response()
        }
        Err(cause) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
    }
}

async fn events_websocket(State(state): State<ControlState>, Extension(session_hash): Extension<String>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| stream_events(socket, None, state, session_hash)).into_response()
}

async fn events_snapshot() -> Json<Vec<crate::events_hub::Event>> {
    Json(crate::events_hub::snapshot_events().await)
}

async fn listener_events_websocket(State(state): State<ControlState>, Extension(session_hash): Extension<String>, Path(listener): Path<String>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| stream_events(socket, Some(listener), state, session_hash)).into_response()
}

async fn stream_events(socket: WebSocket, listener: Option<String>, state: ControlState, session_hash: String) {
    static NEXT_STREAM_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let stream_id = NEXT_STREAM_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log::info!("event websocket {stream_id} opened; listener={listener:?}");
    let (mut outgoing, mut incoming) = socket.split();
    let receiver = crate::events_hub::subscribe();
    for event in crate::events_hub::snapshot_events().await {
        let key = event.event_payload.get("key").and_then(|value| value.as_str()).unwrap_or_default();
        let accepted = listener.as_deref().is_none_or(|listener| key == listener);
        if !accepted { continue; }
        let Ok(encoded) = serde_json::to_string(&event) else { continue };
        if let Err(cause) = outgoing.send(Message::Text(encoded.into())).await {
            log::warn!("event websocket {stream_id} ended while sending initial snapshot: {cause}");
            return;
        }
    }
    let mut session_check = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    session_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = session_check.tick() => {
                let valid_session = state.store.session_async(session_hash.clone()).await.ok().flatten()
                    .filter(|session| session.expires_at.is_some_and(|expiry| expiry > OffsetDateTime::now_utc()));
                let valid = if let Some(session) = valid_session {
                    state.store.user_async(session.username).await.ok().flatten()
                        .is_some_and(|user| user.administrator && !user.disabled)
                } else { false };
                if !valid {
                    log::info!("event websocket {stream_id} closing because its session is no longer valid");
                    let _ = outgoing.send(Message::Close(None)).await;
                    break;
                }
            }
            frame = incoming.next() => match frame {
                Some(Ok(Message::Ping(payload))) => {
                    if let Err(cause) = outgoing.send(Message::Pong(payload)).await {
                        log::warn!("event websocket {stream_id} ended while sending pong: {cause}");
                        break;
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    log::info!("event websocket {stream_id} received client close: {frame:?}");
                    break;
                }
                Some(Err(cause)) => {
                    log::warn!("event websocket {stream_id} receive failed: {cause}");
                    break;
                }
                None => {
                    log::info!("event websocket {stream_id} ended because the client stream reached EOF");
                    break;
                }
                _ => {}
            },
            received = receiver.recv_async() => {
                // named_queue drops oldest per-subscriber on lag rather than
                // signalling it; the per-second snapshot events reconcile any
                // gap. Any error means the topic closed.
                let Ok(event) = received else {
                    log::warn!("event websocket {stream_id} ended because the event hub closed");
                    break;
                };
                let key = event.event_payload.get("key").and_then(|value| value.as_str()).unwrap_or_default();
                let event_listener = event.event_payload.get("listener_name").and_then(|value| value.as_str());
                let accepted = match listener.as_deref() {
                    Some(listener) => key == listener || event_listener == Some(listener),
                    None => event.event_type != crate::events_hub::CONNECTION_BYTES_TRANSFERRED_CHANGED,
                };
                if !accepted { continue; }
                let Ok(encoded) = serde_json::to_string(&event) else { continue };
                if let Err(cause) = outgoing.send(Message::Text(encoded.into())).await {
                    log::warn!("event websocket {stream_id} ended while publishing an event: {cause}");
                    break;
                }
            }
        }
    }
    log::info!("event websocket {stream_id} closed; listener={listener:?}");
}

async fn health_checks() -> Json<Vec<crate::forward::HealthCheckTarget>> {
    Json(crate::forward::health_check_targets().await)
}

#[derive(Deserialize)]
struct HealthHistoryQuery { listener: String, host: String, backend: String, endpoint: String }

async fn health_check_history(State(state): State<ControlState>, Query(query): Query<HealthHistoryQuery>) -> Response {
    match state.store.health_check_history_filtered_async(query.listener, query.host, query.backend, query.endpoint).await {
        Ok(values) => Json(values).into_response(),
        Err(cause) => internal(cause),
    }
}

fn is_administrator(state: &ControlState, session: &SessionRecord) -> bool {
    state.store.user(&session.username).ok().flatten().is_some_and(|user| user.administrator && !user.disabled)
}

async fn export_database(State(state): State<ControlState>, Extension(session): Extension<SessionRecord>) -> Response {
    if !is_administrator(&state, &session) { return StatusCode::FORBIDDEN.into_response(); }
    match state.store.export_json() {
        Ok(export) => match serde_json::to_string_pretty(&export) {
            Ok(body) => ([(header::CONTENT_TYPE, "application/json"), (header::CONTENT_DISPOSITION, "attachment; filename=\"tlsproxy-rocksdb-export.json\"")], body).into_response(),
            Err(cause) => internal(cause),
        },
        Err(cause) => internal(cause),
    }
}

#[derive(Deserialize)]
struct ImportDatabaseRequest {
    mode: String,
    export: crate::store::RocksDbJsonExport,
}

async fn import_database(State(state): State<ControlState>, Extension(session): Extension<SessionRecord>, Json(request): Json<ImportDatabaseRequest>) -> Response {
    if !is_administrator(&state, &session) { return StatusCode::FORBIDDEN.into_response(); }
    let clear_existing = match request.mode.as_str() {
        "merge" => false,
        "clear_and_import" => true,
        _ => return (StatusCode::BAD_REQUEST, "mode must be `merge` or `clear_and_import`").into_response(),
    };
    match state.store.import_json(&request.export, clear_existing) {
        Ok(imported) => {
            let _ = state.store.append_audit("database_json_import", serde_json::json!({"mode": request.mode, "entries": imported, "actor": session.username}));
            (state.configuration_changed)();
            Json(serde_json::json!({"imported_entries": imported, "mode": request.mode})).into_response()
        }
        Err(cause) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
    }
}

async fn login_page() -> Html<&'static str> {
    Html(include_str!("../static/login.html"))
}

async fn stylesheet() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/styles.css"))
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/control.html"))
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login(State(state): State<ControlState>, Form(form): Form<LoginForm>) -> Response {
    let throttle_key = form.username.trim().to_ascii_lowercase();
    {
        let mut attempts = state.login_attempts.lock().await;
        attempts.retain(|_, (_, started)| started.elapsed() < Duration::from_secs(5 * 60));
        if attempts
            .get(&throttle_key)
            .is_some_and(|(count, _)| *count >= 5)
        {
            return (StatusCode::TOO_MANY_REQUESTS, "Too many login attempts").into_response();
        }
    }
    let user = match state.store.user_async(form.username.clone()).await {
        Ok(Some(user)) if !user.disabled && verify_password(&user.password_hash, &form.password) => user,
        _ => {
            let mut attempts = state.login_attempts.lock().await;
            let entry = attempts.entry(throttle_key).or_insert((0, Instant::now()));
            entry.0 += 1;
            return (StatusCode::UNAUTHORIZED, "Invalid username or password").into_response();
        }
    };
    state.login_attempts.lock().await.remove(&throttle_key);
    let token = random_token();
    let csrf_token = random_token();
    let now = OffsetDateTime::now_utc();
    let session = SessionRecord {
        token_hash: token_hash(&token),
        username: user.username,
        csrf_token,
        created_at: Some(now),
        expires_at: Some(now + time::Duration::hours(SESSION_HOURS)),
    };
    if let Err(cause) = state.store.save_session_async(session).await {
        return internal(cause);
    }
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
        SESSION_HOURS * 3600
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

async fn require_session(
    State(state): State<ControlState>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = cookie_value(request.headers().get(header::COOKIE), SESSION_COOKIE);
    let Some(token) = token else {
        return Redirect::to("/login").into_response();
    };
    let hash = token_hash(&token);
    let session = match state.store.session_async(hash.clone()).await {
        Ok(Some(session)) if session.expires_at.is_some_and(|expiry| expiry > OffsetDateTime::now_utc()) => session,
        _ => return Redirect::to("/login").into_response(),
    };
    match state.store.user_async(session.username.clone()).await {
        Ok(Some(user)) if user.administrator && !user.disabled => {}
        _ => return (StatusCode::FORBIDDEN, "Administrator access required").into_response(),
    }
    if matches!(*request.method(), Method::POST | Method::PUT | Method::PATCH | Method::DELETE) {
        let supplied = request
            .headers()
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok());
        if supplied != Some(session.csrf_token.as_str()) {
            return (StatusCode::FORBIDDEN, "Missing or invalid CSRF token").into_response();
        }
    }
    request.extensions_mut().insert(session);
    request.extensions_mut().insert(hash);
    next.run(request).await
}

#[derive(Serialize)]
struct SessionInfo {
    username: String,
    csrf_token: String,
    expires_at: Option<OffsetDateTime>,
}

async fn session_info(Extension(session): Extension<SessionRecord>) -> Json<SessionInfo> {
    Json(SessionInfo {
        username: session.username,
        csrf_token: session.csrf_token,
        expires_at: session.expires_at,
    })
}

async fn logout(State(state): State<ControlState>, Extension(hash): Extension<String>) -> Response {
    if let Err(cause) = state.store.delete_session_async(hash).await {
        return internal(cause);
    }
    let cookie = format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict"
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response()
}

#[derive(Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
    #[serde(default)]
    logout_all: bool,
}

async fn change_password(
    State(state): State<ControlState>,
    Extension(session): Extension<SessionRecord>,
    Json(request): Json<ChangePasswordRequest>,
) -> Response {
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut user = store
            .user(&session.username)?
            .context("administrator no longer exists")?;
        if !verify_password(&user.password_hash, &request.current_password) {
            anyhow::bail!("current password is incorrect");
        }
        user.password_hash = crate::auth::hash_password(&request.new_password)?;
        store.save_user(&user)?;
        if request.logout_all {
            store.delete_user_sessions(&user.username)?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(cause)) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn get_config(State(state): State<ControlState>) -> Response {
    match state.store.load_config_async().await {
        Ok(Some(config)) => Json(config).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(cause) => internal(cause),
    }
}

async fn config_history(State(state): State<ControlState>) -> Response {
    match state.store.config_history(50) { Ok(values) => Json(values).into_response(), Err(cause) => internal(cause) }
}

async fn rollback_config(State(state): State<ControlState>, Extension(session): Extension<SessionRecord>, Path(revision): Path<u64>) -> Response {
    let _write_guard = state.config_write.lock().await;
    let previous = match state.store.config_revision(revision) { Ok(Some(value)) => value, Ok(None) => return StatusCode::NOT_FOUND.into_response(), Err(cause) => return internal(cause) };
    let current = match state.store.load_config() { Ok(Some(value)) => value, Ok(None) => return StatusCode::NOT_FOUND.into_response(), Err(cause) => return internal(cause) };
    if previous.config.default_listener.bind != current.config.default_listener.bind {
        return (StatusCode::BAD_REQUEST, "the mandatory Public HTTPS bind address cannot be changed by rollback").into_response();
    }
    if current.config.control_plane.hostname != previous.config.control_plane.hostname {
        let hostname = &previous.config.control_plane.hostname;
        let usable = state.store.certificate_for_domain(hostname).ok().flatten()
            .and_then(|managed| state.store.active_generation(&managed.id).ok().flatten().map(|generation| (managed, generation)))
            .is_some_and(|(managed, generation)| crate::managed_tls::validate_generation(&managed, &generation).is_ok());
        if !usable { return (StatusCode::BAD_REQUEST, "historical control hostname has no active valid managed certificate").into_response(); }
    }
    if let Err(cause) = state.store.ensure_automatic_certificates(&previous.config) {
        return (StatusCode::BAD_REQUEST, cause.to_string()).into_response();
    }
    match state.store.save_config(&previous.config, &session.username) {
        Ok(stored) => { (state.configuration_changed)(); Json(stored).into_response() },
        Err(cause) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct SaveConfigRequest {
    revision: u64,
    config: RuntimeConfig,
}

async fn put_config(
    State(state): State<ControlState>,
    Extension(session): Extension<SessionRecord>,
    Json(request): Json<SaveConfigRequest>,
) -> Response {
    let _write_guard = state.config_write.lock().await;
    let current = match state.store.load_config_async().await {
        Ok(Some(value)) => value,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(cause) => return internal(cause),
    };
    if current.revision != request.revision {
        return (StatusCode::CONFLICT, "Configuration revision changed").into_response();
    }
    if request.config.default_listener.bind != current.config.default_listener.bind {
        return (StatusCode::BAD_REQUEST, "the mandatory Public HTTPS bind address cannot be changed").into_response();
    }
    if let Err(cause) = request.config.validate() {
        return (StatusCode::BAD_REQUEST, cause.to_string()).into_response();
    }
    if current.config.control_plane.hostname != request.config.control_plane.hostname {
        let new_hostname = request.config.control_plane.hostname.clone();
        let managed = match state.store.certificate_for_domain_async(new_hostname.clone()).await {
            Ok(Some(value)) => value,
            Ok(None) => return (StatusCode::BAD_REQUEST, "Issue a managed certificate for the new control hostname before switching").into_response(),
            Err(cause) => return internal(cause),
        };
        let generation = match state.store.active_generation_async(managed.id.clone()).await {
            Ok(Some(value)) => value,
            Ok(None) => return (StatusCode::BAD_REQUEST, "The new control hostname certificate has not been issued yet").into_response(),
            Err(cause) => return internal(cause),
        };
        if let Err(cause) = crate::managed_tls::validate_generation(&managed, &generation) {
            return (StatusCode::BAD_REQUEST, format!("The new control hostname certificate is unusable: {cause}")).into_response();
        }
    }
    if let Err(cause) = state.store.ensure_automatic_certificates(&request.config) {
        return (StatusCode::BAD_REQUEST, cause.to_string()).into_response();
    }
    let previous_config = current.config.clone();
    match state
        .store
        .save_config_async(request.config, session.username)
        .await
    {
        Ok(saved) => {
            apply_runtime_config(&state, &previous_config, &saved).await;
            Json(saved).into_response()
        }
        Err(cause) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
    }
}

async fn apply_runtime_config(state: &ControlState, previous: &RuntimeConfig, saved: &crate::store::StoredConfig) {
    if crate::runtime_live::listener_settings_only(previous, &saved.config) {
        crate::runtime_live::store(saved.config.clone());
        if let Err(cause) = crate::forward::apply_hot_listener_settings(&saved.config).await {
            log::error!("hot listener configuration failed: {cause:#}; reloading runtime");
            (state.configuration_changed)();
        } else {
            // A hot-applied revision is proven working; record it so a later
            // failed full reload rolls back to it rather than to the older
            // revision the runtime loop last applied.
            crate::runtime_live::store_last_good(saved.clone());
        }
    } else {
        (state.configuration_changed)();
    }
}

#[derive(Serialize)]
struct PublicProvider {
    id: String,
    name: String,
    directory_url: String,
    contacts: Vec<String>,
    eab_key_id: Option<String>,
    has_eab_hmac_key: bool,
    staging: bool,
    is_default: bool,
    builtin: bool,
    directory_ca_pem: Option<String>,
}

impl From<AcmeProvider> for PublicProvider {
    fn from(value: AcmeProvider) -> Self {
        let builtin = matches!(value.id.as_str(), "letsencrypt-production" | "letsencrypt-staging");
        Self {
            id: value.id,
            name: value.name,
            directory_url: value.directory_url,
            contacts: value.contacts,
            eab_key_id: value.eab_key_id,
            has_eab_hmac_key: value.eab_hmac_key.is_some(),
            staging: value.staging,
            is_default: value.is_default,
            builtin,
            directory_ca_pem: value.directory_ca_pem,
        }
    }
}

async fn list_providers(State(state): State<ControlState>) -> Response {
    match state.store.providers_async().await {
        Ok(values) => Json(values.into_iter().map(PublicProvider::from).collect::<Vec<_>>()).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn save_provider(State(state): State<ControlState>, Json(provider): Json<AcmeProvider>) -> Response {
    // Serialize with other admin mutations so read-modify-write invariants
    // (unique default provider, reference checks) cannot interleave.
    let _write_guard = state.config_write.lock().await;
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || {
        let mut provider = provider;
        match provider.id.as_str() {
            "letsencrypt-production" => {
                provider.name = "Let's Encrypt Production".into();
                provider.directory_url = "https://acme-v02.api.letsencrypt.org/directory".into();
                provider.staging = false;
                provider.directory_ca_pem = None;
            }
            "letsencrypt-staging" => {
                provider.name = "Let's Encrypt Staging".into();
                provider.directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory".into();
                provider.staging = true;
                provider.directory_ca_pem = None;
            }
            _ => {}
        }
        if provider.eab_hmac_key.is_none() {
            if let Some(existing) = store.provider(&provider.id)? {
                provider.eab_hmac_key = existing.eab_hmac_key;
                if provider.eab_key_id.is_none() {
                    provider.eab_key_id = existing.eab_key_id;
                }
                if provider.directory_ca_pem.is_none() { provider.directory_ca_pem = existing.directory_ca_pem; }
            }
        }
        store.save_provider(&provider)
    }).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(cause)) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn delete_provider(State(state): State<ControlState>, Path(id): Path<String>) -> Response {
    let _write_guard = state.config_write.lock().await;
    if matches!(id.as_str(), "letsencrypt-production" | "letsencrypt-staging") {
        return (StatusCode::CONFLICT, "built-in Let's Encrypt providers cannot be deleted").into_response();
    }
    let config = match state.store.load_config() {
        Ok(Some(value)) => value.config,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(cause) => return internal(cause),
    };
    if config.control_plane.provider_id == id {
        return (StatusCode::CONFLICT, "provider is used by the control-plane certificate").into_response();
    }
    match state.store.managed_certificates() {
        Ok(values) if values.iter().any(|certificate| certificate.provider_id == id) => {
            return (StatusCode::CONFLICT, "provider is used by a managed certificate").into_response();
        }
        Ok(_) => {}
        Err(cause) => return internal(cause),
    }
    match state.store.delete_provider(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(cause) => internal(cause),
    }
}

async fn list_certificates(State(state): State<ControlState>) -> Response {
    match state.store.managed_certificates_async().await {
        Ok(values) => Json(values).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn save_certificate(
    State(state): State<ControlState>,
    Json(mut certificate): Json<ManagedCertificate>,
) -> Response {
    let _write_guard = state.config_write.lock().await;
    certificate.automatic = false;
    if certificate.domains.len() == 1 {
        match crate::store::normalize_domain(&certificate.domains[0]) {
            Ok(domain) => { certificate.id = domain.clone(); certificate.domains = vec![domain]; }
            Err(cause) => return (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
        }
    }
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.save_managed_certificate(&certificate)).await {
        Ok(Ok(())) => {
            (state.configuration_changed)();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(cause)) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn delete_certificate(State(state): State<ControlState>, Path(id): Path<String>) -> Response {
    let _write_guard = state.config_write.lock().await;
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.delete_managed_certificate(&id)).await {
        Ok(Ok(true)) => {
            (state.configuration_changed)();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Ok(false)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(cause)) => (StatusCode::CONFLICT, cause.to_string()).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn update_certificate_publish(State(state): State<ControlState>, Path(id): Path<String>, Json(publish): Json<crate::acme_types::PublishPolicy>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || {
        let mut certificate = store.managed_certificate(&id)?.context("managed certificate not found")?;
        certificate.publish = publish;
        store.save_managed_certificate(&certificate)
    }).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(cause)) => (StatusCode::BAD_REQUEST, cause.to_string()).into_response(),
        Err(cause) => internal(cause),
    }
}

async fn request_renewal(State(state): State<ControlState>) -> Response {
    if let Err(cause) = state.store.clear_renewal_retry_gates_async().await {
        return internal(cause);
    }
    (state.request_renewal_scan)();
    StatusCode::ACCEPTED.into_response()
}

fn cookie_value(header: Option<&axum::http::HeaderValue>, name: &str) -> Option<String> {
    header?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_owned()))
}

fn internal(cause: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, cause.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use rcgen::{CertificateParams, KeyPair};

    fn initialized_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .bootstrap(
                &RuntimeConfig::default(),
                &crate::auth::UserRecord {
                    username: "admin".into(),
                    password_hash: crate::auth::hash_password("correct horse battery staple").unwrap(),
                    administrator: true,
                    created_at: Some(OffsetDateTime::now_utc()),
                    disabled: false,
                },
            )
            .unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn form_login_sets_secure_cookie_and_session_hides_token_hash() {
        let (_directory, store) = initialized_store();
        let app = router(ControlState::new(store, || {}));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=admin&password=correct+horse+battery+staple"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("Secure"));
        assert!(set_cookie.contains("SameSite=Strict"));
        let cookie = set_cookie.split(';').next().unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/tlsproxy_api/session")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["username"], "admin");
        assert!(body.get("csrf_token").is_some());
        assert!(body.get("token_hash").is_none());
    }

    #[tokio::test]
    async fn protected_mutations_require_csrf() {
        let (_directory, store) = initialized_store();
        let token = random_token();
        store
            .save_session(&SessionRecord {
                token_hash: token_hash(&token),
                username: "admin".into(),
                csrf_token: "csrf".into(),
                created_at: Some(OffsetDateTime::now_utc()),
                expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
            })
            .unwrap();
        let app = router(ControlState::new(store, || {}));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/tlsproxy_api/renew")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn publication_is_domain_indexed_scoped_etagged_and_key_gated() {
        let (_directory, store) = initialized_store();
        let managed = ManagedCertificate { id: "www.example".into(), domains: vec!["www.example".into()], provider_id: "test".into(), publish: crate::acme_types::PublishPolicy { enabled: true, allow_private_key: false }, ..Default::default() };
        store.save_managed_certificate(&managed).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["www.example".into()]).unwrap().self_signed(&key).unwrap();
        let generation = crate::acme_types::CertificateGeneration { id: "gen-2".into(), certificate_id: managed.id.clone(), certificate_pem: cert.pem(), private_key_pem: key.serialize_pem(), ..Default::default() };
        store.activate_generation(&generation, &crate::acme_types::RenewalState { certificate_id: managed.id.clone(), ..Default::default() }).unwrap();
        let bearer = random_token();
        store.save_retrieval_token(&RetrievalToken { id: "reader".into(), token_hash: token_hash(&bearer), certificate_ids: [managed.id.clone()].into_iter().collect(), expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)), ..Default::default() }).unwrap();
        let app = router(ControlState::new(store.clone(), || {}));
        let response = app.clone().oneshot(Request::builder().uri("/tlsproxy_api/certs/WWW.EXAMPLE.").header(header::AUTHORIZATION, format!("Bearer {bearer}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()[header::ETAG].clone();
        let body: serde_json::Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["certificate_id"], "www.example");
        assert_eq!(body["generation_id"], "gen-2");
        assert!(body["private_key_pem"].is_null());
        let unchanged = app.clone().oneshot(Request::builder().uri("/tlsproxy_api/certs/www.example").header(header::AUTHORIZATION, format!("Bearer {bearer}")).header(header::IF_NONE_MATCH, etag).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        let denied = app.oneshot(Request::builder().uri("/tlsproxy_api/certs/www.example?private_key=true").header(header::AUTHORIZATION, format!("Bearer {bearer}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert!(store.audits(20).unwrap().iter().any(|value| value["kind"] == "certificate_retrieval"));
    }
}
