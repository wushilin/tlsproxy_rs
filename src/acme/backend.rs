use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use instant_acme::Account;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::scheduler::{BackendFuture, RenewalBackend, RenewalCandidate};
use crate::acme_challenge::ChallengeRegistry;
use crate::acme_types::{AcmeSettings, ControlPlaneConfig, RenewalState};
use crate::store::Store;

pub type DnsFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Public-DNS validation is injected so production can use explicitly
/// configured recursive resolvers while tests remain entirely local. It must
/// never consult the proxy's upstream DNS override table.
pub trait DnsPrerequisite: Send + Sync + 'static {
    fn verify<'a>(
        &'a self,
        certificate_id: &'a str,
        domains: &'a [String],
        control: &'a ControlPlaneConfig,
    ) -> DnsFuture<'a>;
}

pub struct StoreRenewalBackend<D> {
    store: Store,
    registry: ChallengeRegistry,
    settings: AcmeSettings,
    control: ControlPlaneConfig,
    dns: Arc<D>,
    accounts: Mutex<HashMap<String, Account>>,
    certificate_cache: Option<crate::managed_tls::ManagedCertificateCache>,
}

impl<D> StoreRenewalBackend<D> {
    pub fn new(
        store: Store,
        registry: ChallengeRegistry,
        settings: AcmeSettings,
        control: ControlPlaneConfig,
        dns: Arc<D>,
    ) -> Self {
        Self {
            store,
            registry,
            settings,
            control,
            dns,
            accounts: Mutex::new(HashMap::new()),
            certificate_cache: None,
        }
    }

    pub fn with_certificate_cache(
        mut self,
        cache: crate::managed_tls::ManagedCertificateCache,
    ) -> Self {
        self.certificate_cache = Some(cache);
        self
    }
}

impl<D: DnsPrerequisite> StoreRenewalBackend<D> {
    async fn refresh_ari(&self, now: OffsetDateTime) -> Result<()> {
        for managed in self.store.managed_certificates_async().await? {
            if !managed.enabled { continue; }
            let previous = self.store.renewal_state_async(managed.id.clone()).await?.unwrap_or_else(|| RenewalState { certificate_id: managed.id.clone(), ..Default::default() });
            if previous.ari_next_check.is_some_and(|next| next > now) { continue; }
            let Some(generation) = self.store.active_generation_async(managed.id.clone()).await? else { continue; };
            let account = match self.account(&managed.provider_id).await { Ok(value) => value, Err(cause) => { log::warn!("ACME ARI account unavailable: cert={}, error={cause:#}", managed.id); continue; } };
            match super::client::renewal_information(&account, &generation).await {
                Ok(Some(value)) => {
                    let mut state = previous;
                    state.ari_suggested_at = Some(value.suggested_at);
                    state.ari_explanation_url = value.explanation_url;
                    state.ari_checked_at = Some(now);
                    state.ari_next_check = Some(now + time::Duration::try_from(value.retry_after).unwrap_or(time::Duration::hours(12)));
                    self.store.save_renewal_state_async(state).await?;
                }
                Ok(None) => {
                    let mut state = previous;
                    state.ari_checked_at = Some(now);
                    state.ari_next_check = Some(now + time::Duration::days(7));
                    self.store.save_renewal_state_async(state).await?;
                }
                Err(cause) => log::warn!("ACME ARI refresh failed: cert={}, error={cause:#}", managed.id),
            }
        }
        Ok(())
    }
    async fn account(&self, provider_id: &str) -> Result<Account> {
        if let Some(account) = self.accounts.lock().await.get(provider_id).cloned() {
            return Ok(account);
        }
        let provider = self
            .store
            .provider_async(provider_id.to_owned())
            .await?
            .with_context(|| format!("ACME provider `{provider_id}` does not exist"))?;
        let account = match self
            .store
            .account_credentials_async(provider_id.to_owned())
            .await?
        {
            Some(stored) => {
                let bytes = serde_json::to_vec(&stored.credentials)?;
                super::client::restore_account(&provider, &bytes).await?
            }
            None => {
                let (account, credentials) = super::client::create_account(&provider).await?;
                let value = serde_json::to_value(credentials)?;
                let store = self.store.clone();
                let provider_id = provider_id.to_owned();
                tokio::task::spawn_blocking(move || {
                    store.save_account_credentials(&provider_id, value)
                })
                .await
                .context("ACME account database task failed")??;
                if provider.eab_hmac_key.is_some() {
                    let mut consumed = provider.clone();
                    consumed.eab_key_id = None;
                    consumed.eab_hmac_key = None;
                    let store = self.store.clone();
                    tokio::task::spawn_blocking(move || store.save_provider(&consumed))
                        .await
                        .context("ACME provider secret cleanup task failed")??;
                }
                account
            }
        };
        self.accounts
            .lock()
            .await
            .insert(provider_id.to_owned(), account.clone());
        Ok(account)
    }

