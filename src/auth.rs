use anyhow::{anyhow, bail, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserRecord {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub administrator: bool,
    #[serde(default)]
    pub created_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub token_hash: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub created_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub expires_at: Option<OffsetDateTime>,
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.len() < 12 {
        bail!("password must contain at least 12 characters");
    }
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt)
        .map_err(|cause| anyhow!("failed to encode password salt: {cause}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|cause| anyhow!("failed to hash password: {cause}"))?
        .to_string())
}

pub fn verify_password(encoded: &str, candidate: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(candidate.as_bytes(), &hash)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_round_trip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "incorrect password"));
        assert!(!hash.contains("correct horse"));
    }
}
