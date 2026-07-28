use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options, WriteBatch};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use time::OffsetDateTime;

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

impl Store {
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
                id: "control-plane".into(),
                domains: vec![domain.clone()],
                provider_id: config.control_plane.provider_id.clone(),
                enabled: true,
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
        self.put_json(CF_PROVIDERS, provider.id.as_bytes(), provider)
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

    pub async fn providers_async(&self) -> Result<Vec<AcmeProvider>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.providers())
            .await
            .context("ACME provider database task failed")?
    }

    pub fn ensure_builtin_providers(&self) -> Result<()> {
        for provider in [
            AcmeProvider {
                id: "letsencrypt-production".into(),
                name: "Let's Encrypt Production".into(),
                directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
                staging: false,
                ..Default::default()
            },
            AcmeProvider {
                id: "letsencrypt-staging".into(),
                name: "Let's Encrypt Staging".into(),
                directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
                staging: true,
                ..Default::default()
            },
            AcmeProvider {
                id: "gts-production".into(),
                name: "Google Trust Services Production".into(),
                directory_url: "https://dv.acme-v02.api.pki.goog/directory".into(),
                staging: false,
                ..Default::default()
            },
            AcmeProvider {
                id: "gts-staging".into(),
                name: "Google Trust Services Staging".into(),
                directory_url: "https://dv.acme-v02.test-api.pki.goog/directory".into(),
                staging: true,
                ..Default::default()
            },
        ] {
            if self.provider(&provider.id)?.is_none() {
                self.save_provider(&provider)?;
            }
        }
        Ok(())
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
        if certificate.id.trim().is_empty() {
            bail!("managed certificate ID is required");
        }
        if certificate.domains.is_empty() {
            bail!("managed certificate must contain at least one domain");
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
            if state.next_attempt.take().is_some() {
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
        assert_eq!(store.providers().unwrap().len(), 4);
        let mut provider = store.provider("gts-production").unwrap().unwrap();
        provider.contacts = vec!["mailto:admin@example".into()];
        provider.eab_key_id = Some("key-id".into());
        provider.eab_hmac_key = Some("secret".into());
        store.save_provider(&provider).unwrap();
        store.ensure_builtin_providers().unwrap();
        let provider = store.provider("gts-production").unwrap().unwrap();
        assert_eq!(provider.contacts, vec!["mailto:admin@example"]);
        assert_eq!(provider.eab_hmac_key.as_deref(), Some("secret"));
    }

    #[test]
    fn due_selection_handles_missing_expiring_and_retry_gates() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let now = OffsetDateTime::now_utc();
        for id in ["missing", "expiring", "fresh", "delayed"] {
            store
                .save_managed_certificate(&ManagedCertificate {
                    id: id.into(),
                    domains: vec![format!("{id}.example")],
                    ..Default::default()
                })
                .unwrap();
        }
        for (id, not_after) in [
            ("expiring", now + time::Duration::days(10)),
            ("fresh", now + time::Duration::days(60)),
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
                certificate_id: "delayed".into(),
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
        assert_eq!(ids, vec!["expiring", "missing"]);
    }

    #[test]
    fn domain_index_is_unique_and_normalized() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = ManagedCertificate {
            id: "first".into(),
            domains: vec!["Home.Example.".into()],
            ..Default::default()
        };
        store.save_managed_certificate(&first).unwrap();
        assert_eq!(
            store
                .certificate_for_domain("HOME.EXAMPLE")
                .unwrap()
                .unwrap()
                .id,
            "first"
        );
        let conflicting = ManagedCertificate {
            id: "second".into(),
            domains: vec!["home.example".into()],
            ..Default::default()
        };
        assert!(store.save_managed_certificate(&conflicting).is_err());
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
        let secret = "this bearer is returned once";
        let token = RetrievalToken { id: "deployment".into(), token_hash: crate::auth::token_hash(secret), expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)), ..Default::default() };
        store.save_retrieval_token(&token).unwrap();
        assert_eq!(store.retrieval_token_by_hash(&crate::auth::token_hash(secret)).unwrap().unwrap().id, "deployment");
        assert!(store.retrieval_token_by_hash(&crate::auth::token_hash("wrong")).unwrap().is_none());
        store.save_session(&SessionRecord { token_hash: crate::auth::token_hash("old"), username: "admin".into(), expires_at: Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)), ..Default::default() }).unwrap();
        let summary = store.cleanup_retention(OffsetDateTime::now_utc(), 3, 90).unwrap();
        assert_eq!(summary.sessions, 1);
        assert!(store.session(&crate::auth::token_hash("old")).unwrap().is_none());
    }
}
