//! Provider-neutral RFC 8555 issuance using the TLS-ALPN-01 challenge.
//!
//! This module owns ACME protocol state, while `acme_challenge` owns only the
//! short-lived certificates presented by the mandatory port-443 listener.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use log::info;
use rand::RngCore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::acme_challenge::{ChallengeGuard, ChallengeRegistry};
use crate::acme_types::{AcmeProvider, CertificateGeneration, ManagedCertificate};
use crate::store::normalize_domain;

pub async fn create_account(provider: &AcmeProvider) -> Result<(Account, AccountCredentials)> {
    if provider.directory_url.trim().is_empty() {
        bail!("ACME provider directory URL is required");
    }
    let contacts: Vec<&str> = provider.contacts.iter().map(String::as_str).collect();
    let external = match (&provider.eab_key_id, &provider.eab_hmac_key) {
        (Some(id), Some(value)) => {
            let decoded = general_purpose::URL_SAFE_NO_PAD
                .decode(value)
                .or_else(|_| general_purpose::URL_SAFE.decode(value))
                .context("ACME EAB HMAC key is not valid base64url")?;
            Some(instant_acme::ExternalAccountKey::new(id.clone(), &decoded))
        }
        (None, None) => None,
        _ => bail!("ACME external account binding requires both key ID and HMAC key"),
    };
    info!("ACME account creation started: provider={}, directory={}", provider.id, provider.directory_url);
    let result = account_builder(provider)?
        .create(
            &NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            provider.directory_url.clone(),
            external.as_ref(),
        )
        .await
        .context("failed to create ACME account")?;
    info!("ACME account creation succeeded: provider={}", provider.id);
    Ok(result)
}

pub async fn restore_account(provider: &AcmeProvider, credentials_json: &[u8]) -> Result<Account> {
    let credentials: AccountCredentials =
        serde_json::from_slice(credentials_json).context("invalid stored ACME credentials")?;
    let account = account_builder(provider)?
        .from_credentials(credentials)
        .await
        .context("failed to restore ACME account")?;
    info!("ACME account restored from runtime store");
    Ok(account)
}

fn account_builder(provider: &AcmeProvider) -> Result<instant_acme::AccountBuilder> {
    let Some(pem) = provider.directory_ca_pem.as_deref() else { return Ok(Account::builder()?); };
    let mut file = tempfile::NamedTempFile::new().context("failed to create temporary CA file")?;
    std::io::Write::write_all(&mut file, pem.as_bytes())?;
    Ok(Account::builder_with_root(file.path())?)
}

