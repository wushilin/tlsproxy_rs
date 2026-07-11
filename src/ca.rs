use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::{bail, Result};
use log::{info, warn};
use lru::LruCache;
use rcgen::{Issuer, KeyPair};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use time::OffsetDateTime;

use crate::certificate::{self, CertificatePaths};
use crate::config::{Config, LocalCaConfig};

pub const LEAF_VALIDITY_DAYS: u32 = 365;
pub const EVICT_WITHIN_HOURS: i64 = 72;
pub const EVICTION_INTERVAL: StdDuration = StdDuration::from_secs(60 * 60);
const CACHE_CAPACITY: usize = 10_000;
const ADMIN_CACHE_KEY: &str = "__admin__";

#[derive(Clone)]
pub struct LocalCa {
    inner: Arc<LocalCaInner>,
}

struct LocalCaInner {
    ca_cert: PathBuf,
    ca_key: PathBuf,
    working_dir: PathBuf,
    issuer: Mutex<Issuer<'static, KeyPair>>,
    cache: Mutex<LruCache<String, CachedIdentity>>,
}

#[derive(Clone)]
struct CachedIdentity {
    key: Arc<CertifiedKey>,
    expires_at: OffsetDateTime,
}

impl LocalCa {
    pub fn from_config(config: &Config) -> Result<Self> {
        let localca = config
            .ca
            .as_ref()
            .and_then(|ca| ca.localca.clone())
            .unwrap_or_default();
        Self::new(&config.options.runtime_dir, &localca)
    }

    pub fn new(runtime_dir: &str, config: &LocalCaConfig) -> Result<Self> {
        let runtime_dir = Path::new(runtime_dir);
        let working_dir = runtime_dir.join(force_relative(&config.working_dir));
        let ca_cert = runtime_dir.join(force_relative(&config.ca_cert));
        let ca_key = runtime_dir.join(force_relative(&config.ca_key));
        let issuer = certificate::load_or_create_ca(&ca_cert, &ca_key)?;
        info!(
            "initialized local CA manager: ca_cert=`{}`, ca_key=`{}`, working_dir=`{}`, cache_capacity={CACHE_CAPACITY}",
            ca_cert.display(),
            ca_key.display(),
            working_dir.display()
        );
        Ok(Self {
            inner: Arc::new(LocalCaInner {
                ca_cert,
                ca_key,
                working_dir,
                issuer: Mutex::new(issuer),
                cache: Mutex::new(LruCache::new(
                    NonZeroUsize::new(CACHE_CAPACITY).expect("cache capacity is non-zero"),
                )),
            }),
        })
    }

    pub fn ca_cert_path(&self) -> &Path {
        &self.inner.ca_cert
    }

    pub fn resolve_or_mint(&self, hostname: &str) -> Result<Arc<CertifiedKey>> {
        let hostname = sanitize_hostname(hostname)?;
        self.resolve_or_mint_inner(&hostname, std::slice::from_ref(&hostname), false)
    }

    pub fn resolve_admin(&self, san: &[String]) -> Result<Arc<CertifiedKey>> {
        let san = if san.is_empty() {
            vec!["localhost".into(), "127.0.0.1".into()]
        } else {
            san.to_vec()
        };
        if let Some(key) = self.get_cached(ADMIN_CACHE_KEY) {
            return Ok(key);
        }
        if let Some(key) = self.load_admin_from_disk(&san)? {
            return Ok(key);
        }
        self.resolve_or_mint_inner(ADMIN_CACHE_KEY, &san, true)
    }

