use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use log::info;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use time::OffsetDateTime;
use tokio::sync::RwLock;
use x509_parser::extensions::GeneralName;

use crate::acme_types::{CertificateGeneration, ManagedCertificate};
use crate::runtime_config::CertificateFallbackPolicy;
use crate::store::{normalize_domain, Store};

#[derive(Clone, Default)]
pub struct ManagedCertificateCache {
    inner: Arc<RwLock<Arc<HashMap<String, CachedCertificate>>>>,
}

#[derive(Clone)]
struct CachedCertificate {
    certificate_id: String,
    generation_id: String,
    key: Arc<CertifiedKey>,
}

impl ManagedCertificateCache {
    /// Atomically activates a persisted generation and makes it available to
    /// all new TLS handshakes before returning.
    pub async fn activate_and_reload(
        &self,
        store: &Store,
        generation: CertificateGeneration,
        renewal: crate::acme_types::RenewalState,
    ) -> Result<usize> {
        store
            .activate_generation_async(generation, renewal)
            .await?;
        self.reload(store).await
    }

    pub async fn reload(&self, store: &Store) -> Result<usize> {
        let store = store.clone();
        let entries = tokio::task::spawn_blocking(move || load_entries(&store))
            .await
            .context("managed certificate cache task failed")??;
        let count = entries.len();
        *self.inner.write().await = Arc::new(entries);
        info!("managed certificate cache atomically reloaded: domains={count}");
        Ok(count)
    }

    pub async fn resolve(&self, sni: &str) -> Option<Arc<CertifiedKey>> {
        let domain = normalize_domain(sni).ok()?;
        self.inner
            .read()
            .await
            .get(&domain)
            .map(|entry| Arc::clone(&entry.key))
    }

    pub async fn identity(&self, sni: &str) -> Option<(String, String)> {
        let domain = normalize_domain(sni).ok()?;
        self.inner.read().await.get(&domain).map(|entry| {
            (entry.certificate_id.clone(), entry.generation_id.clone())
        })
    }

    pub async fn resolve_with_fallback(
        &self,
        sni: &str,
        ca: &crate::ca::LocalCa,
        fallback: CertificateFallbackPolicy,
    ) -> Result<Arc<CertifiedKey>> {
        if let Some(key) = self.resolve(sni).await {
            return Ok(key);
        }
        match fallback {
            CertificateFallbackPolicy::LocalCa => ca.resolve_or_mint(sni),
            CertificateFallbackPolicy::Reject => {
                bail!("no active managed certificate for `{sni}` and fallback is reject")
            }
        }
    }
}

fn load_entries(store: &Store) -> Result<HashMap<String, CachedCertificate>> {
    let mut entries = HashMap::new();
    for managed in store.managed_certificates()? {
        if !managed.enabled {
            continue;
        }
        let Some(generation) = store.active_generation(&managed.id)? else {
            continue;
        };
        let cached = match prepare_entry(&managed, &generation) {
            Ok(value) => value,
            Err(cause) => {
                log::warn!(
                    "skipping unusable managed certificate generation: cert={}, generation={}, error={cause:#}",
                    managed.id,
                    generation.id
                );
                continue;
            }
        };
        for domain in &managed.domains {
            entries.insert(normalize_domain(domain)?, cached.clone());
        }
    }
    Ok(entries)
}

pub fn validate_generation(
    managed: &ManagedCertificate,
    generation: &CertificateGeneration,
) -> Result<()> {
    prepare_entry(managed, generation).map(|_| ())
}

