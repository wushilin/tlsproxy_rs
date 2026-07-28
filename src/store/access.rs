//! Administrative access records: users, sessions, retrieval tokens, and
//! the audit trail.

use super::*;

impl Store {
    pub fn save_retrieval_token(&self, token: &RetrievalToken) -> Result<()> {
        if token.id.trim().is_empty() || token.token_hash.len() != 64 {
            bail!("retrieval token ID and SHA-256 hash are required");
        }
        self.put_json(CF_TOKENS, token.id.as_bytes(), token)
    }

    pub fn retrieval_token_by_hash(&self, hash: &str) -> Result<Option<RetrievalToken>> {
        let cf = self.cf(CF_TOKENS)?;
        for item in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (_, value) = item?;
            let token: RetrievalToken = serde_json::from_slice(&value)?;
            if subtle::ConstantTimeEq::ct_eq(token.token_hash.as_bytes(), hash.as_bytes()).into() {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    pub fn retrieval_tokens(&self) -> Result<Vec<RetrievalToken>> {
        let cf = self.cf(CF_TOKENS)?;
        self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start).map(|item| {
            let (_, value) = item?;
            Ok(serde_json::from_slice(&value)?)
        }).collect()
    }

    pub fn delete_retrieval_token(&self, id: &str) -> Result<()> {
        self.db.delete_cf(&self.cf(CF_TOKENS)?, id.as_bytes())?;
        Ok(())
    }

    pub fn append_audit(&self, kind: &str, value: serde_json::Value) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let sequence = u64::try_from(now.unix_timestamp_nanos()).unwrap_or(u64::MAX);
        self.put_json(CF_AUDIT, &audit_key(kind, sequence), &serde_json::json!({
            "kind": kind, "timestamp": now, "detail": value
        }))
    }

    pub fn audits(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let cf = self.cf(CF_AUDIT)?;
        let mut values = self.db.iterator_cf(&cf, rocksdb::IteratorMode::End)
            .take(limit).map(|item| {
                let (_, value) = item?;
                Ok(serde_json::from_slice(&value)?)
            }).collect::<Result<Vec<_>>>()?;
        values.reverse();
        Ok(values)
    }

    pub fn user(&self, username: &str) -> Result<Option<UserRecord>> {
        self.get_json(CF_USERS, username.as_bytes())
    }

    pub async fn user_async(&self, username: String) -> Result<Option<UserRecord>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.user(&username))
            .await
            .context("user database task failed")?
    }

    pub fn save_user(&self, user: &UserRecord) -> Result<()> {
        if user.username.trim().is_empty() || user.password_hash.is_empty() {
            bail!("username and password hash are required");
        }
        self.put_json(CF_USERS, user.username.as_bytes(), user)
    }

    pub fn save_session(&self, session: &SessionRecord) -> Result<()> {
        if session.token_hash.is_empty() || session.username.is_empty() {
            bail!("session token hash and username are required");
        }
        self.put_json(CF_SESSIONS, session.token_hash.as_bytes(), session)
    }

    pub fn session(&self, token_hash: &str) -> Result<Option<SessionRecord>> {
        self.get_json(CF_SESSIONS, token_hash.as_bytes())
    }

    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        let cf = self.cf(CF_SESSIONS)?;
        self.db.delete_cf(&cf, token_hash.as_bytes())?;
        Ok(())
    }

    pub fn delete_user_sessions(&self, username: &str) -> Result<usize> {
        let cf = self.cf(CF_SESSIONS)?;
        let mut keys = Vec::new();
        for entry in self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, value) = entry?;
            let session: SessionRecord = serde_json::from_slice(&value)?;
            if session.username == username {
                keys.push(key.to_vec());
            }
        }
        let count = keys.len();
        let mut batch = WriteBatch::default();
        for key in keys {
            batch.delete_cf(&cf, key);
        }
        self.db.write(batch)?;
        Ok(count)
    }

    pub async fn save_session_async(&self, session: SessionRecord) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_session(&session))
            .await
            .context("session database task failed")?
    }

    pub async fn session_async(&self, token_hash: String) -> Result<Option<SessionRecord>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.session(&token_hash))
            .await
            .context("session database task failed")?
    }

    pub async fn delete_session_async(&self, token_hash: String) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.delete_session(&token_hash))
            .await
            .context("session database task failed")?
    }

}
