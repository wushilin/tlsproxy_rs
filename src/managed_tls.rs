use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock as StdRwLock};

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

#[derive(Clone)]
struct AutoRegistration {
    store: Store,
    request_scan: Arc<dyn Fn() + Send + Sync>,
}

fn auto_registration() -> &'static StdRwLock<Option<AutoRegistration>> {
    static REGISTRATION: OnceLock<StdRwLock<Option<AutoRegistration>>> = OnceLock::new();
    REGISTRATION.get_or_init(|| StdRwLock::new(None))
}

pub fn configure_auto_registration(store: Store, request_scan: impl Fn() + Send + Sync + 'static) {
    *auto_registration().write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(AutoRegistration {
        store,
        request_scan: Arc::new(request_scan),
    });
}

fn recent_registration_attempts() -> &'static StdRwLock<lru::LruCache<String, std::time::Instant>> {
    static ATTEMPTS: OnceLock<StdRwLock<lru::LruCache<String, std::time::Instant>>> = OnceLock::new();
    ATTEMPTS.get_or_init(|| {
        StdRwLock::new(lru::LruCache::new(std::num::NonZeroUsize::new(4096).expect("non-zero cache size")))
    })
}

const REGISTRATION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

const DNS_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Rejections only. An accepted domain immediately gains a certificate — a
/// cached local-CA leaf or a managed record — so the script is never asked
/// about it again; only rejected hostnames would otherwise spawn a process on
/// every handshake.
fn dns_check_rejections() -> &'static StdRwLock<lru::LruCache<(String, String), std::time::Instant>> {
    static REJECTIONS: OnceLock<StdRwLock<lru::LruCache<(String, String), std::time::Instant>>> = OnceLock::new();
    REJECTIONS.get_or_init(|| {
        StdRwLock::new(lru::LruCache::new(std::num::NonZeroUsize::new(4096).expect("non-zero cache size")))
    })
}

/// Bounds concurrent DNS-check-script processes so a burst of unmanaged SNIs
/// cannot fork-bomb the host; excess handshakes wait their turn.
fn dns_check_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(4))
}

/// Consults the operator's optional DNS check script before a *new*
/// certificate is created for `domain` — an automatic ACME registration or a
/// fresh local-CA fallback mint. Domains that already hold a certificate are
/// never re-checked; callers only invoke this when issuance is imminent.
///
/// Domains listed verbatim in an `exact` route entry skip the script: only
/// suffix and regex routes accept client-chosen hostnames, so only they can
/// be gamed into unwanted issuance.
async fn dns_check_permits_new_issuance(domain: &str) -> Result<()> {
    let config = crate::runtime_live::load();
    let script = config.acme.dns_check_script.trim().to_string();
    if script.is_empty() {
        return Ok(());
    }
    if config.tls_routes_list_host_exactly(domain) {
        return Ok(());
    }
    let failure_ttl = std::time::Duration::from_secs(u64::from(config.acme.cache_failure_time_secs));
    if dns_check_allows(&script, domain, failure_ttl).await {
        Ok(())
    } else {
        bail!("DNS check script rejected domain `{domain}`")
    }
}

/// Runs the operator's DNS check script for one domain. Rejections are
/// remembered for `failure_ttl` so a rejected hostname does not spawn a
/// process per handshake; acceptances are not cached (the resulting
/// certificate makes re-checks moot). Spawn failures, non-zero exits, and
/// timeouts all fail closed.
async fn dns_check_allows(script: &str, domain: &str, failure_ttl: std::time::Duration) -> bool {
    let key = (script.to_string(), domain.to_string());
    {
        let mut rejections = dns_check_rejections().write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(rejected_at) = rejections.get(&key) {
            if rejected_at.elapsed() < failure_ttl {
                return false;
            }
            rejections.pop(&key);
        }
    }
    let _slot = dns_check_slots().acquire().await.expect("DNS check semaphore is never closed");
    let allowed = run_dns_check_script(script, domain).await;
    if !allowed {
        dns_check_rejections()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .put(key, std::time::Instant::now());
    }
    allowed
}

