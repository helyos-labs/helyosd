//! Persistence for multi-user API tokens (helyosd-local; NOT a helyos-core port).
//!
//! Wraps the same SQLite pool as `SqliteStore`. This module is pure data
//! access — token generation, hashing, and verification live in
//! `crate::api::auth`.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use uuid::Uuid;

/// A row of the `api_tokens` table. `token_hash` is never serialized so it can
/// never leak through the list endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiTokenRecord {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub token_hash: String,
    pub token_prefix: String,
    pub scope: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

impl ApiTokenRecord {
    fn from_row(row: &SqliteRow) -> Self {
        Self {
            id: row.get("id"),
            name: row.get("name"),
            token_hash: row.get("token_hash"),
            token_prefix: row.get("token_prefix"),
            scope: row.get("scope"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            revoked_at: row.get("revoked_at"),
        }
    }

    /// True when `expires_at` is set and at or before `now`. An unparseable
    /// timestamp is treated as non-expiring (fail-open on data corruption
    /// rather than locking the operator out).
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        match &self.expires_at {
            None => false,
            Some(s) => match DateTime::parse_from_rfc3339(s) {
                Ok(exp) => exp.with_timezone(&Utc) <= now,
                Err(_) => false,
            },
        }
    }
}

/// Parameters for creating a token row. The plaintext token is hashed by the
/// caller; only the hash and the non-secret prefix are persisted.
pub struct NewApiToken {
    pub name: String,
    pub token_hash: String,
    pub token_prefix: String,
    pub scope: String,
    pub expires_at: Option<String>,
}

#[derive(Clone)]
pub struct TokenStore {
    pool: SqlitePool,
}

impl TokenStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new token row and return the persisted record. Fails with a
    /// UNIQUE-violation error if `name` already exists.
    pub async fn create(&self, t: NewApiToken) -> sqlx::Result<ApiTokenRecord> {
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO api_tokens \
             (id, name, token_hash, token_prefix, scope, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&t.name)
        .bind(&t.token_hash)
        .bind(&t.token_prefix)
        .bind(&t.scope)
        .bind(&created_at)
        .bind(&t.expires_at)
        .execute(&self.pool)
        .await?;

        Ok(ApiTokenRecord {
            id,
            name: t.name,
            token_hash: t.token_hash,
            token_prefix: t.token_prefix,
            scope: t.scope,
            created_at,
            expires_at: t.expires_at,
            last_used_at: None,
            revoked_at: None,
        })
    }

    /// All tokens, newest first.
    pub async fn list(&self) -> sqlx::Result<Vec<ApiTokenRecord>> {
        let rows = sqlx::query("SELECT * FROM api_tokens ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(ApiTokenRecord::from_row).collect())
    }

    /// Active (non-revoked) candidate rows matching a token prefix. Usually 0
    /// or 1 rows; the caller does the Argon2 verify to confirm the real match.
    pub async fn find_active_by_prefix(&self, prefix: &str) -> sqlx::Result<Vec<ApiTokenRecord>> {
        let rows =
            sqlx::query("SELECT * FROM api_tokens WHERE token_prefix = ? AND revoked_at IS NULL")
                .bind(prefix)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(ApiTokenRecord::from_row).collect())
    }

    /// Look up a single token by its unique name (in any state).
    pub async fn get_by_name(&self, name: &str) -> sqlx::Result<Option<ApiTokenRecord>> {
        let row = sqlx::query("SELECT * FROM api_tokens WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(ApiTokenRecord::from_row))
    }

    /// Soft-revoke by name. Returns true if an active row was updated.
    pub async fn revoke_by_name(&self, name: &str) -> sqlx::Result<bool> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE api_tokens SET revoked_at = ? WHERE name = ? AND revoked_at IS NULL",
        )
        .bind(&now)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Update the last-used timestamp (best-effort; callers fire-and-forget).
    pub async fn touch_last_used(&self, id: &str) -> sqlx::Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Number of token rows (used to decide the one-time legacy auto-seed).
    pub async fn count(&self) -> sqlx::Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM api_tokens")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::state::SqliteStore;

    async fn store() -> (TokenStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}/t.db?mode=rwc", dir.path().display());
        let sqlite = SqliteStore::connect(&url).await.unwrap();
        (TokenStore::new(sqlite.pool()), dir)
    }

    fn sample(name: &str, prefix: &str) -> NewApiToken {
        NewApiToken {
            name: name.to_string(),
            token_hash: "argon2-hash".to_string(),
            token_prefix: prefix.to_string(),
            scope: "admin".to_string(),
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn create_list_find_revoke_roundtrip() {
        let (ts, _d) = store().await;
        assert_eq!(ts.count().await.unwrap(), 0);

        let rec = ts.create(sample("ci", "nxa-api_abcd")).await.unwrap();
        assert_eq!(rec.name, "ci");
        assert!(rec.revoked_at.is_none());
        assert_eq!(ts.count().await.unwrap(), 1);

        let found = ts.find_active_by_prefix("nxa-api_abcd").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "ci");

        assert!(ts.revoke_by_name("ci").await.unwrap());
        assert!(
            ts.find_active_by_prefix("nxa-api_abcd")
                .await
                .unwrap()
                .is_empty(),
            "revoked rows must not be returned as active candidates"
        );
        assert!(
            !ts.revoke_by_name("ci").await.unwrap(),
            "second revoke is a no-op"
        );
    }

    #[tokio::test]
    async fn duplicate_name_fails() {
        let (ts, _d) = store().await;
        ts.create(sample("dup", "p1")).await.unwrap();
        let err = ts.create(sample("dup", "p2")).await;
        assert!(err.is_err(), "UNIQUE(name) must reject duplicates");
    }

    #[tokio::test]
    async fn expiry_check() {
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let mut rec = ApiTokenRecord {
            id: "i".into(),
            name: "n".into(),
            token_hash: "h".into(),
            token_prefix: "p".into(),
            scope: "admin".into(),
            created_at: Utc::now().to_rfc3339(),
            expires_at: Some(past),
            last_used_at: None,
            revoked_at: None,
        };
        assert!(rec.is_expired(Utc::now()));
        rec.expires_at = None;
        assert!(!rec.is_expired(Utc::now()));
    }
}
