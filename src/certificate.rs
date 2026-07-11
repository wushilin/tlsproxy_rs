use std::fs::{self, OpenOptions};
use std::io::{BufReader, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use log::{info, warn};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::{
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer},
    sign::CertifiedKey,
};
use time::{Duration, OffsetDateTime};
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::FromDer;

const DEFAULT_VALIDITY_DAYS: u32 = 365;
const CA_VALIDITY_DAYS: i64 = 3650;

#[derive(Debug, Clone)]
pub struct CertificatePaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LocalIdentityPaths {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

pub struct MintedIdentity {
    pub certified_key: CertifiedKey,
    pub cert_pem: String,
    pub key_pem: String,
    pub expires_at: OffsetDateTime,
}

/// Generates or reuses an identity signed by the locally managed CA. The CA
/// is created on first use; the leaf is regenerated when missing, nearing
/// expiry, or no longer covering `san`.
pub fn prepare_local_identity(
    paths: &LocalIdentityPaths,
    san: &[String],
) -> Result<CertificatePaths> {
    let issuer = load_or_create_ca(&paths.ca_cert, &paths.ca_key)?;
    let sans = parse_sans(san)?;
    if !leaf_is_reusable(&paths.cert, &paths.key, &paths.ca_cert, &sans) {
        info!(
            "local identity `{}` is missing, stale, or SAN-mismatched; generating new certificate",
            paths.cert.display()
        );
        generate_leaf(
            &issuer,
            &paths.cert,
            &paths.key,
            sans,
            DEFAULT_VALIDITY_DAYS,
        )?;
    } else {
        info!("reusing existing local identity `{}`", paths.cert.display());
    }
    Ok(CertificatePaths {
        cert: paths.cert.clone(),
        key: paths.key.clone(),
    })
}

/// Splits configured SAN entries into DNS and IP subject alternative names.
pub fn parse_sans(san: &[String]) -> Result<Vec<SanType>> {
    let mut sans = Vec::new();
    for entry in san {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match IpAddr::from_str(entry) {
            Ok(ip) => sans.push(SanType::IpAddress(ip)),
            Err(_) => sans.push(SanType::DnsName(
                entry
                    .to_string()
                    .try_into()
                    .map_err(|_| anyhow!("invalid certificate DNS name `{entry}`"))?,
            )),
        }
    }
    if sans.is_empty() {
        sans.push(SanType::DnsName("localhost".try_into().unwrap()));
    }
    Ok(sans)
}

/// Remaining validity of a PEM certificate file; zero when already expired.
pub fn remaining_validity(cert_path: &Path) -> Result<std::time::Duration> {
    let pem = fs::read(cert_path)
        .with_context(|| format!("failed to read certificate `{}`", cert_path.display()))?;
    let (_, pem) = parse_x509_pem(&pem).context("failed to decode certificate PEM")?;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(&pem.contents)
        .context("failed to parse X.509 certificate")?;
    Ok(certificate
        .validity()
        .time_to_expiration()
        .and_then(|remaining| std::time::Duration::try_from(remaining).ok())
        .unwrap_or(std::time::Duration::ZERO))
}

pub fn certificate_expires_at(cert_der: &[u8]) -> Result<OffsetDateTime> {
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .context("failed to parse X.509 certificate")?;
    let timestamp = certificate.validity().not_after.timestamp();
    OffsetDateTime::from_unix_timestamp(timestamp)
        .context("certificate has an unsupported expiration timestamp")
}

pub fn certificate_file_expires_at(cert_path: &Path) -> Result<OffsetDateTime> {
    let pem = fs::read(cert_path)
        .with_context(|| format!("failed to read certificate `{}`", cert_path.display()))?;
    let (_, pem) = parse_x509_pem(&pem).context("failed to decode certificate PEM")?;
    certificate_expires_at(&pem.contents)
}

/// Writes a certificate/key PEM pair to disk atomically (key mode 0600).
pub fn write_identity(paths: &CertificatePaths, cert_pem: &str, key_pem: &str) -> Result<()> {
    atomic_write(&paths.cert, cert_pem.as_bytes(), false)?;
    atomic_write(&paths.key, key_pem.as_bytes(), true)
}

pub fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<CertifiedKey> {
    let certs = read_certificates(cert_path)?;
    let key = read_private_key(key_path)?;
    certified_key_from_parts(certs, key)
}

pub fn load_server_config(cert_path: &Path, key_path: &Path) -> Result<rustls::ServerConfig> {
    let certs = read_certificates(cert_path)?;
    let key = read_private_key(key_path)?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("certificate and key do not form a valid TLS identity: {e}"))
}

