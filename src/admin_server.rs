use include_dir::{include_dir, Dir};
use regex::Regex;
use std::{
    collections::HashMap,
    error::Error,
    fmt::Display,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

static STATIC: Dir<'_> = include_dir!("static");

use crate::{
    active_tracker, ca,
    config::{AdminServerConfig, Config as PFConfig, Listener},
    controller::Controller,
    manager,
};
use axum::{
    extract::{Path as UrlPath, Request},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use base64::{engine::general_purpose, Engine as _};
use lazy_static::lazy_static;
use log::info;
use serde::{Deserialize, Serialize};
use tokio::{fs, fs::File, io::AsyncWriteExt, sync::RwLock};

lazy_static! {
    static ref LOCK: Arc<RwLock<bool>> = Arc::new(RwLock::new(false));
    static ref RANGE_REGEX: Regex = Regex::new(r"(?i)^bytes\s*=\s*(\d*)\s*-\s*(\d*)\s*$").unwrap();
}

async fn require_auth(request: Request, next: Next) -> Response {
    if is_authorized(request.headers()).await {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"TLS Proxy ACE\", charset=\"UTF-8\"",
            )],
            "Please login",
        )
            .into_response()
    }
}

async fn is_authorized(headers: &HeaderMap) -> bool {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Some(authorization) = authorization else {
        return false;
    };
    let prefix = "basic";
    if !authorization.to_ascii_lowercase().starts_with(prefix) {
        return false;
    }
    let encoded = authorization[prefix.len()..].trim();
    let Ok(decoded) = general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(credentials) = String::from_utf8(decoded) else {
        return false;
    };
    let Some(split) = credentials.find(':') else {
        return false;
    };
    let username = &credentials[..split];
    let password = &credentials[split + 1..];
    let config = CONFIG.read().await;
    match (config.username.as_deref(), config.password.as_deref()) {
        (Some(expected_username), Some(expected_password)) => {
            username == expected_username && password == expected_password
        }
        // no credentials configured: any well-formed Basic header is accepted
        (None, None) => true,
        // half-configured credentials can never match
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ISE {
    pub message: String,
}

impl ISE {
    pub fn from<T>(err: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            message: format!("{err}"),
        }
    }
}
impl Display for ISE {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InternalServerError")
    }
}

impl Error for ISE {}

impl IntoResponse for ISE {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.message).into_response()
    }
}

fn json_response(body: String) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

struct RangeHeader {
    start: Option<usize>,
    end: Option<usize>,
}

impl RangeHeader {
    fn parse(headers: &HeaderMap) -> RangeHeader {
        let range = headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok());
        if let Some(range_str) = range {
            if let Some(caps) = RANGE_REGEX.captures(range_str) {
                let start = caps
                    .get(1)
                    .filter(|m| !m.as_str().is_empty())
                    .and_then(|m| m.as_str().parse::<usize>().ok());
                let end = caps
                    .get(2)
                    .filter(|m| !m.as_str().is_empty())
                    .and_then(|m| m.as_str().parse::<usize>().ok());
                return RangeHeader { start, end };
            }
        }
        RangeHeader {
            start: None,
            end: None,
        }
    }

    pub fn has_value(&self) -> bool {
        return self.start.is_some() || self.end.is_some();
    }

    pub fn align(&self, max_size: usize) -> (usize, usize) {
        let mut start = 0;
        let mut end = max_size;
        if let Some(this_start) = self.start {
            start = this_start;
            if start > max_size {
                start = max_size;
            }
        }

        if let Some(this_end) = self.end {
            end = this_end;
            if end > max_size {
                end = max_size;
            }
            if end < start {
                end = start;
            }
        }

        return (start, end);
    }
}

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn serve_static(path: &str, headers: &HeaderMap) -> Response {
    let Some(entry) = STATIC.get_file(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content: &'static [u8] = entry.contents();
    let content_type = content_type_for(path);
    let range = RangeHeader::parse(headers);
    let total_length = content.len();
    if range.has_value() {
        let (start, end) = range.align(total_length);
        (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_length),
                ),
            ],
            &content[start..end],
        )
            .into_response()
    } else {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
            content,
        )
            .into_response()
    }
}

async fn static_handler(UrlPath(path): UrlPath<String>, headers: HeaderMap) -> Response {
    serve_static(&path, &headers)
}

async fn index(headers: HeaderMap) -> Response {
    serve_static("index.html", &headers)
}