    async fn renew_inner(&self, candidate: &RenewalCandidate) -> Result<()> {
        let managed = self
            .store
            .managed_certificate_async(candidate.certificate_id.clone())
            .await?
            .context("managed certificate was deleted before renewal")?;
        if !managed.enabled {
            bail!("managed certificate was disabled before renewal");
        }
        if managed.provider_id != candidate.provider_id || managed.domains != candidate.domains {
            bail!("managed certificate changed after renewal scan; retrying next scan");
        }
        self.dns.verify(&managed.id, &managed.domains, &self.control).await?;
        let account = self.account(&managed.provider_id).await?;
        let previous_generation = self.store.active_generation_async(managed.id.clone()).await?;
        let generation = super::client::issue(
            &account,
            &managed,
            &self.registry,
            Duration::from_secs(u64::from(self.settings.challenge_ttl_seconds)),
            previous_generation.as_ref(),
        )
        .await?;
        crate::managed_tls::validate_generation(&managed, &generation)?;
        let now = OffsetDateTime::now_utc();
        let ari = super::client::renewal_information(&account, &generation).await;
        let (ari_suggested_at, ari_explanation_url, ari_next_check) = match ari {
            Ok(Some(value)) => (Some(value.suggested_at), value.explanation_url, Some(now + time::Duration::try_from(value.retry_after).unwrap_or(time::Duration::hours(12)))),
            Ok(None) => (None, None, None),
            Err(cause) => { log::warn!("ACME ARI lookup failed after issuance: cert={}, error={cause:#}", managed.id); (None, None, None) }
        };
        self.store
            .activate_generation_async(
                generation,
                RenewalState {
                    certificate_id: managed.id,
                    last_attempt: Some(now),
                    last_success: Some(now),
                    next_attempt: None,
                    consecutive_failures: 0,
                    last_error: None,
                    ari_suggested_at,
                    ari_explanation_url,
                    ari_checked_at: Some(now),
                    ari_next_check,
                },
            )
            .await?;
        if let Some(cache) = &self.certificate_cache {
            cache.reload(&self.store).await?;
        }
        Ok(())
    }

    async fn record_failure(&self, certificate_id: &str, error: String) -> Result<()> {
        let previous = self
            .store
            .renewal_state_async(certificate_id.to_owned())
            .await?;
        let failures = previous
            .as_ref()
            .map_or(1, |state| state.consecutive_failures.saturating_add(1));
        let now = OffsetDateTime::now_utc();
        let jitter_percent = rand::random_range(80u64..=120);
        let retry_minutes = retry_delay_minutes(failures, jitter_percent);
        self.store
            .save_renewal_state_async(RenewalState {
                certificate_id: certificate_id.to_owned(),
                last_attempt: Some(now),
                last_success: previous.as_ref().and_then(|state| state.last_success),
                next_attempt: Some(now + time::Duration::minutes(i64::try_from(retry_minutes.max(1)).unwrap_or(720))),
                consecutive_failures: failures,
                last_error: Some(error),
                ari_suggested_at: previous.as_ref().and_then(|state| state.ari_suggested_at),
                ari_explanation_url: previous.as_ref().and_then(|state| state.ari_explanation_url.clone()),
                ari_checked_at: previous.as_ref().and_then(|state| state.ari_checked_at),
                ari_next_check: previous.and_then(|state| state.ari_next_check),
            })
            .await
    }
}