/// Issues one certificate. The caller owns the overall five-minute deadline;
/// this function keeps challenge guards alive until every authorization has
/// become ready, then clears them before finalization.
pub async fn issue(
    account: &Account,
    managed: &ManagedCertificate,
    registry: &ChallengeRegistry,
    challenge_ttl: Duration,
    previous: Option<&CertificateGeneration>,
) -> Result<CertificateGeneration> {
    if managed.domains.is_empty() {
        bail!("managed certificate has no domains");
    }
    let domains = managed
        .domains
        .iter()
        .map(|domain| normalize_domain(domain))
        .collect::<Result<Vec<_>>>()?;
    let unique = domains.iter().collect::<std::collections::BTreeSet<_>>();
    if unique.len() != domains.len() {
        bail!("managed certificate contains duplicate domains");
    }
    let identifiers = domains
        .iter()
        .cloned()
        .map(Identifier::Dns)
        .collect::<Vec<_>>();
    info!("ACME order creation started: cert={}, domains={domains:?}", managed.id);
    let mut new_order = NewOrder::new(&identifiers);
    let previous_der = previous.and_then(|value| CertificateDer::from_pem_slice(value.certificate_pem.as_bytes()).ok());
    let previous_id = previous_der.as_ref().and_then(|der| instant_acme::CertificateIdentifier::try_from(der).ok());
    if let Some(identifier) = previous_id { new_order = new_order.replaces(identifier); }
    let mut order = account
        .new_order(&new_order)
        .await
        .context("failed to create ACME order")?;
    info!("ACME order created: cert={}, order={}", managed.id, order.url());
    let mut guards: Vec<ChallengeGuard> = Vec::with_capacity(domains.len());
    let mut authorizations = order.authorizations();
    while let Some(result) = authorizations.next().await {
        let mut authorization = result.context("failed to fetch ACME authorization")?;
        match authorization.status {
            AuthorizationStatus::Valid => continue,
            AuthorizationStatus::Pending => {}
            ref status => bail!("ACME authorization has unexpected status {status:?}"),
        }
        let mut challenge = authorization
            .challenge(ChallengeType::TlsAlpn01)
            .context("ACME server does not offer tls-alpn-01")?;
        let domain = match &challenge.identifier().identifier {
            Identifier::Dns(domain) => normalize_domain(domain)?,
            identifier => bail!("unsupported ACME identifier {identifier:?}"),
        };
        let key_authorization = challenge.key_authorization();
        info!("ACME TLS-ALPN authorization provisioning: cert={}, domain={domain}", managed.id);
        guards.push(registry.register(
            &domain,
            key_authorization.as_str().as_bytes(),
            challenge_ttl,
        )?);
        challenge
            .set_ready()
            .await
            .with_context(|| format!("failed to mark TLS-ALPN challenge ready for {domain}"))?;
    }
    drop(authorizations);

    let retries = RetryPolicy::new()
        .initial_delay(Duration::from_millis(500))
        .timeout(Duration::from_secs(180));
    let status = order
        .poll_ready(&retries)
        .await
        .context("ACME authorization polling failed")?;
    drop(guards);
    if status != OrderStatus::Ready {
        bail!("ACME order did not become ready: {status:?}");
    }

    info!("ACME authorizations ready; challenge gates cleared: cert={}", managed.id);

    info!("ACME order finalization started: cert={}", managed.id);
    let private_key_pem = order.finalize().await.context("ACME finalization failed")?;
    let fullchain = order
        .poll_certificate(&retries)
        .await
        .context("ACME certificate download failed")?;
    let generation = generation_from_pem(&managed.id, &fullchain, &private_key_pem)?;
    info!("ACME certificate downloaded and parsed: cert={}, generation={}, not_after={:?}", managed.id, generation.id, generation.not_after);
    Ok(generation)
}

fn generation_from_pem(
    certificate_id: &str,
    fullchain_pem: &str,
    private_key_pem: &str,
) -> Result<CertificateGeneration> {
    let blocks = certificate_blocks(fullchain_pem);
    let certificate_pem = blocks.first().context("ACME response contained no certificate")?;
    let chain_pem = blocks.iter().skip(1).cloned().collect::<Vec<_>>().join("\n");
    let certificate_der = CertificateDer::from_pem_slice(certificate_pem.as_bytes())?;
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate_der.as_ref())
        .map_err(|cause| anyhow::anyhow!("invalid issued certificate: {cause}"))?;
    let not_before = OffsetDateTime::from_unix_timestamp(parsed.validity().not_before.timestamp())?;
    let not_after = OffsetDateTime::from_unix_timestamp(parsed.validity().not_after.timestamp())?;
    validate_issuer_chain(&blocks)?;
    let mut id_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut id_bytes);
    Ok(CertificateGeneration {
        id: hex::encode(id_bytes),
        certificate_id: certificate_id.to_owned(),
        certificate_pem: certificate_pem.clone(),
        chain_pem,
        private_key_pem: private_key_pem.to_owned(),
        fingerprint_sha256: hex::encode(Sha256::digest(certificate_der.as_ref())),
        not_before: Some(not_before),
        not_after: Some(not_after),
        issued_at: Some(OffsetDateTime::now_utc()),
    })
}