async fn get_listener_config() -> Result<Response, ISE> {
    let _guard = LOCK.read().await;
    let config_file = config_file().await;
    let conf: PFConfig = PFConfig::load_file(&config_file).await.map_err(ISE::from)?;
    let result = serde_json::to_string(&conf.listeners).map_err(ISE::from)?;
    return Ok(json_response(result));
}

async fn get_dns_config() -> Result<Response, ISE> {
    let _guard = LOCK.read().await;
    let config_file = config_file().await;
    let conf: PFConfig = PFConfig::load_file(&config_file).await.map_err(ISE::from)?;
    let result = serde_json::to_string(&conf.dns).map_err(ISE::from)?;
    return Ok(json_response(result));
}

fn convert_error<T, X>(input: Result<T, X>) -> Result<T, ISE>
where
    X: Display,
{
    input.map_err(|e| ISE::from(e))
}

async fn put_dns_config(data: String) -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let config_file = config_file().await;
    let dns: crate::config::DnsConfig = convert_error(serde_json::from_str(&data))?;
    for rule in &dns.regex {
        convert_error(
            crate::resolver::compile_dns_pattern(&rule.hostname)
                .map_err(|cause| format!("invalid DNS regex `{}`: {cause}", rule.hostname)),
        )?;
    }
    let mut conf: PFConfig = convert_error(PFConfig::load_file(&config_file).await)?;
    conf.dns = dns;
    write_config_file(&config_file, &conf).await?;
    convert_error(crate::forward::reconcile_configured_listeners(&conf.listeners).await)?;
    Ok(json_response(data))
}

async fn put_listener_config(data: String) -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let config_file = config_file().await;
    let map: HashMap<String, Listener> = convert_error(serde_json::from_str(&data))?;
    let mut conf: PFConfig = convert_error(PFConfig::load_file(&config_file).await)?;
    conf.listeners = map;
    write_config_file(&config_file, &conf).await?;
    convert_error(crate::forward::reconcile_configured_listeners(&conf.listeners).await)?;
    Ok(json_response(data))
}

async fn get_listener_status() -> Result<Response, ISE> {
    let result = manager::get_listener_status().await;
    let mut result_converted = HashMap::new();
    for (key, value) in result {
        let new_error = value.map_err(ISE::from);
        result_converted.insert(key, new_error);
    }
    let result = convert_error(serde_json::to_string(&result_converted))?;
    return Ok(json_response(result));
}

async fn get_listener_stats() -> Result<Response, ISE> {
    let result = manager::get_listener_stats().await;
    let result = convert_error(serde_json::to_string(&result))?;
    return Ok(json_response(result));
}

async fn get_active_connections() -> Result<Response, ISE> {
    let active = active_tracker::get_active_list().await;
    let result = convert_error(serde_json::to_string(&active))?;
    Ok(json_response(result))
}

async fn get_manager_status() -> Result<Response, ISE> {
    let status = manager::get_manager_status().await;
    let result = convert_error(serde_json::to_string(&status))?;
    Ok(json_response(result))
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SimpleOperationResult {
    pub success: bool,
    pub changed: bool,
    pub message: Option<String>,
}

impl SimpleOperationResult {
    pub fn ok(message: Option<String>) -> Self {
        Self {
            success: true,
            changed: true,
            message,
        }
    }

    pub fn ok_no_change(message: Option<String>) -> Self {
        Self {
            success: true,
            changed: false,
            message,
        }
    }

    pub fn fail(message: &str) -> Self {
        Self {
            success: false,
            changed: false,
            message: Some(message.into()),
        }
    }
}

async fn stop() -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let current_status = manager::get_run_status().await;
    let mut result = SimpleOperationResult::ok(None);
    match current_status {
        manager::Status::STARTED => {
            manager::stop().await;
        }
        manager::Status::STOPPED => {
            result = SimpleOperationResult::ok_no_change(None);
        }
        _ => {
            result = SimpleOperationResult::fail("should not happen");
        }
    }
    return Ok(json_response(serde_json::to_string(&result).unwrap()));
}

async fn start() -> Result<Response, ISE> {
    let _w = LOCK.write().await;
    let current_status = manager::get_run_status().await;
    match current_status {
        manager::Status::STOPPED => {
            drop(_w);
            return restart_and_apply_config().await;
        }
        _ => {
            let result = SimpleOperationResult::ok_no_change(None);
            return Ok(json_response(serde_json::to_string(&result).unwrap()));
        }
    }
}