pub fn load_or_create_ca(cert_path: &Path, key_path: &Path) -> Result<Issuer<'static, KeyPair>> {
    match (cert_path.exists(), key_path.exists()) {
        (false, false) => {
            info!(
                "local CA not found at `{}` / `{}`; generating new CA",
                cert_path.display(),
                key_path.display()
            );
            generate_ca(cert_path, key_path)
        }
        (true, true) => load_ca(cert_path, key_path),
        _ => bail!(
            "local CA is incomplete: both `{}` and `{}` must exist or both must be absent",
            cert_path.display(),
            key_path.display()
        ),
    }
}

fn generate_ca(cert_path: &Path, key_path: &Path) -> Result<Issuer<'static, KeyPair>> {
    let key = KeyPair::generate().context("failed to generate local CA private key")?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "TLS Proxy Local CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
    params.not_after = OffsetDateTime::now_utc() + Duration::days(CA_VALIDITY_DAYS);
    let certificate = params
        .self_signed(&key)
        .context("failed to create local CA certificate")?;
    atomic_write(cert_path, certificate.pem().as_bytes(), false)?;
    atomic_write(key_path, key.serialize_pem().as_bytes(), true)?;
    info!(
        "generated new local CA `{}` (valid until {})",
        cert_path.display(),
        params.not_after
    );
    Ok(Issuer::new(params, key))
}

fn load_ca(cert_path: &Path, key_path: &Path) -> Result<Issuer<'static, KeyPair>> {
    let cert_pem = fs::read_to_string(cert_path).with_context(|| {
        format!(
            "failed to read local CA certificate `{}`",
            cert_path.display()
        )
    })?;
    let key_pem = fs::read_to_string(key_path)
        .with_context(|| format!("failed to read local CA key `{}`", key_path.display()))?;
    let key = KeyPair::from_pem(&key_pem).context("failed to parse local CA private key")?;
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).context("failed to decode local CA PEM")?;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(&pem.contents)
        .context("failed to parse local CA X.509 certificate")?;
    if !certificate.is_ca() {
        bail!("local CA certificate is not marked as a certificate authority");
    }
    if !certificate.validity().is_valid() {
        if certificate.validity().time_to_expiration().is_none() {
            warn!(
                "local CA `{}` is expired; replacing it with a new CA",
                cert_path.display()
            );
            return generate_ca(cert_path, key_path);
        }
        bail!("local CA certificate is not currently valid");
    }
    if certificate.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        bail!("local CA certificate and private key do not match");
    }
    certificate
        .verify_signature(None)
        .context("local CA self-signature is invalid")?;
    info!(
        "loaded existing local CA `{}` (valid until {})",
        cert_path.display(),
        certificate.validity().not_after
    );
    Issuer::from_ca_cert_pem(&cert_pem, key).context("failed to parse local CA certificate")
}

fn leaf_is_reusable(
    cert_path: &Path,
    key_path: &Path,
    ca_cert_path: &Path,
    required_sans: &[SanType],
) -> bool {
    leaf_is_reusable_with_threshold(
        cert_path,
        key_path,
        ca_cert_path,
        required_sans,
        std::time::Duration::from_secs(30 * 24 * 60 * 60),
    )
}

