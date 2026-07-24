use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options, WriteBatch};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use time::OffsetDateTime;
use base64::Engine as _;

use crate::acme_types::{
    AcmeProvider, CertificateGeneration, DnsDiagnostic, ManagedCertificate, RenewalState,
    RetrievalToken, StoredAcmeAccount,
};
use crate::auth::{SessionRecord, UserRecord};
use crate::runtime_config::RuntimeConfig;

pub const SCHEMA_VERSION: u32 = 1;

const CF_METADATA: &str = "metadata";
const CF_CONFIG: &str = "config";
const CF_CONFIG_HISTORY: &str = "config_history";
const CF_PROVIDERS: &str = "providers";
const CF_ACCOUNTS: &str = "accounts";
const CF_CERTIFICATES: &str = "certificates";
const CF_DOMAIN_INDEX: &str = "domain_index";
const CF_GENERATIONS: &str = "generations";
const CF_ACTIVE_GENERATIONS: &str = "active_generations";
const CF_RENEWALS: &str = "renewals";
const CF_TOKENS: &str = "tokens";
const CF_USERS: &str = "users";
const CF_SESSIONS: &str = "sessions";
const CF_AUDIT: &str = "audit";
const CF_DNS_DIAGNOSTICS: &str = "dns_diagnostics";
const CF_HEALTH_CHECK_STATUS: &str = "health_check_status";

const ALL_CFS: &[&str] = &[
    CF_METADATA,
    CF_CONFIG,
    CF_CONFIG_HISTORY,
    CF_PROVIDERS,
    CF_ACCOUNTS,
    CF_CERTIFICATES,
    CF_DOMAIN_INDEX,
    CF_GENERATIONS,
    CF_ACTIVE_GENERATIONS,
    CF_RENEWALS,
    CF_TOKENS,
    CF_USERS,
    CF_SESSIONS,
    CF_AUDIT,
    CF_DNS_DIAGNOSTICS,
    CF_HEALTH_CHECK_STATUS,
];

const KEY_SCHEMA_VERSION: &[u8] = b"schema_version";
const KEY_CURRENT_CONFIG: &[u8] = b"current";
const KEY_CONFIG_REVISION: &[u8] = b"revision";

type RocksDb = DBWithThreadMode<MultiThreaded>;

#[derive(Clone)]
pub struct Store {
    db: Arc<RocksDb>,
    path: Arc<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredConfig {
    pub revision: u64,
    pub updated_at: OffsetDateTime,
    pub updated_by: String,
    pub config: RuntimeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckSample {
    pub listener: String,
    pub host: String,
    pub path: String,
    pub backend: String,
    pub endpoint: String,
    pub transport: String,
    pub load_balancing: String,
    pub online: bool,
    pub checked_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RocksDbJsonExport {
    pub format: String,
    pub schema_version: u32,
    pub exported_at: OffsetDateTime,
    pub column_families: std::collections::BTreeMap<String, Vec<RocksDbJsonEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RocksDbJsonEntry {
    pub key_base64: String,
    pub value_base64: String,
}

impl Store {
    pub fn export_json(&self) -> Result<RocksDbJsonExport> {
        let mut column_families = std::collections::BTreeMap::new();
        for name in std::iter::once("default").chain(ALL_CFS.iter().copied()).filter(|name| *name != CF_HEALTH_CHECK_STATUS) {
            let cf = self.cf(name)?;
            let entries = self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start).map(|item| {
                let (key, value) = item?;
                Ok(RocksDbJsonEntry {
                    key_base64: base64::engine::general_purpose::STANDARD.encode(key),
                    value_base64: base64::engine::general_purpose::STANDARD.encode(value),
                })
            }).collect::<Result<Vec<_>>>()?;
            column_families.insert(name.to_string(), entries);
        }
        Ok(RocksDbJsonExport { format: "tlsproxy-rocksdb-json-v1".into(), schema_version: SCHEMA_VERSION, exported_at: OffsetDateTime::now_utc(), column_families })
    }

    /// Imports the export's entries. When `selected` is `Some`, only the named
    /// column families are imported (and, under `clear_existing`, only those
    /// are cleared) — a scoped restore that leaves other families untouched.
    /// `None` imports every family present in the export.
    pub fn import_json(
        &self,
        export: &RocksDbJsonExport,
        clear_existing: bool,
        selected: Option<&std::collections::HashSet<String>>,
    ) -> Result<usize> {
        if export.format != "tlsproxy-rocksdb-json-v1" { bail!("unsupported RocksDB JSON export format"); }
        if export.schema_version != SCHEMA_VERSION { bail!("export schema version {} does not match runtime schema version {SCHEMA_VERSION}", export.schema_version); }
        let allowed = std::iter::once("default").chain(ALL_CFS.iter().copied()).filter(|name| *name != CF_HEALTH_CHECK_STATUS).collect::<std::collections::HashSet<_>>();
        if let Some(selected) = selected {
            if let Some(unknown) = selected.iter().find(|name| !allowed.contains(name.as_str())) {
                bail!("unknown or excluded column family `{unknown}`");
            }
            if selected.is_empty() { bail!("select at least one column family to import"); }
        }
        let mut batch = WriteBatch::default();
        let mut imported = 0usize;
        for (name, entries) in &export.column_families {
            if !allowed.contains(name.as_str()) { bail!("unknown or excluded column family `{name}`"); }
            if selected.is_some_and(|selected| !selected.contains(name)) { continue; }
            let cf = self.cf(name)?;
            // Clear happens per imported family so a scoped clear-and-import
            // never wipes families the caller did not choose.
            if clear_existing {
                for item in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
                    let (key, _) = item?;
                    batch.delete_cf(&cf, key);
                }
            }
            for entry in entries {
                let key = base64::engine::general_purpose::STANDARD.decode(&entry.key_base64).with_context(|| format!("invalid base64 key in `{name}`"))?;
                let value = base64::engine::general_purpose::STANDARD.decode(&entry.value_base64).with_context(|| format!("invalid base64 value in `{name}`"))?;
                batch.put_cf(&cf, key, value);
                imported += 1;
            }
        }
        self.db.write(batch)?;
        Ok(imported)
    }

    pub fn open(runtime_dir: impl AsRef<Path>) -> Result<Self> {
        let path = runtime_dir.as_ref().join("tlsproxy.rocksdb");
        std::fs::create_dir_all(runtime_dir.as_ref()).with_context(|| {
            format!(
                "failed to create runtime directory `{}`",
                runtime_dir.as_ref().display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(runtime_dir.as_ref(), std::fs::Permissions::from_mode(0o700))
                .with_context(|| {
                    format!(
                        "failed to restrict runtime directory `{}`",
                        runtime_dir.as_ref().display()
                    )
                })?;
        }

        let mut db_options = Options::default();
        db_options.create_if_missing(true);
        db_options.create_missing_column_families(true);
        let descriptors = ALL_CFS.iter().map(|name| {
            let mut options = Options::default();
            options.set_compression_type(rocksdb::DBCompressionType::Lz4);
            ColumnFamilyDescriptor::new(*name, options)
        });
        let db = RocksDb::open_cf_descriptors(&db_options, &path, descriptors)
            .with_context(|| format!("failed to open RocksDB `{}`", path.display()))?;
        let store = Self {
            db: Arc::new(db),
            path: Arc::new(path),
        };
        store.initialize_schema()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn checkpoint(&self, destination: impl AsRef<Path>) -> Result<()> {
        let destination = destination.as_ref();
        if destination.exists() {
            bail!("checkpoint destination `{}` already exists", destination.display());
        }
        rocksdb::checkpoint::Checkpoint::new(&self.db)?
            .create_checkpoint(destination)
            .with_context(|| format!("failed to create checkpoint `{}`", destination.display()))
    }

    pub fn save_retrieval_token(&self, token: &RetrievalToken) -> Result<()> {
        if token.id.trim().is_empty() || token.token_hash.len() != 64 {
            bail!("retrieval token ID and SHA-256 hash are required");
        }
        self.put_json(CF_TOKENS, token.id.as_bytes(), token)
    }

    pub fn retrieval_token_by_hash(&self, hash: &str) -> Result<Option<RetrievalToken>> {
        let cf = self.cf(CF_TOKENS)?;
        for item in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (_, value) = item?;
            let token: RetrievalToken = serde_json::from_slice(&value)?;
            if subtle::ConstantTimeEq::ct_eq(token.token_hash.as_bytes(), hash.as_bytes()).into() {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    pub fn retrieval_tokens(&self) -> Result<Vec<RetrievalToken>> {
        let cf = self.cf(CF_TOKENS)?;
        self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start).map(|item| {
            let (_, value) = item?;
            Ok(serde_json::from_slice(&value)?)
        }).collect()
    }

    pub fn delete_retrieval_token(&self, id: &str) -> Result<()> {
        self.db.delete_cf(&self.cf(CF_TOKENS)?, id.as_bytes())?;
        Ok(())
    }

    pub fn append_audit(&self, kind: &str, value: serde_json::Value) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let sequence = u64::try_from(now.unix_timestamp_nanos()).unwrap_or(u64::MAX);
        self.put_json(CF_AUDIT, &audit_key(kind, sequence), &serde_json::json!({
            "kind": kind, "timestamp": now, "detail": value
        }))
    }

    pub fn audits(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let cf = self.cf(CF_AUDIT)?;
        let mut values = self.db.iterator_cf(&cf, rocksdb::IteratorMode::End)
            .take(limit).map(|item| {
                let (_, value) = item?;
                Ok(serde_json::from_slice(&value)?)
            }).collect::<Result<Vec<_>>>()?;
        values.reverse();
        Ok(values)
    }

    pub fn save_dns_diagnostics(&self, certificate_id: &str, values: &[DnsDiagnostic]) -> Result<()> {
        let cf = self.cf(CF_DNS_DIAGNOSTICS)?;
        let prefix = format!("{certificate_id}\0");
        let mut batch = WriteBatch::default();
        for item in self.db.prefix_iterator_cf(&cf, prefix.as_bytes()) {
            let (key, _) = item?;
            if !key.starts_with(prefix.as_bytes()) { break; }
            batch.delete_cf(&cf, key);
        }
        for value in values {
            let key = format!("{}\0{}\0{}", certificate_id, value.domain, value.resolver);
            batch.put_cf(&cf, key.as_bytes(), serde_json::to_vec(value)?);
        }
        self.db.write(batch)?;
        Ok(())
    }

    pub fn dns_diagnostics(&self) -> Result<Vec<DnsDiagnostic>> {
        let cf = self.cf(CF_DNS_DIAGNOSTICS)?;
        self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start).map(|item| {
            let (_, value) = item?;
            Ok(serde_json::from_slice(&value)?)
        }).collect()
    }

    pub fn save_health_check_samples(&self, values: &[HealthCheckSample]) -> Result<()> {
        let cf = self.cf(CF_HEALTH_CHECK_STATUS)?;
        let mut batch = WriteBatch::default();
        for value in values {
            let nanos = value.checked_at.unix_timestamp_nanos();
            let identity = format!("{}\0{}\0{}\0{}\0{}", value.listener, value.host, value.path, value.backend, value.endpoint);
            let key = format!("{nanos:039}\0{identity}");
            batch.put_cf(&cf, key.as_bytes(), serde_json::to_vec(value)?);
        }
        let cutoff = OffsetDateTime::now_utc() - time::Duration::hours(24);
        let cutoff_nanos = cutoff.unix_timestamp_nanos();
        for item in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, _) = item?;
            let timestamp = std::str::from_utf8(key.get(..39).unwrap_or_default()).ok().and_then(|value| value.parse::<i128>().ok());
            if timestamp.is_some_and(|value| value < cutoff_nanos) { batch.delete_cf(&cf, key); } else { break; }
        }
        self.db.write(batch)?;
        Ok(())
    }

    pub async fn save_health_check_samples_async(&self, values: Vec<HealthCheckSample>) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_health_check_samples(&values))
            .await
            .context("health-check database task failed")?
    }

