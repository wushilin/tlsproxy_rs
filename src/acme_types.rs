use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

fn default_true() -> bool {
    true
}

fn default_renew_before_days() -> u16 {
    15
}

fn default_challenge_ttl_seconds() -> u16 {
    600
}

fn default_scan_interval_hours() -> u16 {
    12
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlPlaneConfig {
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub self_ips: BTreeSet<String>,
    #[serde(default = "default_public_resolvers")]
    pub public_resolvers: Vec<String>,
    #[serde(default)]
    pub require_all_resolvers: bool,
    #[serde(default)]
    pub provider_id: String,
}

fn default_public_resolvers() -> Vec<String> {
    vec!["1.1.1.1".into(), "8.8.8.8".into()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeSettings {
    #[serde(default = "default_renew_before_days")]
    pub renew_before_days: u16,
    #[serde(default = "default_scan_interval_hours")]
    pub scan_interval_hours: u16,
    #[serde(default = "default_challenge_ttl_seconds")]
    pub challenge_ttl_seconds: u16,
}

impl Default for AcmeSettings {
    fn default() -> Self {
        Self {
            renew_before_days: default_renew_before_days(),
            scan_interval_hours: default_scan_interval_hours(),
            challenge_ttl_seconds: default_challenge_ttl_seconds(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcmeProvider {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub directory_url: String,
    #[serde(default)]
    pub contacts: Vec<String>,
    #[serde(default)]
    pub eab_key_id: Option<String>,
    #[serde(default)]
    pub eab_hmac_key: Option<String>,
    #[serde(default)]
    pub staging: bool,
    #[serde(default)]
    pub is_default: bool,
    /// Optional PEM root used only for private/local ACME directories such as Pebble.
    #[serde(default)]
    pub directory_ca_pem: Option<String>,
}

/// Opaque ACME library credentials. These values contain private account key
/// material and must never be returned by administrative list APIs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredAcmeAccount {
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub credentials: serde_json::Value,
    #[serde(default)]
    pub updated_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublishPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow_private_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedCertificate {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Created from a TLS-terminating host route and retained while in use.
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub publish: PublishPolicy,
    #[serde(default)]
    pub created_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub updated_at: Option<OffsetDateTime>,
}

impl Default for ManagedCertificate {
    fn default() -> Self {
        Self {
            id: String::new(),
            domains: Vec::new(),
            provider_id: String::new(),
            enabled: true,
            automatic: false,
            publish: PublishPolicy::default(),
            created_at: None,
            updated_at: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CertificateGeneration {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub certificate_id: String,
    #[serde(default)]
    pub certificate_pem: String,
    #[serde(default)]
    pub chain_pem: String,
    #[serde(default)]
    pub private_key_pem: String,
    #[serde(default)]
    pub fingerprint_sha256: String,
    #[serde(default)]
    pub not_before: Option<OffsetDateTime>,
    #[serde(default)]
    pub not_after: Option<OffsetDateTime>,
    #[serde(default)]
    pub issued_at: Option<OffsetDateTime>,
}

impl CertificateGeneration {
    pub fn fullchain_pem(&self) -> String {
        let mut value = self.certificate_pem.trim_end().to_owned();
        if !self.chain_pem.trim().is_empty() {
            value.push('\n');
            value.push_str(self.chain_pem.trim());
        }
        value.push('\n');
        value
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenewalState {
    #[serde(default)]
    pub certificate_id: String,
    #[serde(default)]
    pub last_attempt: Option<OffsetDateTime>,
    #[serde(default)]
    pub last_success: Option<OffsetDateTime>,
    #[serde(default)]
    pub next_attempt: Option<OffsetDateTime>,
    /// A server-provided retry deadline. Unlike a local retry gate, an
    /// administrator-triggered scan must not clear this value early.
    #[serde(default)]
    pub ca_retry_after: Option<OffsetDateTime>,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    /// RFC 9773 suggested renewal instant selected from the CA window.
    #[serde(default)]
    pub ari_suggested_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub ari_explanation_url: Option<String>,
    #[serde(default)]
    pub ari_checked_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub ari_next_check: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetrievalToken {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// SHA-256 of the bearer value, used for constant-time authentication.
    #[serde(default)]
    pub token_hash: String,
    /// Recoverable bearer value. This is intentionally persisted so an
    /// administrator can copy an existing retrieval token again.
    #[serde(default)]
    pub bearer_token: String,
    /// Empty means every published managed certificate.
    #[serde(default)]
    pub certificate_ids: BTreeSet<String>,
    #[serde(default)]
    pub allow_private_key: bool,
    #[serde(default)]
    pub created_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DnsDiagnostic {
    #[serde(default)]
    pub certificate_id: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub resolver: String,
    #[serde(default)]
    pub addresses: BTreeSet<String>,
    #[serde(default)]
    pub passed: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub checked_at: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_json_defaults_new_fields_and_ignores_unknown_fields() {
        let json = r#"{
            "id":"home",
            "domains":["home.example"],
            "provider_id":"le",
            "field_from_a_newer_binary":true
        }"#;
        let certificate: ManagedCertificate = serde_json::from_str(json).unwrap();
        assert!(certificate.enabled);
        assert!(!certificate.publish.enabled);
    }
}