async fn run_dns_check_script(script: &str, domain: &str) -> bool {
    let mut command = tokio::process::Command::new(script);
    command
        .arg(domain)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(cause) => {
            log::warn!("DNS check script `{script}` could not run for `{domain}`: {cause}; rejecting");
            return false;
        }
    };
    let output = match tokio::time::timeout(DNS_CHECK_TIMEOUT, child.wait_with_output()).await {
        Err(_elapsed) => {
            // `wait_with_output` consumed the child, so dropping the timed-out
            // future just now (via kill_on_drop) SIGKILLed the script process.
            log::warn!("DNS check script `{script}` exceeded {DNS_CHECK_TIMEOUT:?} for `{domain}`; force-killed and rejecting");
            return false;
        }
        Ok(Err(cause)) => {
            log::warn!("DNS check script `{script}` could not be awaited for `{domain}`: {cause}; rejecting");
            return false;
        }
        Ok(Ok(output)) => output,
    };
    if output.status.success() {
        info!("DNS check script accepted `{domain}`");
        return true;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    log::warn!(
        "DNS check script rejected `{domain}` ({}){}{}",
        output.status,
        if stderr.is_empty() { "" } else { ": " },
        stderr
    );
    false
}

/// Schedules certificate registration for a concrete SNI after an explicit
/// terminating route has accepted it. This supports exact, suffix, and regex
/// host rules without wildcards.
///
/// Because suffix/regex routes accept arbitrary client-chosen subdomains, a
/// domain is only registered after the public-DNS prerequisite confirms it
/// resolves to this deployment's configured self IPs — otherwise a client
/// could mint unbounded certificate records and real ACME orders just by
/// varying the SNI. (An operator wildcard DNS record pointing here makes every
/// subdomain pass; that is the operator's explicit choice.) Attempts are
/// throttled per domain and run in the background so TLS handshakes are never
/// delayed by DNS lookups or store writes.
pub fn request_automatic_for_sni(sni: &str) {
    if !crate::runtime_live::load().acme.enabled {
        return;
    }
    let Ok(domain) = normalize_domain(sni) else { return };
    let Some(registration) = auto_registration().read().unwrap_or_else(std::sync::PoisonError::into_inner).clone() else {
        return;
    };
    {
        let mut attempts = recent_registration_attempts().write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempts.get(&domain).is_some_and(|last| last.elapsed() < REGISTRATION_RETRY_INTERVAL) {
            return;
        }
        attempts.put(domain.clone(), std::time::Instant::now());
    }
    tokio::spawn(async move {
        if let Err(cause) = register_automatic(domain.clone(), registration).await {
            log::warn!("automatic certificate registration for `{domain}` was not performed: {cause:#}");
        }
    });
}