    /// Loads, filters, and sorts one endpoint's 24-hour history off the async
    /// executor; the column family holds every endpoint's samples.
    pub async fn health_check_history_filtered_async(
        &self,
        listener: String,
        host: String,
        backend: String,
        endpoint: String,
    ) -> Result<Vec<HealthCheckSample>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut values = store.health_check_history()?;
            values.retain(|value| {
                value.listener == listener
                    && value.host == host
                    && value.backend == backend
                    && value.endpoint == endpoint
            });
            values.sort_by_key(|value| value.checked_at);
            Ok(values)
        })
        .await
        .context("health-check database task failed")?
    }

    pub fn health_check_history(&self) -> Result<Vec<HealthCheckSample>> {
        let cf = self.cf(CF_HEALTH_CHECK_STATUS)?;
        let cutoff = OffsetDateTime::now_utc() - time::Duration::hours(24);
        self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start).filter_map(|item| match item {
            Ok((_, raw)) => match serde_json::from_slice::<HealthCheckSample>(&raw) {
                Ok(value) if value.checked_at >= cutoff => Some(Ok(value)),
                Ok(_) => None,
                Err(cause) => Some(Err(cause.into())),
            },
            Err(cause) => Some(Err(cause.into())),
        }).collect()
    }

    fn cf(&self, name: &str) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| anyhow!("RocksDB column family `{name}` is missing"))
    }

    fn initialize_schema(&self) -> Result<()> {
        let metadata = self.cf(CF_METADATA)?;
        let stored = self.db.get_cf(&metadata, KEY_SCHEMA_VERSION)?;
        match stored {
            None => self
                .db
                .put_cf(&metadata, KEY_SCHEMA_VERSION, SCHEMA_VERSION.to_be_bytes())?,
            Some(bytes) => {
                let bytes: [u8; 4] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("invalid database schema version"))?;
                let version = u32::from_be_bytes(bytes);
                if version > SCHEMA_VERSION {
                    bail!(
                        "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
                    );
                }
                if version < SCHEMA_VERSION {
                    bail!("database schema version {version} requires migration");
                }
            }
        }
        Ok(())
    }

    pub fn is_initialized(&self) -> Result<bool> {
        let cf = self.cf(CF_CONFIG)?;
        Ok(self.db.get_cf(&cf, KEY_CURRENT_CONFIG)?.is_some())
    }

    pub fn load_config(&self) -> Result<Option<StoredConfig>> {
        self.get_json(CF_CONFIG, KEY_CURRENT_CONFIG)
    }

    pub async fn load_config_async(&self) -> Result<Option<StoredConfig>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load_config())
            .await
            .context("configuration database task failed")?
    }

    pub fn bootstrap(&self, config: &RuntimeConfig, administrator: &UserRecord) -> Result<StoredConfig> {
        if self.is_initialized()? {
            bail!("initial setup is already complete");
        }
        if administrator.username.trim().is_empty()
            || administrator.password_hash.is_empty()
            || !administrator.administrator
        {
            bail!("a valid initial administrator is required");
        }
        config
            .validate()
            .map_err(|cause| anyhow!(cause.to_string()))?;
        let stored = StoredConfig {
            revision: 1,
            updated_at: OffsetDateTime::now_utc(),
            updated_by: administrator.username.clone(),
            config: config.clone(),
        };
        let config_cf = self.cf(CF_CONFIG)?;
        let history_cf = self.cf(CF_CONFIG_HISTORY)?;
        let metadata_cf = self.cf(CF_METADATA)?;
        let users_cf = self.cf(CF_USERS)?;
        let audit_cf = self.cf(CF_AUDIT)?;
        let certificates_cf = self.cf(CF_CERTIFICATES)?;
        let domain_index_cf = self.cf(CF_DOMAIN_INDEX)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(
            &config_cf,
            KEY_CURRENT_CONFIG,
            serde_json::to_vec(&stored)?,
        );
        batch.put_cf(&history_cf, 1u64.to_be_bytes(), serde_json::to_vec(&stored)?);
        batch.put_cf(
            &metadata_cf,
            KEY_CONFIG_REVISION,
            1u64.to_be_bytes(),
        );
        batch.put_cf(
            &users_cf,
            administrator.username.as_bytes(),
            serde_json::to_vec(administrator)?,
        );
        batch.put_cf(
            &audit_cf,
            audit_key("bootstrap", 1),
            serde_json::to_vec(&serde_json::json!({
                "kind": "initial_setup_completed",
                "actor": administrator.username,
                "timestamp": stored.updated_at,
            }))?,
        );
        if !config.control_plane.hostname.is_empty() && !config.control_plane.provider_id.is_empty() {
            let domain = normalize_domain(&config.control_plane.hostname)?;
            let control_certificate = ManagedCertificate {
                id: domain.clone(),
                domains: vec![domain.clone()],
                provider_id: config.control_plane.provider_id.clone(),
                enabled: true,
                automatic: true,
                publish: Default::default(),
                created_at: Some(stored.updated_at),
                updated_at: Some(stored.updated_at),
            };
            batch.put_cf(
                &certificates_cf,
                control_certificate.id.as_bytes(),
                serde_json::to_vec(&control_certificate)?,
            );
            batch.put_cf(&domain_index_cf, domain.as_bytes(), control_certificate.id.as_bytes());
        }
        self.db.write(batch)?;
        Ok(stored)
    }

    pub fn user(&self, username: &str) -> Result<Option<UserRecord>> {
        self.get_json(CF_USERS, username.as_bytes())
    }

    pub async fn user_async(&self, username: String) -> Result<Option<UserRecord>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.user(&username))
            .await
            .context("user database task failed")?
    }

    pub fn save_user(&self, user: &UserRecord) -> Result<()> {
        if user.username.trim().is_empty() || user.password_hash.is_empty() {
            bail!("username and password hash are required");
        }
        self.put_json(CF_USERS, user.username.as_bytes(), user)
    }

    pub fn save_session(&self, session: &SessionRecord) -> Result<()> {
        if session.token_hash.is_empty() || session.username.is_empty() {
            bail!("session token hash and username are required");
        }
        self.put_json(CF_SESSIONS, session.token_hash.as_bytes(), session)
    }

    pub fn session(&self, token_hash: &str) -> Result<Option<SessionRecord>> {
        self.get_json(CF_SESSIONS, token_hash.as_bytes())
    }

    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        let cf = self.cf(CF_SESSIONS)?;
        self.db.delete_cf(&cf, token_hash.as_bytes())?;
        Ok(())
    }

    pub fn delete_user_sessions(&self, username: &str) -> Result<usize> {
        let cf = self.cf(CF_SESSIONS)?;
        let mut keys = Vec::new();
        for entry in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, value) = entry?;
            let session: SessionRecord = serde_json::from_slice(&value)?;
            if session.username == username {
                keys.push(key.to_vec());
            }
        }
        let count = keys.len();
        let mut batch = WriteBatch::default();
        for key in keys {
            batch.delete_cf(&cf, key);
        }
        self.db.write(batch)?;
        Ok(count)
    }

    pub async fn save_session_async(&self, session: SessionRecord) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_session(&session))
            .await
            .context("session database task failed")?
    }

    pub async fn session_async(&self, token_hash: String) -> Result<Option<SessionRecord>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.session(&token_hash))
            .await
            .context("session database task failed")?
    }

    pub async fn delete_session_async(&self, token_hash: String) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.delete_session(&token_hash))
            .await
            .context("session database task failed")?
    }

    pub fn save_config(&self, config: &RuntimeConfig, updated_by: &str) -> Result<StoredConfig> {
        config
            .validate()
            .map_err(|cause| anyhow!(cause.to_string()))?;
        let metadata = self.cf(CF_METADATA)?;
        let revision = self
            .db
            .get_cf(&metadata, KEY_CONFIG_REVISION)?
            .map(|bytes| {
                let value: [u8; 8] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("invalid configuration revision"))?;
                Ok::<_, anyhow::Error>(u64::from_be_bytes(value))
            })
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow!("configuration revision overflow"))?;
        let stored = StoredConfig {
            revision,
            updated_at: OffsetDateTime::now_utc(),
            updated_by: updated_by.to_owned(),
            config: config.clone(),
        };
        let encoded = serde_json::to_vec(&stored)?;
        let mut batch = WriteBatch::default();
        let config_cf = self.cf(CF_CONFIG)?;
        let history_cf = self.cf(CF_CONFIG_HISTORY)?;
        let audit_cf = self.cf(CF_AUDIT)?;
        batch.put_cf(&config_cf, KEY_CURRENT_CONFIG, encoded);
        batch.put_cf(&history_cf, revision.to_be_bytes(), serde_json::to_vec(&stored)?);
        batch.put_cf(
            &metadata,
            KEY_CONFIG_REVISION,
            revision.to_be_bytes(),
        );
        batch.put_cf(
            &audit_cf,
            audit_key("config", revision),
            serde_json::to_vec(&serde_json::json!({
                "kind": "configuration_saved",
                "revision": revision,
                "actor": updated_by,
                "timestamp": stored.updated_at,
            }))?,
        );
        self.db.write(batch)?;
        Ok(stored)
    }

    pub fn config_history(&self, limit: usize) -> Result<Vec<StoredConfig>> {
        let cf = self.cf(CF_CONFIG_HISTORY)?;
        self.db.iterator_cf(&cf, rocksdb::IteratorMode::End).take(limit).map(|item| {
            let (_, value) = item?;
            Ok(serde_json::from_slice(&value)?)
        }).collect()
    }

    pub fn config_revision(&self, revision: u64) -> Result<Option<StoredConfig>> {
        self.get_json(CF_CONFIG_HISTORY, &revision.to_be_bytes())
    }

    pub fn cleanup_retention(&self, now: OffsetDateTime, generations_per_certificate: usize, audit_days: i64) -> Result<RetentionSummary> {
        let sessions = self.cf(CF_SESSIONS)?;
        let generations = self.cf(CF_GENERATIONS)?;
        let active = self.cf(CF_ACTIVE_GENERATIONS)?;
        let audit = self.cf(CF_AUDIT)?;
        let history = self.cf(CF_CONFIG_HISTORY)?;
        let mut batch = WriteBatch::default();
        let mut summary = RetentionSummary::default();
        for item in self.db.iterator_cf(&sessions, rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            let session: SessionRecord = serde_json::from_slice(&value)?;
            if session.expires_at.is_none_or(|expiry| expiry <= now) { batch.delete_cf(&sessions, key); summary.sessions += 1; }
        }
        let mut grouped = std::collections::BTreeMap::<String, Vec<(Vec<u8>, CertificateGeneration)>>::new();
        for item in self.db.iterator_cf(&generations, rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            let generation: CertificateGeneration = serde_json::from_slice(&value)?;
            grouped.entry(generation.certificate_id.clone()).or_default().push((key.to_vec(), generation));
        }
        for (certificate_id, mut values) in grouped {
            values.sort_by_key(|(_, value)| std::cmp::Reverse(value.issued_at));
            let active_id = self.db.get_cf(&active, certificate_id.as_bytes())?;
            let mut kept_nonactive = 0usize;
            let nonactive_limit = generations_per_certificate.saturating_sub(usize::from(active_id.is_some()));
            for (key, value) in values {
                let is_active = active_id.as_deref() == Some(value.id.as_bytes());
                if is_active { continue; }
                if kept_nonactive < nonactive_limit { kept_nonactive += 1; } else { batch.delete_cf(&generations, key); summary.generations += 1; }
            }
        }
        let cutoff = now - time::Duration::days(audit_days.max(1));
        for item in self.db.iterator_cf(&audit, rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            let value: serde_json::Value = serde_json::from_slice(&value)?;
            let timestamp = value.get("timestamp").and_then(|v| serde_json::from_value::<OffsetDateTime>(v.clone()).ok());
            if timestamp.is_some_and(|at| at < cutoff) { batch.delete_cf(&audit, key); summary.audits += 1; }
        }
        let history_keys = self.db.iterator_cf(&history, rocksdb::IteratorMode::End).skip(50)
            .map(|item| item.map(|(key, _)| key.to_vec())).collect::<std::result::Result<Vec<_>, _>>()?;
        for key in history_keys { batch.delete_cf(&history, key); summary.config_revisions += 1; }
        self.db.write(batch)?;
        Ok(summary)
    }

    pub async fn save_config_async(
        &self,
        config: RuntimeConfig,
        updated_by: String,
    ) -> Result<StoredConfig> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_config(&config, &updated_by))
            .await
            .context("configuration database task failed")?
    }

    pub fn save_provider(&self, provider: &AcmeProvider) -> Result<()> {
        if provider.id.trim().is_empty() {
            bail!("ACME provider ID is required");
        }
        if provider.directory_url.trim().is_empty() {
            bail!("ACME provider directory URL is required");
        }
        if !provider.directory_url.starts_with("https://") {
            bail!("ACME provider directory URL must use HTTPS");
        }
        if provider.eab_key_id.is_some() != provider.eab_hmac_key.is_some() {
            bail!("ACME external account binding requires both key ID and HMAC key");
        }
        let providers = self.cf(CF_PROVIDERS)?;
        let mut batch = WriteBatch::default();
        if provider.is_default {
            for current in self.providers()? {
                if current.id != provider.id && current.is_default {
                    let mut current = current;
                    current.is_default = false;
                    batch.put_cf(&providers, current.id.as_bytes(), serde_json::to_vec(&current)?);
                }
            }
        }
        batch.put_cf(&providers, provider.id.as_bytes(), serde_json::to_vec(provider)?);
        self.db.write(batch)?;
        if !self.providers()?.iter().any(|provider| provider.is_default) {
            if let Some(mut production) = self.provider("letsencrypt-production")? {
                production.is_default = true;
                self.put_json(CF_PROVIDERS, production.id.as_bytes(), &production)?;
            }
        }
        Ok(())
    }

    pub fn provider(&self, provider_id: &str) -> Result<Option<AcmeProvider>> {
        self.get_json(CF_PROVIDERS, provider_id.as_bytes())
    }

    pub fn providers(&self) -> Result<Vec<AcmeProvider>> {
        let cf = self.cf(CF_PROVIDERS)?;
        self.db
            .iterator_cf(&cf, rocksdb::IteratorMode::Start)
            .map(|entry| {
                let (_, value) = entry?;
                Ok(serde_json::from_slice(&value)?)
            })
            .collect()
    }

    pub fn delete_provider(&self, provider_id: &str) -> Result<()> {
        if matches!(provider_id, "letsencrypt-production" | "letsencrypt-staging") {
            bail!("built-in Let's Encrypt providers cannot be deleted");
        }
        if self.provider(provider_id)?.is_some_and(|provider| provider.is_default) {
            bail!("the default ACME provider cannot be deleted; set another provider as default first");
        }
        self.db
            .delete_cf(&self.cf(CF_PROVIDERS)?, provider_id.as_bytes())?;
        Ok(())
    }

    pub async fn providers_async(&self) -> Result<Vec<AcmeProvider>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.providers())
            .await
            .context("ACME provider database task failed")?
    }

    pub fn ensure_builtin_providers(&self) -> Result<()> {
        let has_default = self.providers()?.iter().any(|provider| provider.is_default);
        for provider in [
            AcmeProvider {
                id: "letsencrypt-production".into(),
                name: "Let's Encrypt Production".into(),
                directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
                staging: false,
                is_default: !has_default,
                ..Default::default()
            },
            AcmeProvider {
                id: "letsencrypt-staging".into(),
                name: "Let's Encrypt Staging".into(),
                directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
                staging: true,
                ..Default::default()
            },
        ] {
            if self.provider(&provider.id)?.is_none() {
                self.save_provider(&provider)?;
            }
        }
        if !has_default {
            let mut production = self.provider("letsencrypt-production")?
                .context("built-in Let's Encrypt production provider is missing")?;
            if !production.is_default {
                production.is_default = true;
                self.save_provider(&production)?;
            }
        }
        Ok(())
    }

    pub fn default_provider_id(&self) -> Result<String> {
        Ok(self.providers()?.into_iter().find(|provider| provider.is_default)
            .map(|provider| provider.id)
            .unwrap_or_else(|| "letsencrypt-production".into()))
    }

    /// Ensures every exact hostname that terminates client TLS has a managed
    /// certificate. Suffix and regex rules cannot be converted into concrete
    /// TLS-ALPN-01 certificate identifiers.
    pub fn ensure_automatic_certificates(&self, config: &RuntimeConfig) -> Result<usize> {
        use crate::runtime_config::{AdditionalListenerConfig, TlsRouteAction};
        let mut domains = std::collections::BTreeSet::new();
        let terminating = |action: &TlsRouteAction| matches!(action, TlsRouteAction::Terminate { .. } | TlsRouteAction::ReverseProxy { .. });
        for route in &config.default_listener.ordinary_traffic.routes {
            if terminating(&route.action) { domains.extend(route.matcher.exact.iter().cloned()); }
        }
        for listener in config.additional_listeners.values() {
            if let AdditionalListenerConfig::Tls(listener) = listener {
                for route in &listener.routing.routes {
                    if terminating(&route.action) { domains.extend(route.matcher.exact.iter().cloned()); }
                }
            }
        }
        let provider_id = self.default_provider_id()?;
        let mut created = 0;
        for domain in domains {
            let domain = normalize_domain(&domain)?;
            if self.certificate_for_domain(&domain)?.is_some() { continue; }
            self.save_managed_certificate(&ManagedCertificate {
                id: domain.clone(), domains: vec![domain], provider_id: provider_id.clone(), automatic: true,
                ..Default::default()
            })?;
            created += 1;
        }
        Ok(created)
    }

    /// Migrates legacy arbitrary IDs and multi-SAN records to one record per
    /// normalized domain, where the domain itself is the stable ID.
    pub fn migrate_certificates_to_single_domain_ids(&self) -> Result<usize> {
        let legacy = self.managed_certificates()?.into_iter()
            .filter(|certificate| certificate.domains.len() != 1 || normalize_domain(&certificate.domains[0]).ok().as_deref() != Some(certificate.id.as_str()))
            .collect::<Vec<_>>();
        if legacy.is_empty() { return Ok(0); }
        let existing_ids = self.managed_certificates()?.into_iter().map(|certificate| certificate.id).collect::<std::collections::HashSet<_>>();
        let certificates = self.cf(CF_CERTIFICATES)?;
        let domain_index = self.cf(CF_DOMAIN_INDEX)?;
        let generations = self.cf(CF_GENERATIONS)?;
        let active = self.cf(CF_ACTIVE_GENERATIONS)?;
        let renewals = self.cf(CF_RENEWALS)?;
        let diagnostics = self.cf(CF_DNS_DIAGNOSTICS)?;
        let tokens = self.cf(CF_TOKENS)?;
        let mut replacements = std::collections::HashMap::<String, Vec<String>>::new();
        let mut batch = WriteBatch::default();
        for certificate in &legacy {
            let domains = certificate.domains.iter().map(|domain| normalize_domain(domain)).collect::<Result<Vec<_>>>()?;
            for domain in &domains {
                if existing_ids.contains(domain) && domain != &certificate.id {
                    bail!("cannot migrate certificate `{}`: domain ID `{domain}` already exists", certificate.id);
                }
            }
            let active_generation_id = self.db.get_cf(&active, certificate.id.as_bytes())?;
            let renewal = self.renewal_state(&certificate.id)?;
            let generation_prefix = generation_key(&certificate.id, "");
            let mut old_generations = Vec::new();
            for item in self.db.prefix_iterator_cf(&generations, &generation_prefix) {
                let (key, value) = item?;
                if !key.starts_with(&generation_prefix) { break; }
                old_generations.push((key.to_vec(), serde_json::from_slice::<CertificateGeneration>(&value)?));
            }
            let diagnostic_prefix = format!("{}\0", certificate.id);
            let mut old_diagnostics = Vec::new();
            for item in self.db.prefix_iterator_cf(&diagnostics, diagnostic_prefix.as_bytes()) {
                let (key, value) = item?;
                if !key.starts_with(diagnostic_prefix.as_bytes()) { break; }
                old_diagnostics.push((key.to_vec(), serde_json::from_slice::<DnsDiagnostic>(&value)?));
            }
            batch.delete_cf(&certificates, certificate.id.as_bytes());
            batch.delete_cf(&active, certificate.id.as_bytes());
            batch.delete_cf(&renewals, certificate.id.as_bytes());
            for (key, _) in &old_generations { batch.delete_cf(&generations, key); }
            for (key, _) in &old_diagnostics { batch.delete_cf(&diagnostics, key); }
            for domain in &domains {
                let mut split = certificate.clone();
                split.id = domain.clone();
                split.domains = vec![domain.clone()];
                batch.put_cf(&certificates, domain.as_bytes(), serde_json::to_vec(&split)?);
                batch.put_cf(&domain_index, domain.as_bytes(), domain.as_bytes());
                if let Some(generation_id) = &active_generation_id { batch.put_cf(&active, domain.as_bytes(), generation_id); }
                if let Some(mut renewal) = renewal.clone() {
                    renewal.certificate_id = domain.clone();
                    batch.put_cf(&renewals, domain.as_bytes(), serde_json::to_vec(&renewal)?);
                }
                for (_, generation) in &old_generations {
                    let mut generation = generation.clone();
                    generation.certificate_id = domain.clone();
                    batch.put_cf(&generations, generation_key(domain, &generation.id), serde_json::to_vec(&generation)?);
                }
                for (_, diagnostic) in old_diagnostics.iter().filter(|(_, diagnostic)| normalize_domain(&diagnostic.domain).ok().as_deref() == Some(domain.as_str())) {
                    let mut diagnostic = diagnostic.clone();
                    diagnostic.certificate_id = domain.clone();
                    let key = format!("{domain}\0{}\0{}", diagnostic.domain, diagnostic.resolver);
                    batch.put_cf(&diagnostics, key.as_bytes(), serde_json::to_vec(&diagnostic)?);
                }
            }
            replacements.insert(certificate.id.clone(), domains);
        }
        for item in self.db.iterator_cf(&tokens, rocksdb::IteratorMode::Start) {
            let (key, value) = item?;
            let mut token: RetrievalToken = serde_json::from_slice(&value)?;
            let mut changed = false;
            for (old, domains) in &replacements {
                if token.certificate_ids.remove(old) {
                    token.certificate_ids.extend(domains.iter().cloned());
                    changed = true;
                }
            }
            if changed { batch.put_cf(&tokens, key, serde_json::to_vec(&token)?); }
        }
        self.db.write(batch)?;
        Ok(legacy.len())
    }

    pub async fn provider_async(&self, provider_id: String) -> Result<Option<AcmeProvider>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.provider(&provider_id))
            .await
            .context("ACME provider database task failed")?
    }

    pub fn save_account_credentials(
        &self,
        provider_id: &str,
        credentials: serde_json::Value,
    ) -> Result<()> {
        if provider_id.trim().is_empty() || credentials.is_null() {
            bail!("ACME provider ID and account credentials are required");
        }
        self.put_json(
            CF_ACCOUNTS,
            provider_id.as_bytes(),
            &StoredAcmeAccount {
                provider_id: provider_id.to_owned(),
                credentials,
                updated_at: Some(OffsetDateTime::now_utc()),
            },
        )
    }

    pub fn account_credentials(&self, provider_id: &str) -> Result<Option<StoredAcmeAccount>> {
        self.get_json(CF_ACCOUNTS, provider_id.as_bytes())
    }

    pub async fn account_credentials_async(
        &self,
        provider_id: String,
    ) -> Result<Option<StoredAcmeAccount>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.account_credentials(&provider_id))
            .await
            .context("ACME account database task failed")?
    }

    pub fn put_json<T: Serialize>(&self, cf: &str, key: &[u8], value: &T) -> Result<()> {
        let cf = self.cf(cf)?;
        self.db.put_cf(&cf, key, serde_json::to_vec(value)?)?;
        Ok(())
    }

    pub fn get_json<T: DeserializeOwned>(&self, cf: &str, key: &[u8]) -> Result<Option<T>> {
        let cf = self.cf(cf)?;
        self.db
            .get_cf(&cf, key)?
            .map(|value| serde_json::from_slice(&value).map_err(anyhow::Error::from))
            .transpose()
    }

    pub fn save_managed_certificate(&self, certificate: &ManagedCertificate) -> Result<()> {
        let _gate = certificate_gate().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.save_managed_certificate_locked(certificate)
    }

    /// Creates an automatic single-domain certificate unless the domain is
    /// already managed, atomically with respect to concurrent certificate
    /// saves, so a runtime auto-registration can never overwrite an
    /// administrator's explicit configuration for the same domain.
    pub fn create_automatic_certificate_if_absent(&self, domain: &str) -> Result<bool> {
        let domain = normalize_domain(domain)?;
        let _gate = certificate_gate().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.certificate_for_domain(&domain)?.is_some() {
            return Ok(false);
        }
        let provider_id = self.default_provider_id()?;
        self.save_managed_certificate_locked(&ManagedCertificate {
            id: domain.clone(),
            domains: vec![domain],
            provider_id,
            automatic: true,
            ..Default::default()
        })?;
        Ok(true)
    }

    fn save_managed_certificate_locked(&self, certificate: &ManagedCertificate) -> Result<()> {
        if certificate.domains.len() != 1 {
            bail!("managed certificate must contain exactly one domain");
        }
        let certificates = self.cf(CF_CERTIFICATES)?;
        let domain_index = self.cf(CF_DOMAIN_INDEX)?;
        let existing: Option<ManagedCertificate> = self.get_json(
            CF_CERTIFICATES,
            certificate.id.as_bytes(),
        )?;
        let normalized: Vec<String> = certificate
            .domains
            .iter()
            .map(|domain| normalize_domain(domain))
            .collect::<Result<_>>()?;
        if certificate.id != normalized[0] {
            bail!("managed certificate ID must equal its normalized domain `{}`", normalized[0]);
        }
        if normalized.iter().any(|domain| domain.contains('*')) {
            bail!("wildcard managed certificates are not supported by TLS-ALPN-01");
        }
        if normalized.iter().collect::<std::collections::BTreeSet<_>>().len() != normalized.len() {
            bail!("managed certificate contains duplicate domains");
        }
        for domain in &normalized {
            if let Some(owner) = self.db.get_cf(&domain_index, domain.as_bytes())? {
                if owner.as_slice() != certificate.id.as_bytes() {
                    bail!("domain `{domain}` is already managed by another certificate");
                }
            }
        }

        let mut value = certificate.clone();
        value.domains = normalized.clone();
        let now = OffsetDateTime::now_utc();
        value.created_at = existing
            .as_ref()
            .and_then(|current| current.created_at)
            .or(Some(now));
        value.updated_at = Some(now);

        let mut batch = WriteBatch::default();
        if let Some(existing) = existing {
            for old_domain in existing.domains {
                let old_domain = normalize_domain(&old_domain)?;
                if !normalized.contains(&old_domain) {
                    batch.delete_cf(&domain_index, old_domain.as_bytes());
                }
            }
        }
        for domain in normalized {
            batch.put_cf(&domain_index, domain.as_bytes(), certificate.id.as_bytes());
        }
        batch.put_cf(
            &certificates,
            certificate.id.as_bytes(),
            serde_json::to_vec(&value)?,
        );
        self.db.write(batch)?;
        Ok(())
    }

    pub fn managed_certificate(&self, certificate_id: &str) -> Result<Option<ManagedCertificate>> {
        self.get_json(CF_CERTIFICATES, certificate_id.as_bytes())
    }

    pub async fn managed_certificate_async(
        &self,
        certificate_id: String,
    ) -> Result<Option<ManagedCertificate>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.managed_certificate(&certificate_id))
            .await
            .context("managed certificate database task failed")?
    }

    pub fn managed_certificates(&self) -> Result<Vec<ManagedCertificate>> {
        let cf = self.cf(CF_CERTIFICATES)?;
        self.db
            .iterator_cf(&cf, rocksdb::IteratorMode::Start)
            .map(|entry| {
                let (_, value) = entry?;
                Ok(serde_json::from_slice(&value)?)
            })
            .collect()
    }

    pub async fn managed_certificates_async(&self) -> Result<Vec<ManagedCertificate>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.managed_certificates())
            .await
            .context("managed certificate database task failed")?
    }

    /// Deletes a managed certificate and all runtime state owned by it.
    pub fn delete_managed_certificate(&self, certificate_id: &str) -> Result<bool> {
        let Some(certificate) = self.managed_certificate(certificate_id)? else {
            return Ok(false);
        };
        let lets_encrypt = matches!(certificate.provider_id.as_str(), "letsencrypt-production" | "letsencrypt-staging");
        let issued_and_valid = self.active_generation(certificate_id)?
            .is_some_and(|generation| crate::managed_tls::validate_generation(&certificate, &generation).is_ok());
        if lets_encrypt && issued_and_valid {
            bail!("an issued and still-valid Let's Encrypt certificate cannot be deleted");
        }
        if certificate.automatic {
            let config = self.load_config()?.map(|stored| stored.config);
            let still_managed = config.as_ref().is_some_and(|config| certificate.domains.iter().any(|domain| {
                automatic_domain_is_in_use(config, domain)
            }));
            if still_managed {
                bail!("automatic certificate is managed by an active TLS route and cannot be deleted");
            }
        }
        let certificates = self.cf(CF_CERTIFICATES)?;
        let domain_index = self.cf(CF_DOMAIN_INDEX)?;
        let generations = self.cf(CF_GENERATIONS)?;
        let active = self.cf(CF_ACTIVE_GENERATIONS)?;
        let renewals = self.cf(CF_RENEWALS)?;
        let diagnostics = self.cf(CF_DNS_DIAGNOSTICS)?;
        let mut batch = WriteBatch::default();
        batch.delete_cf(&certificates, certificate_id.as_bytes());
        batch.delete_cf(&active, certificate_id.as_bytes());
        batch.delete_cf(&renewals, certificate_id.as_bytes());
        for domain in certificate.domains {
            let domain = normalize_domain(&domain)?;
            if self.db.get_cf(&domain_index, domain.as_bytes())?.as_deref()
                == Some(certificate_id.as_bytes())
            {
                batch.delete_cf(&domain_index, domain.as_bytes());
            }
        }
        let prefix = generation_key(certificate_id, "");
        for item in self.db.prefix_iterator_cf(&generations, &prefix) {
            let (key, _) = item?;
            if !key.starts_with(&prefix) { break; }
            batch.delete_cf(&generations, key);
        }
        let diagnostic_prefix = format!("{certificate_id}\0");
        for item in self.db.prefix_iterator_cf(&diagnostics, diagnostic_prefix.as_bytes()) {
            let (key, _) = item?;
            if !key.starts_with(diagnostic_prefix.as_bytes()) { break; }
            batch.delete_cf(&diagnostics, key);
        }
        self.db.write(batch)?;
        Ok(true)
    }

    /// Returns enabled certificates that are missing, within the renewal
    /// window, or already expired, while honoring a persisted retry gate.
    pub fn due_managed_certificates(
        &self,
        now: OffsetDateTime,
        renew_before_days: u16,
    ) -> Result<Vec<ManagedCertificate>> {
        let renewal_cutoff = now + time::Duration::days(i64::from(renew_before_days));
        let mut due = Vec::new();
        for certificate in self.managed_certificates()? {
            if !certificate.enabled {
                continue;
            }
            if self
                .renewal_state(&certificate.id)?
                .and_then(|state| state.next_attempt)
                .is_some_and(|next_attempt| next_attempt > now)
            {
                continue;
            }
            if self.renewal_state(&certificate.id)?
                .and_then(|state| state.ari_suggested_at)
                .is_some_and(|suggested| suggested <= now)
            {
                due.push(certificate);
                continue;
            }
            let generation = self.active_generation(&certificate.id)?;
            if generation
                .and_then(|generation| generation.not_after)
                .is_none_or(|not_after| not_after <= renewal_cutoff)
            {
                due.push(certificate);
            }
        }
        Ok(due)
    }

    pub async fn due_managed_certificates_async(
        &self,
        now: OffsetDateTime,
        renew_before_days: u16,
    ) -> Result<Vec<ManagedCertificate>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.due_managed_certificates(now, renew_before_days)
        })
        .await
        .context("renewal candidate database task failed")?
    }

    pub fn certificate_for_domain(&self, domain: &str) -> Result<Option<ManagedCertificate>> {
        let domain = normalize_domain(domain)?;
        let index = self.cf(CF_DOMAIN_INDEX)?;
        let Some(id) = self.db.get_cf(&index, domain.as_bytes())? else {
            return Ok(None);
        };
        self.get_json(CF_CERTIFICATES, &id)
    }

    pub async fn certificate_for_domain_async(
        &self,
        domain: String,
    ) -> Result<Option<ManagedCertificate>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.certificate_for_domain(&domain))
            .await
            .context("certificate database task failed")?
    }

    pub fn activate_generation(
        &self,
        generation: &CertificateGeneration,
        renewal: &RenewalState,
    ) -> Result<()> {
        if generation.id.is_empty() || generation.certificate_id.is_empty() {
            bail!("certificate generation and certificate IDs are required");
        }
        if generation.certificate_id != renewal.certificate_id {
            bail!("generation and renewal state refer to different certificates");
        }
        let generations = self.cf(CF_GENERATIONS)?;
        let active = self.cf(CF_ACTIVE_GENERATIONS)?;
        let renewals = self.cf(CF_RENEWALS)?;
        let generation_key = generation_key(&generation.certificate_id, &generation.id);
        let mut batch = WriteBatch::default();
        batch.put_cf(&generations, generation_key, serde_json::to_vec(generation)?);
        batch.put_cf(
            &active,
            generation.certificate_id.as_bytes(),
            generation.id.as_bytes(),
        );
        batch.put_cf(
            &renewals,
            generation.certificate_id.as_bytes(),
            serde_json::to_vec(renewal)?,
        );
        self.db.write(batch)?;
        Ok(())
    }

    pub fn renewal_state(&self, certificate_id: &str) -> Result<Option<RenewalState>> {
        self.get_json(CF_RENEWALS, certificate_id.as_bytes())
    }

    pub async fn renewal_state_async(
        &self,
        certificate_id: String,
    ) -> Result<Option<RenewalState>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.renewal_state(&certificate_id))
            .await
            .context("renewal state database task failed")?
    }

    pub fn save_renewal_state(&self, renewal: &RenewalState) -> Result<()> {
        if renewal.certificate_id.trim().is_empty() {
            bail!("renewal state certificate ID is required");
        }
        self.put_json(CF_RENEWALS, renewal.certificate_id.as_bytes(), renewal)
    }

    pub async fn save_renewal_state_async(&self, renewal: RenewalState) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_renewal_state(&renewal))
            .await
            .context("renewal state database task failed")?
    }

    pub fn clear_renewal_retry_gates(&self) -> Result<usize> {
        let cf = self.cf(CF_RENEWALS)?;
        let mut updates = Vec::new();
        for entry in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, value) = entry?;
            let mut state: RenewalState = serde_json::from_slice(&value)?;
            let ca_gate_active = state.ca_retry_after.is_some_and(|deadline| deadline > OffsetDateTime::now_utc());
            if !ca_gate_active && state.next_attempt.take().is_some() {
                state.ca_retry_after = None;
                updates.push((key.to_vec(), state));
            }
        }
        let count = updates.len();
        let mut batch = WriteBatch::default();
        for (key, state) in updates {
            batch.put_cf(&cf, key, serde_json::to_vec(&state)?);
        }
        self.db.write(batch)?;
        Ok(count)
    }

    pub async fn clear_renewal_retry_gates_async(&self) -> Result<usize> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.clear_renewal_retry_gates())
            .await
            .context("renewal retry database task failed")?
    }

    pub async fn activate_generation_async(
        &self,
        generation: CertificateGeneration,
        renewal: RenewalState,
    ) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.activate_generation(&generation, &renewal))
            .await
            .context("certificate activation database task failed")?
    }

    pub fn active_generation(
        &self,
        certificate_id: &str,
    ) -> Result<Option<CertificateGeneration>> {
        let active = self.cf(CF_ACTIVE_GENERATIONS)?;
        let Some(generation_id) = self.db.get_cf(&active, certificate_id.as_bytes())? else {
            return Ok(None);
        };
        self.get_json(
            CF_GENERATIONS,
            &generation_key(certificate_id, std::str::from_utf8(&generation_id)?),
        )
    }

    pub async fn active_generation_async(
        &self,
        certificate_id: String,
    ) -> Result<Option<CertificateGeneration>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.active_generation(&certificate_id))
            .await
            .context("certificate generation database task failed")?
    }
}