pub struct AriSuggestion {
    pub suggested_at: OffsetDateTime,
    pub explanation_url: Option<String>,
    pub retry_after: Duration,
}

pub async fn renewal_information(account: &Account, generation: &CertificateGeneration) -> Result<Option<AriSuggestion>> {
    let der = CertificateDer::from_pem_slice(generation.certificate_pem.as_bytes())?;
    let identifier = instant_acme::CertificateIdentifier::try_from(&der)
        .map_err(anyhow::Error::msg)?;
    match account.renewal_info(&identifier).await {
        Ok((info, retry_after)) => {
            let start = info.suggested_window.start;
            let end = info.suggested_window.end;
            let span = (end - start).whole_seconds().max(0);
            let offset = if span == 0 { 0 } else { rand::random_range(0..=span) };
            Ok(Some(AriSuggestion { suggested_at: start + time::Duration::seconds(offset), explanation_url: info.explanation_url, retry_after }))
        }
        Err(instant_acme::Error::Unsupported(_)) => Ok(None),
        Err(cause) => Err(cause.into()),
    }
}

fn validate_issuer_chain(blocks: &[String]) -> Result<()> {
    let parsed = blocks.iter().map(|pem| {
        let der = CertificateDer::from_pem_slice(pem.as_bytes())?;
        let (_, _cert) = x509_parser::parse_x509_certificate(der.as_ref())
            .map_err(|cause| anyhow::anyhow!("invalid certificate in issuer chain: {cause}"))?;
        Ok::<_, anyhow::Error>(der)
    }).collect::<Result<Vec<_>>>()?;
    for pair in parsed.windows(2) {
        let (_, child) = x509_parser::parse_x509_certificate(pair[0].as_ref()).map_err(|e| anyhow::anyhow!("invalid child certificate: {e}"))?;
        let (_, issuer) = x509_parser::parse_x509_certificate(pair[1].as_ref()).map_err(|e| anyhow::anyhow!("invalid issuer certificate: {e}"))?;
        if child.issuer() != issuer.subject() { bail!("issued certificate chain is not ordered by issuer"); }
        child.verify_signature(Some(issuer.public_key())).context("issued certificate has an invalid issuer signature")?;
    }
    Ok(())
}

fn certificate_blocks(pem: &str) -> Vec<String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut inside = false;
    for line in pem.lines() {
        if line.trim() == BEGIN {
            current.clear();
            inside = true;
        }
        if inside {
            current.push(line);
        }
        if inside && line.trim() == END {
            blocks.push(format!("{}\n", current.join("\n")));
            current.clear();
            inside = false;
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::{certificate_blocks, create_account};

    #[test]
    fn splits_leaf_and_chain_without_accepting_unterminated_blocks() {
        let pem = "noise\n-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nbroken";
        let blocks = certificate_blocks(pem);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("leaf"));
        assert!(blocks[1].contains("ca"));
    }

    #[tokio::test]
    #[ignore = "opt-in public network interoperability test"]
    async fn letsencrypt_staging_account_interoperability() {
        let _ = create_account(&crate::acme_types::AcmeProvider { id: "le-staging".into(), directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(), staging: true, ..Default::default() }).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires GTS_EAB_KEY_ID and GTS_EAB_HMAC plus public network"]
    async fn gts_staging_account_interoperability() {
        let key_id = std::env::var("GTS_EAB_KEY_ID").expect("GTS_EAB_KEY_ID is required");
        let hmac = std::env::var("GTS_EAB_HMAC").expect("GTS_EAB_HMAC is required");
        let _ = create_account(&crate::acme_types::AcmeProvider { id: "gts-staging".into(), directory_url: "https://dv.acme-v02.test-api.pki.goog/directory".into(), eab_key_id: Some(key_id), eab_hmac_key: Some(hmac), staging: true, ..Default::default() }).await.unwrap();
    }
}