async fn register_automatic(domain: String, registration: AutoRegistration) -> Result<()> {
    dns_check_permits_new_issuance(&domain).await?;
    let store = registration.store.clone();
    let existing = {
        let store = store.clone();
        let domain = domain.clone();
        tokio::task::spawn_blocking(move || store.certificate_for_domain(&domain))
            .await
            .context("automatic certificate lookup task failed")??
    };
    if existing.is_some() {
        return Ok(());
    }
    let control = crate::runtime_live::load().control_plane.clone();
    let dns = crate::acme::dns::PublicDnsPrerequisite::default().with_store(store.clone());
    crate::acme::backend::DnsPrerequisite::verify(&dns, &domain, std::slice::from_ref(&domain), &control)
        .await
        .context("public DNS prerequisite rejected SNI-driven registration")?;
    let created = {
        let domain = domain.clone();
        tokio::task::spawn_blocking(move || store.create_automatic_certificate_if_absent(&domain))
            .await
            .context("automatic certificate registration task failed")??
    };
    if created {
        info!("registered automatic certificate for `{domain}` after DNS verification");
        (registration.request_scan)();
    }
    Ok(())
}

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

    /// [`Self::resolve_with_fallback`] for ordinary terminating routes: the
    /// operator's DNS check script is consulted before the local CA mints a
    /// certificate for a domain it has never issued for. Active managed
    /// certificates and still-cached local-CA leaves serve without a check —
    /// the script gates new issuance only. The control-plane service keeps
    /// using the ungated variant so a rejecting script can never lock the
    /// administrator out.
    pub async fn resolve_for_termination(
        &self,
        sni: &str,
        ca: &crate::ca::LocalCa,
        fallback: CertificateFallbackPolicy,
    ) -> Result<Arc<CertifiedKey>> {
        if let Some(key) = self.resolve(sni).await {
            return Ok(key);
        }
        match fallback {
            CertificateFallbackPolicy::LocalCa => {
                if let Some(key) = ca.resolve_cached(sni) {
                    return Ok(key);
                }
                let domain = normalize_domain(sni)
                    .map_err(|cause| anyhow::anyhow!("invalid SNI for DNS check: {cause}"))?;
                dns_check_permits_new_issuance(&domain).await?;
                ca.resolve_or_mint(sni)
            }
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
        for domain in ["www.example", "api.example"] {
            let managed = ManagedCertificate { id: domain.into(), domains: vec![domain.into()], ..Default::default() };
            store.save_managed_certificate(&managed).unwrap();
            store.activate_generation(&generation(domain, &[domain]), &crate::acme_types::RenewalState { certificate_id: domain.into(), ..Default::default() }).unwrap();
        }
        let cache = ManagedCertificateCache::default();
        assert_eq!(cache.reload(&store).await.unwrap(), 2);
        assert!(cache.resolve("WWW.EXAMPLE.").await.is_some());
        assert!(cache.resolve("other.example").await.is_none());
        assert_eq!(
            cache.identity("api.example").await,
            Some(("api.example".into(), "generation-1".into()))
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
            id: "www.example".into(),
            domains: vec!["www.example".into()],
            ..Default::default()
        };
        store.save_managed_certificate(&managed).unwrap();
        let cache = ManagedCertificateCache::default();

        let mut first = generation("www.example", &["www.example"]);
        first.id = "generation-1".into();
        cache
            .activate_and_reload(
                &store,
                first,
                crate::acme_types::RenewalState { certificate_id: "www.example".into(), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(cache.identity("www.example").await.unwrap().1, "generation-1");

        let mut second = generation("www.example", &["www.example"]);
        second.id = "generation-2".into();
        cache
            .activate_and_reload(
                &store,
                second,
                crate::acme_types::RenewalState { certificate_id: "www.example".into(), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(cache.identity("www.example").await.unwrap().1, "generation-2");
    }

    #[tokio::test]
    async fn dns_check_script_gates_domains_and_caches_only_rejections() {
        use std::os::unix::fs::PermissionsExt;
        let ttl = std::time::Duration::from_secs(60);
        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("dns-check.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\ncase \"$1\" in *.allowed.example) exit 0;; esac\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = script_path.to_str().unwrap();

        assert!(dns_check_allows(script, "bucket.allowed.example", ttl).await);
        assert!(!dns_check_allows(script, "bucket.other.example", ttl).await);

        let missing = directory.path().join("missing.sh");
        assert!(!dns_check_allows(missing.to_str().unwrap(), "bucket.allowed.example", ttl).await);

        // Rejections are cached: they survive the script disappearing …
        std::fs::remove_file(&script_path).unwrap();
        assert!(!dns_check_allows(script, "bucket.other.example", ttl).await);
        // … but acceptances are not — the same domain now fails closed.
        assert!(!dns_check_allows(script, "bucket.allowed.example", std::time::Duration::ZERO).await);
        // A zero TTL means an expired rejection is re-checked (and, with the
        // script gone, keeps failing closed).
        assert!(!dns_check_allows(script, "bucket.other.example", std::time::Duration::ZERO).await);
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