/// Serializes managed-certificate check-then-write sequences within this
/// process; RocksDB batches are atomic but not conditional.
fn certificate_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Mutex::new(()))
}

fn automatic_domain_is_in_use(config: &RuntimeConfig, domain: &str) -> bool {
    use crate::runtime_config::{AdditionalListenerConfig, TlsRouteAction};
    let terminating = |action: &TlsRouteAction| matches!(action, TlsRouteAction::Terminate { .. } | TlsRouteAction::ReverseProxy { .. });
    let normalized_hostname = normalize_domain(&config.control_plane.hostname);
    if normalize_domain(domain).is_ok_and(|domain| normalized_hostname.is_ok_and(|hostname| hostname == domain)) { return true; }
    if config.default_listener.ordinary_traffic.routes.iter().any(|route| route.matcher.matches(domain) && terminating(&route.action)) { return true; }
    config.additional_listeners.values().any(|listener| match listener {
        AdditionalListenerConfig::Tls(listener) => listener.routing.routes.iter().any(|route| route.matcher.matches(domain) && terminating(&route.action)),
        _ => false,
    })
}

#[derive(Debug, Default, Serialize)]
pub struct RetentionSummary {
    pub sessions: usize,
    pub generations: usize,
    pub audits: usize,
    pub config_revisions: usize,
}

