use std::fs::{self, OpenOptions};
use std::io::{BufReader, Write};
use std::net::IpAddr;
use std::path::Path;
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
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::FromDer;

const CA_VALIDITY_DAYS: i64 = 3650;

pub struct MintedIdentity {
    pub certified_key: CertifiedKey,
    pub cert_pem: String,
    pub key_pem: String,
    pub expires_at: OffsetDateTime,
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
        sans.push(SanType::DnsName(
            "localhost"
                .try_into()
                .map_err(|_| anyhow!("`localhost` was rejected as a certificate DNS name"))?,
        ));
    }
    Ok(sans)
}

pub fn certificate_expires_at(cert_der: &[u8]) -> Result<OffsetDateTime> {
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .context("failed to parse X.509 certificate")?;
    let timestamp = certificate.validity().not_after.timestamp();
    OffsetDateTime::from_unix_timestamp(timestamp)
        .context("certificate has an unsupported expiration timestamp")
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


/// Appends CA certificates to a served chain, skipping any already present,
/// so clients receive the full chain rather than a bare leaf.
pub fn extend_chain(key: &mut CertifiedKey, ca_chain: &[CertificateDer<'static>]) {
    for ca in ca_chain {
        if !key.cert.contains(ca) {
            key.cert.push(ca.clone());
        }
    }
}

pub fn mint_leaf(
    issuer: &Issuer<'_, KeyPair>,
    ca_chain: &[CertificateDer<'static>],
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
    let mut certified_key = certified_key_from_parts(vec![cert_der], key_der)?;
    extend_chain(&mut certified_key, ca_chain);
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

pub(crate) fn certified_key_from_parts(
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
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generates_and_reuses_ca() {
        let directory = tempdir().unwrap();
        let cert = directory.path().join("CA.pem");
        let key = directory.path().join("CA-key.pem");
        let first = load_or_create_ca(&cert, &key).unwrap();
        let first_pem = std::fs::read(&cert).unwrap();
        drop(first);
        load_or_create_ca(&cert, &key).unwrap();
        assert_eq!(std::fs::read(&cert).unwrap(), first_pem, "existing CA is reused, not regenerated");
    }

    #[test]
    fn refuses_partial_ca_pair() {
        let directory = tempdir().unwrap();
        let cert = directory.path().join("CA.pem");
        let key = directory.path().join("CA-key.pem");
        load_or_create_ca(&cert, &key).unwrap();
        std::fs::remove_file(&key).unwrap();
        assert!(load_or_create_ca(&cert, &key).is_err(), "cert without key must not silently regenerate");
    }

    #[test]
    fn minted_leaf_covers_requested_sans_and_expiry() {
        let directory = tempdir().unwrap();
        let issuer = load_or_create_ca(&directory.path().join("CA.pem"), &directory.path().join("CA-key.pem")).unwrap();
        let minted = mint_leaf(&issuer, &[], &["a.example".into(), "127.0.0.1".into()], "test", 30).unwrap();
        assert_eq!(minted.certified_key.cert.len(), 1);
        assert!(minted.expires_at > time::OffsetDateTime::now_utc());
        let der = minted.certified_key.cert[0].as_ref();
        let (_, parsed) = x509_parser::parse_x509_certificate(der).unwrap();
        let text = format!("{:?}", parsed.extensions());
        assert!(text.contains("a.example"));
    }
}