    pub fn evict_expiring(&self) {
        let now = OffsetDateTime::now_utc();
        let mut cache = self.inner.cache.lock().expect("CA cache lock poisoned");
        let expired_keys: Vec<String> = cache
            .iter()
            .filter(|(_, identity)| {
                identity.expires_at - now <= time::Duration::hours(EVICT_WITHIN_HOURS)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired_keys {
            if let Some(identity) = cache.pop(&key) {
                info!(
                    "evicted cached certificate `{key}` near expiry (expires at {})",
                    identity.expires_at
                );
            }
        }
    }

    pub fn spawn_eviction_job(&self) {
        let ca = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(EVICTION_INTERVAL).await;
                ca.evict_expiring();
            }
        });
    }

    pub fn server_config_with_resolver(
        &self,
        resolver: Arc<dyn ResolvesServerCert>,
    ) -> Arc<rustls::ServerConfig> {
        Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(resolver),
        )
    }

    pub fn admin_server_config(&self, san: Vec<String>) -> Arc<rustls::ServerConfig> {
        self.server_config_with_resolver(Arc::new(AdminCertResolver {
            ca: self.clone(),
            san,
        }))
    }

    fn get_cached(&self, key: &str) -> Option<Arc<CertifiedKey>> {
        let mut cache = self.inner.cache.lock().expect("CA cache lock poisoned");
        let identity = cache.get(key)?;
        if identity.expires_at - OffsetDateTime::now_utc()
            <= time::Duration::hours(EVICT_WITHIN_HOURS)
        {
            let expires_at = identity.expires_at;
            cache.pop(key);
            info!(
                "evicted cached certificate `{key}` during lookup because it expires at {expires_at}"
            );
            return None;
        }
        Some(Arc::clone(&identity.key))
    }

    fn resolve_or_mint_inner(
        &self,
        cache_key: &str,
        san: &[String],
        persist_admin: bool,
    ) -> Result<Arc<CertifiedKey>> {
        if let Some(key) = self.get_cached(cache_key) {
            return Ok(key);
        }
        let kind = if persist_admin { "admin" } else { "ad-hoc" };
        info!(
            "certificate cache miss for {kind} identity `{cache_key}`; minting new leaf certificate"
        );
        let minted = {
            let issuer = self.inner.issuer.lock().expect("CA issuer lock poisoned");
            certificate::mint_leaf(&issuer, san, "TLS Proxy Local Leaf", LEAF_VALIDITY_DAYS)?
        };
        if persist_admin {
            let paths = self.admin_paths();
            certificate::write_identity(&paths, &minted.cert_pem, &minted.key_pem)?;
            info!(
                "persisted renewed admin certificate `{}` and key `{}` (expires at {})",
                paths.cert.display(),
                paths.key.display(),
                minted.expires_at
            );
        }
        let key = Arc::new(minted.certified_key);
        let mut cache = self.inner.cache.lock().expect("CA cache lock poisoned");
        cache.put(
            cache_key.to_string(),
            CachedIdentity {
                key: Arc::clone(&key),
                expires_at: minted.expires_at,
            },
        );
        info!(
            "cached {kind} certificate `{cache_key}` in memory (expires at {})",
            minted.expires_at
        );
        Ok(key)
    }

    fn load_admin_from_disk(&self, san: &[String]) -> Result<Option<Arc<CertifiedKey>>> {
        let paths = self.admin_paths();
        match (paths.cert.exists(), paths.key.exists()) {
            (false, false) => {
                info!(
                    "no persisted admin certificate found at `{}` / `{}`",
                    paths.cert.display(),
                    paths.key.display()
                );
                return Ok(None);
            }
            (true, true) => {}
            _ => bail!(
                "admin certificate is incomplete: both `{}` and `{}` must exist or both must be absent",
                paths.cert.display(),
                paths.key.display()
            ),
        }
        let sans = certificate::parse_sans(san)?;
        let threshold = StdDuration::from_secs(EVICT_WITHIN_HOURS as u64 * 60 * 60);
        if !certificate::leaf_is_reusable_with_threshold(
            &paths.cert,
            &paths.key,
            &self.inner.ca_cert,
            &sans,
            threshold,
        ) {
            warn!(
                "persisted admin certificate `{}` is invalid, stale, SAN-mismatched, CA-mismatched, or within {EVICT_WITHIN_HOURS}h of expiry; removing stale admin identity",
                paths.cert.display()
            );
            remove_stale_admin_identity(&paths);
            return Ok(None);
        }
        let expires_at = certificate::certificate_file_expires_at(&paths.cert)?;
        let certified_key = certificate::load_certified_key(&paths.cert, &paths.key)?;
        let key = Arc::new(certified_key);
        let mut cache = self.inner.cache.lock().expect("CA cache lock poisoned");
        cache.put(
            ADMIN_CACHE_KEY.to_string(),
            CachedIdentity {
                key: Arc::clone(&key),
                expires_at,
            },
        );
        info!(
            "loaded persisted admin certificate `{}` into memory cache (expires at {})",
            paths.cert.display(),
            expires_at
        );
        Ok(Some(key))
    }

    fn admin_paths(&self) -> CertificatePaths {
        CertificatePaths {
            cert: self.inner.working_dir.join("admin-cert.pem"),
            key: self.inner.working_dir.join("admin-key.pem"),
        }
    }
}