pub fn normalize_domain(domain: &str) -> Result<String> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty()
        || domain.len() > 253
        || domain.contains('/')
        || domain.contains('\0')
        || domain.split('.').any(|label| label.is_empty() || label.len() > 63)
    {
        bail!("invalid DNS name `{domain}`");
    }
    Ok(domain)
}

fn generation_key(certificate_id: &str, generation_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(certificate_id.len() + generation_id.len() + 1);
    key.extend_from_slice(certificate_id.as_bytes());
    key.push(0);
    key.extend_from_slice(generation_id.as_bytes());
    key
}

fn audit_key(kind: &str, sequence: u64) -> Vec<u8> {
    format!(
        "{:020}-{:020}-{kind}",
        OffsetDateTime::now_utc().unix_timestamp_nanos(),
        sequence
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn config_round_trip_and_revision_are_atomic() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        assert!(!store.is_initialized().unwrap());

        let config = RuntimeConfig::default();
        let first = store.save_config(&config, "bootstrap").unwrap();
        let second = store.save_config(&config, "admin").unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        let loaded = store.load_config().unwrap().unwrap();
        assert_eq!(loaded.revision, 2);
        assert_eq!(loaded.updated_by, "admin");
        let history = store.config_history(10).unwrap();
        assert_eq!(history.iter().map(|value| value.revision).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn provider_and_opaque_account_credentials_round_trip() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let provider = AcmeProvider {
            id: "gts".into(),
            directory_url: "https://ca.example/directory".into(),
            eab_key_id: Some("key-id".into()),
            eab_hmac_key: Some("secret".into()),
            ..Default::default()
        };
        store.save_provider(&provider).unwrap();
        assert_eq!(store.provider("gts").unwrap().unwrap().directory_url, provider.directory_url);
        let credentials = serde_json::json!({"id":"account", "key_pkcs8":"private"});
        store
            .save_account_credentials("gts", credentials.clone())
            .unwrap();
        let stored = store.account_credentials("gts").unwrap().unwrap();
        assert_eq!(stored.provider_id, "gts");
        assert_eq!(stored.credentials, credentials);
        assert!(stored.updated_at.is_some());
    }

    #[test]
    fn builtin_provider_presets_do_not_overwrite_edits() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.ensure_builtin_providers().unwrap();
        assert_eq!(store.providers().unwrap().len(), 2);
        let mut provider = store.provider("letsencrypt-production").unwrap().unwrap();
        provider.contacts = vec!["mailto:admin@example".into()];
        provider.eab_key_id = Some("key-id".into());
        provider.eab_hmac_key = Some("secret".into());
        store.save_provider(&provider).unwrap();
        store.ensure_builtin_providers().unwrap();
        let provider = store.provider("letsencrypt-production").unwrap().unwrap();
        assert_eq!(provider.contacts, vec!["mailto:admin@example"]);
        assert_eq!(provider.eab_hmac_key.as_deref(), Some("secret"));
    }

    #[test]
    fn provider_default_is_unique_and_production_is_fallback() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.ensure_builtin_providers().unwrap();
        assert_eq!(store.default_provider_id().unwrap(), "letsencrypt-production");
        let mut custom = AcmeProvider { id: "custom".into(), name: "Custom".into(), directory_url: "https://acme.example/directory".into(), is_default: true, ..Default::default() };
        store.save_provider(&custom).unwrap();
        assert_eq!(store.default_provider_id().unwrap(), "custom");
        assert!(!store.provider("letsencrypt-production").unwrap().unwrap().is_default);
        custom.is_default = false;
        store.save_provider(&custom).unwrap();
        assert_eq!(store.default_provider_id().unwrap(), "letsencrypt-production");
        assert!(store.delete_provider("letsencrypt-staging").is_err());
    }

    #[test]
    fn exact_terminating_route_creates_protected_automatic_certificate() {
        use crate::runtime_config::{HostMatcher, TlsHostRoute, TlsRouteAction, UpstreamTransport};
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.ensure_builtin_providers().unwrap();
        let mut config = RuntimeConfig::default();
        config.default_listener.ordinary_traffic.routes.push(TlsHostRoute {
            name: "automatic".into(),
            matcher: HostMatcher { exact: vec!["Auto.Example".into()], ..Default::default() },
            action: TlsRouteAction::Terminate { target_port: 443, target: None, upstream: UpstreamTransport::Plaintext, load_balancing: Default::default() },
        });
        store.save_config(&config, "test").unwrap();
        assert_eq!(store.ensure_automatic_certificates(&config).unwrap(), 1);
        let certificate = store.certificate_for_domain("auto.example").unwrap().unwrap();
        assert!(certificate.automatic);
        assert_eq!(certificate.provider_id, "letsencrypt-production");
        assert!(store.delete_managed_certificate(&certificate.id).is_err());
    }

    #[test]
    fn due_selection_handles_missing_expiring_and_retry_gates() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let now = OffsetDateTime::now_utc();
        for id in ["missing", "expiring", "fresh", "delayed"] {
            let domain = format!("{id}.example");
            store
                .save_managed_certificate(&ManagedCertificate {
                    id: domain.clone(),
                    domains: vec![domain],
                    ..Default::default()
                })
                .unwrap();
        }
        for (id, not_after) in [
            ("expiring.example", now + time::Duration::days(10)),
            ("fresh.example", now + time::Duration::days(60)),
        ] {
            store
                .activate_generation(
                    &CertificateGeneration {
                        id: format!("{id}-generation"),
                        certificate_id: id.into(),
                        not_after: Some(not_after),
                        ..Default::default()
                    },
                    &RenewalState {
                        certificate_id: id.into(),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        store
            .save_renewal_state(&RenewalState {
                certificate_id: "delayed.example".into(),
                next_attempt: Some(now + time::Duration::hours(2)),
                ..Default::default()
            })
            .unwrap();
        let mut ids = store
            .due_managed_certificates(now, 15)
            .unwrap()
            .into_iter()
            .map(|certificate| certificate.id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec!["expiring.example", "missing.example"]);
    }

    #[test]
    fn deleting_managed_certificate_removes_owned_state_and_releases_domains() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let managed = ManagedCertificate {
            id: "one.example".into(),
            domains: vec!["one.example".into()],
            provider_id: "test".into(),
            ..Default::default()
        };
        store.save_managed_certificate(&managed).unwrap();
        let generation = CertificateGeneration {
            id: "generation-1".into(),
            certificate_id: managed.id.clone(),
            ..Default::default()
        };
        store.activate_generation(&generation, &RenewalState {
            certificate_id: managed.id.clone(),
            ..Default::default()
        }).unwrap();

        assert!(store.delete_managed_certificate(&managed.id).unwrap());
        assert!(store.managed_certificate(&managed.id).unwrap().is_none());
        assert!(store.active_generation(&managed.id).unwrap().is_none());
        assert!(store.renewal_state(&managed.id).unwrap().is_none());
        assert!(store.certificate_for_domain("one.example").unwrap().is_none());
        assert!(!store.delete_managed_certificate(&managed.id).unwrap());

        let replacement = ManagedCertificate { id: "one.example".into(), ..managed };
        store.save_managed_certificate(&replacement).unwrap();
    }

    #[test]
    fn manual_scan_preserves_active_ca_retry_deadline() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let now = OffsetDateTime::now_utc();
        let deadline = now + time::Duration::hours(12);
        store.save_renewal_state(&RenewalState {
            certificate_id: "rate-limited".into(),
            next_attempt: Some(deadline),
            ca_retry_after: Some(deadline),
            ..Default::default()
        }).unwrap();

        assert_eq!(store.clear_renewal_retry_gates().unwrap(), 0);
        let state = store.renewal_state("rate-limited").unwrap().unwrap();
        assert_eq!(state.next_attempt, Some(deadline));
        assert_eq!(state.ca_retry_after, Some(deadline));
    }

    #[test]
    fn domain_index_is_unique_and_normalized() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = ManagedCertificate {
            id: "home.example".into(),
            domains: vec!["home.example".into()],
            ..Default::default()
        };
        store.save_managed_certificate(&first).unwrap();
        assert_eq!(
            store
                .certificate_for_domain("HOME.EXAMPLE")
                .unwrap()
                .unwrap()
                .id,
            "home.example"
        );
        let conflicting = ManagedCertificate {
            id: "home.example".into(),
            domains: vec!["home.example".into()],
            ..Default::default()
        };
        assert!(store.save_managed_certificate(&conflicting).is_ok());
        assert_eq!(store.managed_certificates().unwrap().len(), 1);
    }

    #[test]
    fn legacy_multi_domain_certificate_migrates_to_domain_ids() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let legacy = ManagedCertificate { id: "legacy-id".into(), domains: vec!["one.example".into(), "two.example".into()], provider_id: "test".into(), ..Default::default() };
        store.put_json(CF_CERTIFICATES, legacy.id.as_bytes(), &legacy).unwrap();
        let index = store.cf(CF_DOMAIN_INDEX).unwrap();
        for domain in &legacy.domains { store.db.put_cf(&index, domain.as_bytes(), legacy.id.as_bytes()).unwrap(); }
        store.activate_generation(&CertificateGeneration { id: "generation-1".into(), certificate_id: legacy.id.clone(), ..Default::default() }, &RenewalState { certificate_id: legacy.id.clone(), ..Default::default() }).unwrap();
        let token = RetrievalToken { id: "reader".into(), token_hash: "a".repeat(64), certificate_ids: [legacy.id.clone()].into_iter().collect(), ..Default::default() };
        store.save_retrieval_token(&token).unwrap();

        assert_eq!(store.migrate_certificates_to_single_domain_ids().unwrap(), 1);
        assert!(store.managed_certificate("legacy-id").unwrap().is_none());
        for domain in &legacy.domains {
            let migrated = store.managed_certificate(domain).unwrap().unwrap();
            assert_eq!(migrated.domains, vec![domain.clone()]);
            assert_eq!(store.active_generation(domain).unwrap().unwrap().certificate_id, *domain);
        }
        let scopes = &store.retrieval_tokens().unwrap()[0].certificate_ids;
        assert_eq!(scopes, &legacy.domains.iter().cloned().collect());
        assert_eq!(store.migrate_certificates_to_single_domain_ids().unwrap(), 0);
    }

    #[test]
    fn health_check_history_is_persisted_and_pruned_to_24_hours() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let sample = |checked_at| HealthCheckSample {
            listener: "__default__".into(), host: "app.example".into(), path: "/".into(),
            backend: "backend.example:443".into(), endpoint: "192.0.2.1:443".into(),
            transport: "tls".into(), load_balancing: "round_robin".into(), online: true, checked_at,
        };
        store.save_health_check_samples(&[sample(OffsetDateTime::now_utc() - time::Duration::hours(25))]).unwrap();
        store.save_health_check_samples(&[sample(OffsetDateTime::now_utc())]).unwrap();
        let values = store.health_check_history().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].host, "app.example");
    }

    #[test]
    fn database_json_export_round_trips_all_non_health_column_families() {
        let source_dir = tempdir().unwrap();
        let source = Store::open(source_dir.path()).unwrap();
        source.save_config(&RuntimeConfig::default(), "backup").unwrap();
        source.db.put_cf(&source.cf(CF_ACCOUNTS).unwrap(), [0, 255, 1], [9, 0, 254]).unwrap();
        source.save_health_check_samples(&[HealthCheckSample {
            listener: "listener".into(), host: "host".into(), path: "/".into(), backend: "backend:80".into(), endpoint: "127.0.0.1:80".into(),
            transport: "plaintext".into(), load_balancing: "round_robin".into(), online: true, checked_at: OffsetDateTime::now_utc(),
        }]).unwrap();
        let export = source.export_json().unwrap();
        assert!(!export.column_families.contains_key(CF_HEALTH_CHECK_STATUS));

        let target_dir = tempdir().unwrap();
        let target = Store::open(target_dir.path()).unwrap();
        target.append_audit("remove_me", serde_json::json!({})).unwrap();
        let imported = target.import_json(&export, true, None).unwrap();
        assert!(imported > 0);
        assert_eq!(target.db.get_cf(&target.cf(CF_ACCOUNTS).unwrap(), [0, 255, 1]).unwrap().unwrap(), [9, 0, 254]);
        assert_eq!(target.load_config().unwrap().unwrap().updated_by, "backup");
        assert!(!target.audits(10).unwrap().iter().any(|value| value.get("kind").and_then(|kind| kind.as_str()) == Some("remove_me")));
    }

    #[test]
    fn scoped_import_touches_only_selected_column_families() {
        let source_dir = tempdir().unwrap();
        let source = Store::open(source_dir.path()).unwrap();
        source.save_config(&RuntimeConfig::default(), "backup").unwrap();
        source.db.put_cf(&source.cf(CF_ACCOUNTS).unwrap(), [1, 2, 3], [4, 5, 6]).unwrap();
        let export = source.export_json().unwrap();

        let target_dir = tempdir().unwrap();
        let target = Store::open(target_dir.path()).unwrap();
        // Import only the accounts family; config must stay untouched.
        let selected = std::collections::HashSet::from([CF_ACCOUNTS.to_string()]);
        let imported = target.import_json(&export, true, Some(&selected)).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(target.db.get_cf(&target.cf(CF_ACCOUNTS).unwrap(), [1, 2, 3]).unwrap().unwrap(), [4, 5, 6]);
        assert!(target.load_config().unwrap().is_none(), "config family was not selected, so nothing imported to it");

        // An empty selection is rejected; an unknown family is rejected.
        assert!(target.import_json(&export, false, Some(&std::collections::HashSet::new())).is_err());
        assert!(target.import_json(&export, false, Some(&std::collections::HashSet::from(["nope".to_string()]))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let runtime = directory.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _store = Store::open(&runtime).unwrap();
        assert_eq!(
            std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[tokio::test]
    async fn async_configuration_access_uses_same_store() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let config = RuntimeConfig::default();
        store
            .save_config_async(config, "admin".into())
            .await
            .unwrap();
        assert_eq!(
            store.load_config_async().await.unwrap().unwrap().revision,
            1
        );
    }

    #[test]
    fn retrieval_tokens_are_hash_addressed_and_retention_removes_expired_sessions() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let secret = "this bearer remains recoverable";
        let token = RetrievalToken { id: "deployment".into(), token_hash: crate::auth::token_hash(secret), bearer_token: secret.into(), expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)), ..Default::default() };
        store.save_retrieval_token(&token).unwrap();
        let stored = store.retrieval_token_by_hash(&crate::auth::token_hash(secret)).unwrap().unwrap();
        assert_eq!(stored.id, "deployment");
        assert_eq!(stored.bearer_token, secret);
        assert!(store.retrieval_token_by_hash(&crate::auth::token_hash("wrong")).unwrap().is_none());
        store.save_session(&SessionRecord { token_hash: crate::auth::token_hash("old"), username: "admin".into(), expires_at: Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)), ..Default::default() }).unwrap();
        let summary = store.cleanup_retention(OffsetDateTime::now_utc(), 3, 90).unwrap();
        assert_eq!(summary.sessions, 1);
        assert!(store.session(&crate::auth::token_hash("old")).unwrap().is_none());
    }
}