async fn restart_and_apply_config() -> Result<Response, ISE> {
    let _guard = LOCK.write().await;

    let config_file = config_file().await;
    let conf: PFConfig = convert_error(PFConfig::load_file(&config_file).await)?;
    {
        let mut last_w = LAST_CONFIG.write().await;
        *last_w = conf.clone();
    }
    info!("stopping manager...");
    manager::stop().await;
    info!("manager stopped");
    info!("starting manager...");
    let result = convert_error(manager::start(conf).await)?;
    info!("manager started");
    let mut result_converted = HashMap::new();
    for (key, value) in result {
        let new_error = value.map_err(ISE::from);
        result_converted.insert(key, new_error);
    }
    let result = convert_error(serde_json::to_string(&result_converted))?;
    return Ok(json_response(result));
}

async fn start_listener(UrlPath(name): UrlPath<String>) -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let config_file = config_file().await;
    let conf: PFConfig = convert_error(PFConfig::load_file(&config_file).await)?;
    let result = convert_error(manager::start_listener(&name, conf).await)?;
    let result = serialize_listener_operation_result(result)?;
    Ok(json_response(result))
}

async fn stop_listener(UrlPath(name): UrlPath<String>) -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let changed = convert_error(manager::stop_listener(&name).await)?;
    let result = SimpleOperationResult {
        success: true,
        changed,
        message: None,
    };
    Ok(json_response(serde_json::to_string(&result).unwrap()))
}

async fn restart_listener(UrlPath(name): UrlPath<String>) -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let config_file = config_file().await;
    let conf: PFConfig = convert_error(PFConfig::load_file(&config_file).await)?;
    let result = convert_error(manager::restart_listener(&name, conf).await)?;
    let result = serialize_listener_operation_result(result)?;
    Ok(json_response(result))
}

fn serialize_listener_operation_result(
    result: HashMap<String, Result<manager::ListenerStatusSerde, anyhow::Error>>,
) -> Result<String, ISE> {
    let mut result_converted = HashMap::new();
    for (key, value) in result {
        let new_error = value.map_err(ISE::from);
        result_converted.insert(key, new_error);
    }
    convert_error(serde_json::to_string(&result_converted))
}

async fn reset_original_config() -> Result<Response, ISE> {
    let _guard = LOCK.write().await;
    let config_file = config_file().await;
    let old = LAST_CONFIG.read().await;
    let old_dns = old.dns.clone();
    let old_listeners = old.listeners.clone();

    let mut conf: PFConfig = convert_error(PFConfig::load_file(&config_file).await)?;
    conf.listeners = old_listeners;
    conf.dns = old_dns;
    write_config_file(&config_file, &conf).await?;
    let json_result = serde_json::to_string("OK").unwrap();
    Ok(json_response(json_result))
}

lazy_static! {
    static ref CONFIG: Arc<RwLock<AdminServerConfig>> = Arc::new(RwLock::new(Default::default()));
    static ref ADMIN_ENABLED: Arc<RwLock<bool>> = Arc::new(RwLock::new(false));
    static ref CONFIG_FILE: Arc<RwLock<PathBuf>> =
        Arc::new(RwLock::new(PathBuf::from("config.yaml")));
    static ref LAST_CONFIG: Arc<RwLock<PFConfig>> = Arc::new(RwLock::new(Default::default()));
}

async fn config_file() -> PathBuf {
    CONFIG_FILE.read().await.clone()
}

async fn write_config_file(config_file: &Path, config: &PFConfig) -> Result<(), ISE> {
    let yamlout = convert_error(serde_yaml_ng::to_string(config))?;
    if let Some(parent) = config_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        convert_error(fs::create_dir_all(parent).await)?;
    }
    let temporary = config_file.with_extension(format!(
        "{}.tmp",
        config_file
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("yaml")
    ));
    let mut file_out = convert_error(File::create(&temporary).await)?;
    convert_error(file_out.write_all(yamlout.as_bytes()).await)?;
    convert_error(file_out.sync_all().await)?;
    drop(file_out);
    convert_error(fs::rename(&temporary, config_file).await)?;
    info!("wrote config `{}` atomically", config_file.display());
    Ok(())
}

pub async fn enabled() -> bool {
    *ADMIN_ENABLED.read().await
}

