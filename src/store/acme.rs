//! ACME domain state: providers, account credentials, managed
//! certificates, generations, renewal state, and DNS diagnostics.

use super::*;

impl Store {
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
        if !config.acme.enabled {
            return Ok(0);
        }
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
        // Every managed certificate is deletable, issued or not, automatic or
        // manual — accumulated unwanted registrations would otherwise consume
        // renewal quota forever. If a TLS route still matches the domain, the
        // next handshake re-registers it, subject to the DNS check script.
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

fn generation_key(certificate_id: &str, generation_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(certificate_id.len() + generation_id.len() + 1);
    key.extend_from_slice(certificate_id.as_bytes());
    key.push(0);
    key.extend_from_slice(generation_id.as_bytes());
    key
}