pub fn leaf_is_reusable_with_threshold(
    cert_path: &Path,
    key_path: &Path,
    ca_cert_path: &Path,
    required_sans: &[SanType],
    renewal_window: std::time::Duration,
) -> bool {
    let result = (|| -> Result<bool> {
        if !cert_path.exists() || !key_path.exists() {
            return Ok(false);
        }
        validate_leaf_pair(cert_path, key_path)?;
        let leaf_pem = fs::read(cert_path)?;
        let ca_pem = fs::read(ca_cert_path)?;
        let (_, leaf_pem) = parse_x509_pem(&leaf_pem)?;
        let (_, ca_pem) = parse_x509_pem(&ca_pem)?;
        let (_, leaf) = x509_parser::certificate::X509Certificate::from_der(&leaf_pem.contents)?;
        let (_, ca) = x509_parser::certificate::X509Certificate::from_der(&ca_pem.contents)?;
        if leaf
            .validity()
            .time_to_expiration()
            .is_none_or(|remaining| remaining < renewal_window)
        {
            return Ok(false);
        }
        leaf.verify_signature(Some(ca.public_key()))?;
        let san = leaf
            .subject_alternative_name()?
            .ok_or_else(|| anyhow!("generated certificate has no SAN extension"))?;
        for required in required_sans {
            let found = match required {
                SanType::DnsName(name) => san.value.general_names.iter().any(|candidate| {
                    matches!(candidate, GeneralName::DNSName(value) if *value == name.as_str())
                }),
                SanType::IpAddress(ip) => san.value.general_names.iter().any(|candidate| {
                    matches!(candidate, GeneralName::IPAddress(value) if *value == ip_bytes(ip).as_slice())
                }),
                _ => false,
            };
            if !found {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    result.unwrap_or(false)
}

fn ip_bytes(ip: &IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    }
}

pub fn mint_leaf(
    issuer: &Issuer<'_, KeyPair>,
    san: &[String],
    common_name: &str,
    validity_days: u32,
) -> Result<MintedIdentity> {
    let sans = parse_sans(san)?;
    let key = KeyPair::generate().context("failed to generate server private key")?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.subject_alt_names = sans;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
    params.not_after = OffsetDateTime::now_utc() + Duration::days(i64::from(validity_days.max(1)));
    let cert = params
        .signed_by(&key, issuer)
        .context("failed to sign server certificate with the local CA")?;
    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::try_from(key.serialize_der())
        .map_err(|_| anyhow!("generated private key is not a supported DER key"))?;
    let expires_at = certificate_expires_at(cert_der.as_ref())?;
    let certified_key = certified_key_from_parts(vec![cert_der], key_der)?;
    info!(
        "minted new leaf certificate for SANs [{}] (valid until {})",
        san.join(", "),
        expires_at
    );
    Ok(MintedIdentity {
        certified_key,
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        expires_at,
    })
}

fn generate_leaf(
    issuer: &Issuer<'_, KeyPair>,
    cert_path: &Path,
    key_path: &Path,
    sans: Vec<SanType>,
    validity_days: u32,
) -> Result<()> {
    let san_strings: Result<Vec<_>> = sans
        .into_iter()
        .map(|san| match san {
            SanType::DnsName(name) => Ok(name.to_string()),
            SanType::IpAddress(ip) => Ok(ip.to_string()),
            _ => Err(anyhow!("unsupported SAN type")),
        })
        .collect();
    let minted = mint_leaf(issuer, &san_strings?, "TLS Proxy Server", validity_days)?;
    atomic_write(cert_path, minted.cert_pem.as_bytes(), false)?;
    atomic_write(key_path, minted.key_pem.as_bytes(), true)?;
    info!(
        "wrote local identity certificate `{}` and key `{}`",
        cert_path.display(),
        key_path.display()
    );
    Ok(())
}

fn validate_leaf_pair(cert_path: &Path, key_path: &Path) -> Result<()> {
    load_server_config(cert_path, key_path).map(|_| ())
}

pub fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open certificate `{}`", path.display()))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("failed to parse certificate `{}`", path.display()))?;
    if certs.is_empty() {
        bail!(
            "certificate file `{}` contains no certificates",
            path.display()
        );
    }
    Ok(certs)
}

pub fn read_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open private key `{}`", path.display()))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("failed to parse private key `{}`", path.display()))?
        .ok_or_else(|| anyhow!("private key file `{}` contains no key", path.display()))
}

fn certified_key_from_parts(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<CertifiedKey> {
    let fallback;
    let provider = match CryptoProvider::get_default() {
        Some(provider) => provider.as_ref(),
        None => {
            fallback = rustls::crypto::aws_lc_rs::default_provider();
            &fallback
        }
    };
    CertifiedKey::from_der(certs, key, provider)
        .map_err(|e| anyhow!("certificate and key do not form a valid TLS identity: {e}"))
}

fn atomic_write(path: &Path, contents: &[u8], private: bool) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("generated")
    ));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("failed to create `{}`", temporary.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write `{}`", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync `{}`", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace `{}`", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{prepare_local_identity, remaining_validity, LocalIdentityPaths};

    fn paths_in(directory: &Path) -> LocalIdentityPaths {
        LocalIdentityPaths {
            ca_cert: directory.join("CA.pem"),
            ca_key: directory.join("CA.key"),
            cert: directory.join("admin.pem"),
            key: directory.join("admin.key"),
        }
    }

    fn sans() -> Vec<String> {
        vec!["localhost".into(), "proxy.test".into(), "127.0.0.1".into()]
    }

    #[test]
    fn generates_and_reuses_ca_and_leaf() {
        let directory = tempdir().unwrap();
        let paths = paths_in(directory.path());
        let identity = prepare_local_identity(&paths, &sans()).unwrap();
        let ca_before = fs::read(&paths.ca_cert).unwrap();
        let leaf_before = fs::read(&identity.cert).unwrap();

        prepare_local_identity(&paths, &sans()).unwrap();

        assert_eq!(ca_before, fs::read(&paths.ca_cert).unwrap());
        assert_eq!(leaf_before, fs::read(&identity.cert).unwrap());
        assert!(paths.ca_key.exists());
        assert!(identity.key.exists());
    }

    #[test]
    fn changed_sans_regenerate_leaf() {
        let directory = tempdir().unwrap();
        let paths = paths_in(directory.path());
        let identity = prepare_local_identity(&paths, &sans()).unwrap();
        let leaf_before = fs::read(&identity.cert).unwrap();

        prepare_local_identity(&paths, &["another.test".to_string()]).unwrap();
        assert_ne!(leaf_before, fs::read(&identity.cert).unwrap());
    }

    #[test]
    fn generated_leaf_reports_remaining_validity() {
        let directory = tempdir().unwrap();
        let identity = prepare_local_identity(&paths_in(directory.path()), &sans()).unwrap();
        let remaining = remaining_validity(&identity.cert).unwrap();
        // generated with 365 days of validity; expect comfortably more than 300
        assert!(remaining > std::time::Duration::from_secs(300 * 24 * 60 * 60));
    }

    #[test]
    fn refuses_partial_ca_pair() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("CA.pem"), "not a certificate").unwrap();
        let error = prepare_local_identity(&paths_in(directory.path()), &sans()).unwrap_err();
        assert!(error.to_string().contains("local CA is incomplete"));
        assert!(!directory.path().join("CA.key").exists());
    }

    #[test]
    fn invalid_existing_ca_is_never_replaced() {
        let directory = tempdir().unwrap();
        let ca_cert = directory.path().join("CA.pem");
        let ca_key = directory.path().join("CA.key");
        fs::write(&ca_cert, "invalid certificate").unwrap();
        fs::write(&ca_key, "invalid key").unwrap();
        let cert_before = fs::read(&ca_cert).unwrap();
        let key_before = fs::read(&ca_key).unwrap();

        assert!(prepare_local_identity(&paths_in(directory.path()), &sans()).is_err());
        assert_eq!(fs::read(&ca_cert).unwrap(), cert_before);
        assert_eq!(fs::read(&ca_key).unwrap(), key_before);
    }
}
