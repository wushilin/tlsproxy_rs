use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::{bail, Result};
use log::info;
use lru::LruCache;
use rcgen::{Issuer, KeyPair};
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;
use time::OffsetDateTime;

use crate::certificate;
use crate::config::LocalCaConfig;
use crate::controller::Controller;

pub const LEAF_VALIDITY_DAYS: u32 = 365;
pub const EVICT_WITHIN_HOURS: i64 = 72;
pub const EVICTION_INTERVAL: StdDuration = StdDuration::from_secs(60 * 60);
const CACHE_CAPACITY: usize = 10_000;

#[derive(Clone)]
pub struct LocalCa {
    inner: Arc<LocalCaInner>,
}

struct LocalCaInner {
    ca_cert: PathBuf,
    ca_key: PathBuf,
    working_dir: PathBuf,
    issuer: Mutex<Issuer<'static, KeyPair>>,
    /// CA certificate(s) appended to every served chain so clients see the
    /// full chain instead of a bare leaf.
    ca_chain: Vec<CertificateDer<'static>>,
    cache: Mutex<LruCache<String, CachedIdentity>>,
}

#[derive(Clone)]
struct CachedIdentity {
    key: Arc<CertifiedKey>,
    expires_at: OffsetDateTime,
}

impl LocalCa {
    pub fn new(runtime_dir: &str, config: &LocalCaConfig) -> Result<Self> {
        let runtime_dir = Path::new(runtime_dir);
        let working_dir = runtime_dir.join(force_relative(&config.working_dir));
        let ca_cert = runtime_dir.join(force_relative(&config.ca_cert));
        let ca_key = runtime_dir.join(force_relative(&config.ca_key));
        let issuer = certificate::load_or_create_ca(&ca_cert, &ca_key)?;
        let ca_chain = certificate::read_certificates(&ca_cert)?;
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
                ca_chain,
                cache: Mutex::new(LruCache::new(
                    NonZeroUsize::new(CACHE_CAPACITY).expect("cache capacity is non-zero"),
                )),
            }),
        })
    }

    pub fn resolve_or_mint(&self, hostname: &str) -> Result<Arc<CertifiedKey>> {
        let hostname = sanitize_hostname(hostname)?;
        self.resolve_or_mint_inner(&hostname, std::slice::from_ref(&hostname))
    }

    /// Returns the cached leaf for `hostname` without minting, so callers can
    /// distinguish serving an existing certificate from issuing a new one.
    pub fn resolve_cached(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
        let hostname = sanitize_hostname(hostname).ok()?;
        self.get_cached(&hostname)
    }

    pub fn evict_expiring(&self) {
        let now = OffsetDateTime::now_utc();
        let mut cache = self.inner.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

    pub fn spawn_eviction_job(&self, controller: &mut Controller) {
        let ca = self.clone();
        drop(controller.spawn(async move {
            ca.evict_expiring();
            loop {
                tokio::time::sleep(EVICTION_INTERVAL).await;
                ca.evict_expiring();
            }
        }));
    }

    fn get_cached(&self, key: &str) -> Option<Arc<CertifiedKey>> {
        let mut cache = self.inner.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    ) -> Result<Arc<CertifiedKey>> {
        if let Some(key) = self.get_cached(cache_key) {
            return Ok(key);
        }
        info!(
            "certificate cache miss for ad-hoc identity `{cache_key}`; minting new leaf certificate"
        );
        let minted = {
            let issuer = self.inner.issuer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            certificate::mint_leaf(
                &issuer,
                &self.inner.ca_chain,
                san,
                "TLS Proxy Local Leaf",
                LEAF_VALIDITY_DAYS,
            )?
        };
        let key = Arc::new(minted.certified_key);
        let mut cache = self.inner.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put(
            cache_key.to_string(),
            CachedIdentity {
                key: Arc::clone(&key),
                expires_at: minted.expires_at,
            },
        );
        info!(
            "cached ad-hoc certificate `{cache_key}` in memory (expires at {})",
            minted.expires_at
        );
        Ok(key)
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
    use tempfile::tempdir;

    #[test]
    fn names_are_sanitized_for_files() {
        assert_eq!(sanitize_name("listener-1443"), "listener-1443");
        assert_eq!(sanitize_name("my listener/\u{2116}1"), "my_listener__1");
    }

    #[test]
    fn mints_ad_hoc_cert_without_persisting_leaf() {
        let runtime = tempdir().unwrap();
        let ca = LocalCa::new(runtime.path().to_str().unwrap(), &LocalCaConfig::default()).unwrap();
        ca.resolve_or_mint("Example.TEST").unwrap();
        assert!(runtime.path().join("local_ca/CA.pem").exists());
        assert!(!runtime
            .path()
            .join("local_ca/example.test-cert.pem")
            .exists());
    }

    #[test]
    fn minted_chain_includes_ca_certificate() {
        let runtime = tempdir().unwrap();
        let ca = LocalCa::new(runtime.path().to_str().unwrap(), &LocalCaConfig::default()).unwrap();
        let key = ca.resolve_or_mint("example.test").unwrap();
        let ca_der = certificate::read_certificates(&runtime.path().join("local_ca/CA.pem")).unwrap();
        assert_eq!(key.cert.len(), 1 + ca_der.len());
        assert_eq!(&key.cert[1..], ca_der.as_slice());
    }
}