pub async fn init(config_file: &Path, config: &PFConfig) -> Result<(), Box<dyn Error>> {
    info!("initializing adminserver...");
    {
        let mut w = CONFIG_FILE.write().await;
        *w = config_file.to_path_buf();
    }
    {
        let mut w = LAST_CONFIG.write().await;
        *w = config.clone();
    }
    let admin_config = config.admin_server.clone();
    match admin_config {
        Some(what) => {
            *ADMIN_ENABLED.write().await = true;
            let mut w = CONFIG.write().await;
            *w = what;
        }
        None => {
            *ADMIN_ENABLED.write().await = false;
            info!("admin server disabled (admin_server section is absent)");
            return Ok(());
        }
    }
    info!("initialized admin server");
    Ok(())
}

fn choose<T>(first: &Option<T>, default: T) -> T
where
    T: Clone,
{
    let first_c = first.clone();
    match first_c {
        Some(what) => {
            return what;
        }
        _ => {
            return default;
        }
    }
}

fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route(
            "/apiserver/config/listeners",
            get(get_listener_config).put(put_listener_config),
        )
        .route(
            "/apiserver/config/dns",
            get(get_dns_config).put(put_dns_config),
        )
        .route("/apiserver/status/listeners", get(get_listener_status))
        .route("/apiserver/status/manager", get(get_manager_status))
        .route("/apiserver/stats/listeners", get(get_listener_stats))
        .route("/apiserver/active/listeners", get(get_active_connections))
        .route(
            "/apiserver/config/listeners/{name}/start",
            post(start_listener),
        )
        .route(
            "/apiserver/config/listeners/{name}/stop",
            post(stop_listener),
        )
        .route(
            "/apiserver/config/listeners/{name}/restart",
            post(restart_listener),
        )
        .route("/apiserver/config/stop", post(stop))
        .route("/apiserver/config/start", post(start))
        .route("/apiserver/config/apply", post(restart_and_apply_config))
        .route("/apiserver/config/reset", post(reset_original_config))
        .route("/{*path}", get(static_handler))
        .layer(middleware::from_fn(require_auth))
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    if !enabled().await {
        info!("admin server disabled; not starting admin UI");
        return Ok(());
    }
    let config = CONFIG.read().await.clone();
    let bind_port = choose(&config.bind_port, 48888);
    let bind_address = choose(&config.bind_address, "0.0.0.0".into());
    let bind_address = crate::bindaddr::resolve_bind_addr(&bind_address)?;
    let addr: SocketAddr = format!("{bind_address}:{bind_port}").parse()?;
    let app = router();

    let enable_tls = choose(&config.tls, false);
    let protocol = if enable_tls { "https" } else { "http" };
    info!(
        "admin UI listening on {protocol}://localhost:{bind_port} (bound to {bind_address}, Basic Auth required)"
    );
    if enable_tls {
        let mut admin_controller = Controller::new();
        let full_config = LAST_CONFIG.read().await.clone();
        let ca = ca::LocalCa::from_config(&full_config)?;
        ca.spawn_eviction_job(&mut admin_controller);
        let rustls_config = RustlsConfig::from_config(ca.admin_server_config(config.san.clone()));
        axum_server::bind_rustls(addr, rustls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        axum_server::bind(addr)
            .serve(app.into_make_service())
            .await?;
    }
    info!("admin server over");
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Options, Rules};
    use regex::Regex;
    use tempfile::tempdir;

    fn config_with_listener(bind: &str) -> PFConfig {
        let mut config = PFConfig {
            options: Options::default(),
            ..Default::default()
        };
        config.listeners.insert(
            "test".into(),
            Listener {
                bind: bind.into(),
                target: None,
                target_port: 443,
                policy: crate::config::Policy::DENY,
                rules: Rules {
                    static_hosts: Vec::new(),
                    patterns: vec![Regex::new("^blocked\\.example$").unwrap()],
                },
                max_idle_time_ms: None,
                speed_limit: None,
                mode: crate::config::ListenerMode::Passthrough,
                upstream_tls: false,
            },
        );
        config
    }

    #[tokio::test]
    async fn write_config_file_replaces_existing_file_with_valid_yaml() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        fs::write(&path, b"not: valid: yaml").await.unwrap();

        write_config_file(&path, &config_with_listener("127.0.0.1:9443"))
            .await
            .unwrap();

        let loaded = PFConfig::load_file(&path).await.unwrap();
        assert_eq!(loaded.listeners["test"].bind, "127.0.0.1:9443");
        assert!(!path.with_extension("yaml.tmp").exists());
    }

    #[tokio::test]
    async fn absent_admin_server_disables_admin_ui() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        let mut config = config_with_listener("127.0.0.1:9444");
        config.admin_server = None;

        init(&path, &config).await.unwrap();

        assert!(!enabled().await);
    }
}