fn prepare_entry(
    managed: &ManagedCertificate,
    generation: &CertificateGeneration,
) -> Result<CachedCertificate> {
    if generation.certificate_id != managed.id {
        bail!("generation belongs to a different managed certificate");
    }
    let certs = CertificateDer::pem_slice_iter(generation.fullchain_pem().as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let leaf = certs
        .first()
        .context("certificate generation has no leaf")?
        .clone();
    let key = PrivateKeyDer::from_pem_slice(generation.private_key_pem.as_bytes())?;
    let certified = crate::certificate::certified_key_from_parts(certs, key)?;
    certified.keys_match()?;
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|cause| anyhow::anyhow!("invalid managed leaf certificate: {cause}"))?;
    let not_after = OffsetDateTime::from_unix_timestamp(parsed.validity().not_after.timestamp())?;
    if not_after <= OffsetDateTime::now_utc() {
        bail!("managed certificate generation is expired");
    }
    let sans = parsed
        .subject_alternative_name()?
        .context("managed certificate leaf has no subjectAltName")?
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(value) => normalize_domain(value).ok(),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    for domain in &managed.domains {
        let domain = normalize_domain(domain)?;
        if !sans.contains(&domain) {
            bail!("issued certificate does not cover managed domain `{domain}`");
        }
    }
    Ok(CachedCertificate {
        certificate_id: managed.id.clone(),
        generation_id: generation.id.clone(),
        key: Arc::new(certified),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair};

    fn generation(certificate_id: &str, domains: &[&str]) -> CertificateGeneration {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(domains.iter().map(|value| value.to_string()).collect::<Vec<_>>())
            .unwrap()
            .self_signed(&key)
            .unwrap();
        CertificateGeneration {
            id: "generation-1".into(),
            certificate_id: certificate_id.into(),
            certificate_pem: cert.pem(),
            private_key_pem: key.serialize_pem(),
            not_after: Some(OffsetDateTime::now_utc() + time::Duration::days(30)),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn reload_is_exact_sni_and_exposes_active_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let managed = ManagedCertificate {
            id: "site".into(),
            domains: vec!["www.example".into(), "api.example".into()],
            ..Default::default()
        };
        store.save_managed_certificate(&managed).unwrap();
        let generation = generation("site", &["www.example", "api.example"]);
        store
            .activate_generation(
                &generation,
                &crate::acme_types::RenewalState {
                    certificate_id: "site".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let cache = ManagedCertificateCache::default();
        assert_eq!(cache.reload(&store).await.unwrap(), 2);
        assert!(cache.resolve("WWW.EXAMPLE.").await.is_some());
        assert!(cache.resolve("other.example").await.is_none());
        assert_eq!(
            cache.identity("api.example").await,
            Some(("site".into(), "generation-1".into()))
        );
        let ca_directory = tempfile::tempdir().unwrap();
        let ca = crate::ca::LocalCa::new(
            ca_directory.path().to_str().unwrap(),
            &crate::config::LocalCaConfig::default(),
        )
        .unwrap();
        let exact = cache.resolve("www.example").await.unwrap();
        let selected = cache
            .resolve_with_fallback(
                "www.example",
                &ca,
                CertificateFallbackPolicy::LocalCa,
            )
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&exact, &selected));
        assert!(cache
            .resolve_with_fallback("unknown.example", &ca, CertificateFallbackPolicy::Reject)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn activation_hot_swaps_the_generation_for_new_handshakes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let managed = ManagedCertificate {
            id: "site".into(),
            domains: vec!["www.example".into()],
            ..Default::default()
        };
        store.save_managed_certificate(&managed).unwrap();
        let cache = ManagedCertificateCache::default();

        let mut first = generation("site", &["www.example"]);
        first.id = "generation-1".into();
        cache
            .activate_and_reload(
                &store,
                first,
                crate::acme_types::RenewalState { certificate_id: "site".into(), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(cache.identity("www.example").await.unwrap().1, "generation-1");

        let mut second = generation("site", &["www.example"]);
        second.id = "generation-2".into();
        cache
            .activate_and_reload(
                &store,
                second,
                crate::acme_types::RenewalState { certificate_id: "site".into(), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(cache.identity("www.example").await.unwrap().1, "generation-2");
    }

    #[test]
    fn rejects_generation_missing_a_managed_san() {
        let managed = ManagedCertificate {
            id: "site".into(),
            domains: vec!["www.example".into(), "api.example".into()],
            ..Default::default()
        };
        let generation = generation("site", &["www.example"]);
        assert!(validate_generation(&managed, &generation).is_err());
    }
}
