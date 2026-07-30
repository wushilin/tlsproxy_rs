//! RocksDB-backed runtime store: database lifecycle, configuration
//! revisions, retention, JSON import/export, and health-check samples.
//! Domain records live in the submodules: [`access`] (users, sessions,
//! tokens, audit) and [`acme`] (providers, certificates, renewals).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rocksdb::{BlockBasedOptions, ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options, WriteBatch};
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

const ROCKSDB_DB_WRITE_BUFFER_SIZE: usize = 1024 * 1024;
const ROCKSDB_CF_WRITE_BUFFER_SIZE: usize = 16 * 1024;
const ROCKSDB_MAX_WRITE_BUFFERS: i32 = 2;
const ROCKSDB_MAX_TOTAL_WAL_SIZE: u64 = 4 * 1024 * 1024;

type RocksDb = DBWithThreadMode<MultiThreaded>;

fn rocksdb_column_family_options() -> Options {
    let mut options = Options::default();
    options.set_compression_type(rocksdb::DBCompressionType::Lz4);
    options.set_write_buffer_size(ROCKSDB_CF_WRITE_BUFFER_SIZE);
    options.set_max_write_buffer_number(ROCKSDB_MAX_WRITE_BUFFERS);
    let mut table_options = BlockBasedOptions::default();
    table_options.disable_cache();
    options.set_block_based_table_factory(&table_options);
    options
}

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

mod access;
mod acme;

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
        db_options.set_db_write_buffer_size(ROCKSDB_DB_WRITE_BUFFER_SIZE);
        db_options.set_max_total_wal_size(ROCKSDB_MAX_TOTAL_WAL_SIZE);
        let descriptors = std::iter::once(ColumnFamilyDescriptor::new("default", rocksdb_column_family_options()))
            .chain(ALL_CFS.iter().map(|name| {
                ColumnFamilyDescriptor::new(*name, rocksdb_column_family_options())
            }));
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

    fn put_json<T: Serialize>(&self, cf: &str, key: &[u8], value: &T) -> Result<()> {
        let cf = self.cf(cf)?;
        self.db.put_cf(&cf, key, serde_json::to_vec(value)?)?;
        Ok(())
    }

    fn get_json<T: DeserializeOwned>(&self, cf: &str, key: &[u8]) -> Result<Option<T>> {
        let cf = self.cf(cf)?;
        self.db
            .get_cf(&cf, key)?
            .map(|value| serde_json::from_slice(&value).map_err(anyhow::Error::from))
            .transpose()
    }

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
    fn disabling_acme_stops_automatic_certificate_creation() {
        use crate::runtime_config::{HostMatcher, TlsHostRoute, TlsRouteAction, UpstreamTransport};
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.ensure_builtin_providers().unwrap();
        let mut config = RuntimeConfig::default();
        config.acme.enabled = false;
        config.default_listener.ordinary_traffic.routes.push(TlsHostRoute {
            name: "automatic".into(),
            matcher: HostMatcher { exact: vec!["auto.example".into()], ..Default::default() },
            action: TlsRouteAction::Terminate { target_port: 443, target: None, upstream: UpstreamTransport::Plaintext, load_balancing: Default::default() },
        });
        assert_eq!(store.ensure_automatic_certificates(&config).unwrap(), 0);
        assert!(store.certificate_for_domain("auto.example").unwrap().is_none());
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

        // Never-issued (pending) certificates are always deletable — a pending
        // entry may simply be a typo — even while a route still matches.
        assert!(store.delete_managed_certificate(&certificate.id).unwrap());
        assert_eq!(store.ensure_automatic_certificates(&config).unwrap(), 1);
        let certificate = store.certificate_for_domain("auto.example").unwrap().unwrap();

        // Once issued and still valid, the certificate becomes protected.
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["auto.example".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        store
            .activate_generation(
                &CertificateGeneration {
                    id: "auto-generation".into(),
                    certificate_id: certificate.id.clone(),
                    certificate_pem: leaf.pem(),
                    private_key_pem: key.serialize_pem(),
                    not_after: Some(OffsetDateTime::now_utc() + time::Duration::days(30)),
                    ..Default::default()
                },
                &RenewalState { certificate_id: certificate.id.clone(), ..Default::default() },
            )
            .unwrap();
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

    #[test]
    fn retention_keeps_active_and_only_two_newest_inactive_generations() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let now = OffsetDateTime::now_utc();
        for number in 1..=4 {
            store.activate_generation(
                &CertificateGeneration {
                    id: format!("generation-{number}"),
                    certificate_id: "example.com".into(),
                    issued_at: Some(now + time::Duration::minutes(number)),
                    ..Default::default()
                },
                &RenewalState {
                    certificate_id: "example.com".into(),
                    ..Default::default()
                },
            ).unwrap();
        }

        let summary = store.cleanup_retention(now, 3, 90).unwrap();
        assert_eq!(summary.generations, 1);
        assert_eq!(
            store.active_generation("example.com").unwrap().unwrap().id,
            "generation-4"
        );

        let generations = store.cf(CF_GENERATIONS).unwrap();
        let retained = store.db.iterator_cf(&generations, rocksdb::IteratorMode::Start)
            .map(|item| {
                let (_, value) = item.unwrap();
                serde_json::from_slice::<CertificateGeneration>(&value).unwrap().id
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            retained,
            ["generation-2", "generation-3", "generation-4"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }
}