#[derive(Debug)]
pub struct AdminCertResolver {
    ca: LocalCa,
    san: Vec<String>,
}

impl ResolvesServerCert for AdminCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        match self.ca.resolve_admin(&self.san) {
            Ok(key) => Some(key),
            Err(cause) => {
                warn!("failed to resolve admin certificate: {cause}");
                None
            }
        }
    }
}

fn force_relative(path: &str) -> PathBuf {
    let path = Path::new(path);
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect()
}

pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn sanitize_hostname(hostname: &str) -> Result<String> {
    let hostname = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty() {
        bail!("empty SNI hostname");
    }
    if hostname.len() > 253 {
        bail!("SNI hostname is too long");
    }
    if hostname
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '.')))
    {
        bail!("invalid SNI hostname `{hostname}`");
    }
    Ok(hostname)
}

fn remove_stale_admin_identity(paths: &CertificatePaths) {
    remove_stale_file("admin certificate", &paths.cert);
    remove_stale_file("admin private key", &paths.key);
}

fn remove_stale_file(label: &str, path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => info!("removed stale {label} `{}`", path.display()),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
            info!("stale {label} `{}` was already absent", path.display());
        }
        Err(cause) => warn!(
            "failed to remove stale {label} `{}`: {cause}",
            path.display()
        ),
    }
}

impl std::fmt::Debug for LocalCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCa")
            .field("ca_cert", &self.inner.ca_cert)
            .field("ca_key", &self.inner.ca_key)
            .field("working_dir", &self.inner.working_dir)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CaConfig, Options};
    use std::fs;
    use tempfile::tempdir;

    fn config(runtime_dir: String) -> Config {
        Config {
            options: Options {
                runtime_dir,
                ..Default::default()
            },
            ca: Some(CaConfig {
                localca: Some(LocalCaConfig::default()),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn absent_ca_section_defaults_to_local() {
        let runtime = tempdir().unwrap();
        let mut config = config(runtime.path().to_string_lossy().into_owned());
        config.ca = None;
        let ca = LocalCa::from_config(&config).unwrap();
        assert!(ca.ca_cert_path().ends_with("local_ca/CA.pem"));
    }

    #[test]
    fn names_are_sanitized_for_files() {
        assert_eq!(sanitize_name("listener-1443"), "listener-1443");
        assert_eq!(sanitize_name("my listener/№1"), "my_listener__1");
    }

    #[test]
    fn mints_ad_hoc_cert_without_persisting_leaf() {
        let runtime = tempdir().unwrap();
        let ca =
            LocalCa::from_config(&config(runtime.path().to_string_lossy().into_owned())).unwrap();
        ca.resolve_or_mint("Example.TEST").unwrap();
        assert!(runtime.path().join("local_ca/CA.pem").exists());
        assert!(!runtime
            .path()
            .join("local_ca/example.test-cert.pem")
            .exists());
    }

    #[test]
    fn admin_cert_is_persisted_and_reused() {
        let runtime = tempdir().unwrap();
        let ca =
            LocalCa::from_config(&config(runtime.path().to_string_lossy().into_owned())).unwrap();
        ca.resolve_admin(&["localhost".into()]).unwrap();
        let cert_path = runtime.path().join("local_ca/admin-cert.pem");
        assert!(cert_path.exists());
        let first = fs::read(&cert_path).unwrap();

        let ca =
            LocalCa::from_config(&config(runtime.path().to_string_lossy().into_owned())).unwrap();
        ca.resolve_admin(&["localhost".into()]).unwrap();
        assert_eq!(first, fs::read(&cert_path).unwrap());
    }
}