fn retry_delay_minutes(failures: u32, jitter_percent: u64) -> u64 {
    let exponential = 5u64.saturating_mul(1u64.checked_shl(failures.saturating_sub(1).min(16)).unwrap_or(u64::MAX));
    exponential.saturating_mul(jitter_percent.clamp(80, 120)).saturating_div(100).clamp(1, 12 * 60)
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use tempfile::tempdir;

    use super::*;
    use crate::acme_types::ManagedCertificate;

    struct RejectDns;

    #[test]
    fn retry_backoff_is_exponential_jittered_and_capped() {
        assert_eq!(retry_delay_minutes(1, 100), 5);
        assert_eq!(retry_delay_minutes(2, 100), 10);
        assert_eq!(retry_delay_minutes(3, 80), 16);
        assert_eq!(retry_delay_minutes(40, 120), 720);
    }

    impl DnsPrerequisite for RejectDns {
        fn verify<'a>(
            &'a self,
            _certificate_id: &'a str,
            _domains: &'a [String],
            _control: &'a ControlPlaneConfig,
        ) -> DnsFuture<'a> {
            Box::pin(async { Err(anyhow!("public DNS does not point to this service")) })
        }
    }

    #[tokio::test]
    async fn due_candidates_and_dns_failure_are_persisted_without_contacting_ca() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .save_managed_certificate(&ManagedCertificate {
                id: "home".into(),
                domains: vec!["Home.Example.".into()],
                provider_id: "letsencrypt".into(),
                ..Default::default()
            })
            .unwrap();
        let backend = StoreRenewalBackend::new(
            store.clone(),
            ChallengeRegistry::default(),
            AcmeSettings::default(),
            ControlPlaneConfig::default(),
            Arc::new(RejectDns),
        );
        let candidates = backend
            .due_certificates(OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].domains, vec!["home.example"]);
        assert!(backend.renew(candidates[0].clone()).await.is_err());
        let state = store.renewal_state("home").unwrap().unwrap();
        assert_eq!(state.consecutive_failures, 1);
        assert!(state.next_attempt.is_some());
        assert!(state
            .last_error
            .unwrap()
            .contains("public DNS does not point"));
        assert!(ChallengeRegistry::default().resolve("home.example").is_none());
    }
}

impl<D: DnsPrerequisite> RenewalBackend for StoreRenewalBackend<D> {
    fn due_certificates(
        &self,
        now: OffsetDateTime,
    ) -> BackendFuture<'_, Vec<RenewalCandidate>> {
        Box::pin(async move {
            self.refresh_ari(now).await?;
            Ok(self
                .store
                .due_managed_certificates_async(now, self.settings.renew_before_days)
                .await?
                .into_iter()
                .map(|certificate| RenewalCandidate {
                    certificate_id: certificate.id,
                    domains: certificate.domains,
                    provider_id: certificate.provider_id,
                })
                .collect())
        })
    }

    fn renew(&self, candidate: RenewalCandidate) -> BackendFuture<'_, ()> {
        Box::pin(async move {
            match self.renew_inner(&candidate).await {
                Ok(()) => Ok(()),
                Err(cause) => {
                    let message = format!("{cause:#}");
                    self.record_failure(&candidate.certificate_id, message)
                        .await?;
                    Err(cause)
                }
            }
        })
    }

    fn record_timeout(
        &self,
        candidate: &RenewalCandidate,
        deadline: Duration,
    ) -> BackendFuture<'_, ()> {
        let certificate_id = candidate.certificate_id.clone();
        Box::pin(async move {
            self.record_failure(
                &certificate_id,
                format!("renewal exceeded {} second deadline", deadline.as_secs()),
            )
            .await
        })
    }
}
