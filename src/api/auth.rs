use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;

const TOKEN_PREFIX: &str = "nxa-api_";
const TOKEN_RANDOM_BYTES: usize = 32;

/// Length of the non-secret token prefix used as a DB lookup index:
/// `"nxa-api_"` (8 chars) + 4 hex chars = 12.
const TOKEN_PREFIX_LEN: usize = 12;

/// Name of the auto-seeded row representing the pre-existing single API token,
/// so it appears in `helyos auth token ls` and can be revoked.
pub const LEGACY_TOKEN_NAME: &str = "legacy-default";

/// Compute the non-secret lookup prefix for a token (its first 12 chars, or the
/// whole token if shorter). Stored in `api_tokens.token_prefix` and indexed so
/// auth needs a single Argon2 verify instead of scanning every row.
pub fn token_prefix(token: &str) -> String {
    token.chars().take(TOKEN_PREFIX_LEN).collect()
}

/// Seed a `legacy-default` row carrying the existing token's hash, but only if
/// the table is empty. We only have the hash (not the plaintext) for an
/// existing token, so the row uses a sentinel prefix and is matched via the
/// legacy fallback in [`require_bearer_token`], not the prefix index.
pub async fn seed_legacy_token_if_empty(
    token_store: &std::sync::Arc<crate::adapters::state::TokenStore>,
    hash: &str,
) {
    if token_store.count().await.unwrap_or(0) == 0 {
        let _ = token_store
            .create(crate::adapters::state::NewApiToken {
                name: LEGACY_TOKEN_NAME.to_string(),
                token_hash: hash.to_string(),
                token_prefix: "legacy".to_string(),
                scope: "admin".to_string(),
                expires_at: None,
            })
            .await;
    }
}

/// Generate a new API token: `"nxa-api_" + 64 hex chars` (32 random bytes).
pub fn generate_api_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{}{}", TOKEN_PREFIX, hex::encode(bytes))
}

/// Hash an API token using Argon2id. Returns the PHC-format hash string.
pub fn hash_api_token(token: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(token.as_bytes(), &salt)
        .expect("argon2 hashing should not fail")
        .to_string()
}

/// Verify an API token against an Argon2 hash string.
pub fn verify_api_token(token: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(token.as_bytes(), &parsed)
        .is_ok()
}

/// Axum middleware enforcing Bearer token auth against the multi-token store,
/// then the legacy single-token hash.
///
/// Resolution order:
/// 1. Prefix-indexed lookup in `api_tokens` → single Argon2 verify; rejects
///    expired rows; records the matched token in request extensions.
/// 2. Legacy `cluster_config.api_token_hash` fallback (gated on the
///    `legacy-default` row's revocation, if present).
/// 3. If no auth material is configured at all (`api_token_hash` is `None`),
///    the request passes through (dev/test; M3 adds a non-loopback guardrail).
pub async fn require_bearer_token(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    // 1. Multi-token store: one indexed candidate, one Argon2 verify.
    if let Some(ref token) = presented {
        let prefix = token_prefix(token);
        if let Ok(candidates) = state.token_store.find_active_by_prefix(&prefix).await {
            let now = chrono::Utc::now();
            for rec in candidates {
                if !rec.is_expired(now) && verify_api_token(token, &rec.token_hash) {
                    if should_touch(rec.last_used_at.as_deref(), now) {
                        let ts = state.token_store.clone();
                        let id = rec.id.clone();
                        tokio::spawn(async move {
                            let _ = ts.touch_last_used(&id).await;
                        });
                    }
                    req.extensions_mut().insert(rec);
                    return next.run(req).await;
                }
            }
        }
    }

    // 2. Legacy single-token fallback.
    if let Some(ref expected_hash) = state.api_token_hash {
        if let Some(ref token) = presented
            && verify_api_token(token, expected_hash)
        {
            // Honor revocation of the seeded legacy-default row.
            if let Ok(Some(rec)) = state.token_store.get_by_name(LEGACY_TOKEN_NAME).await {
                if rec.revoked_at.is_some() {
                    return unauthorized_response();
                }
                req.extensions_mut().insert(rec);
            }
            return next.run(req).await;
        }
        return unauthorized_response();
    }

    // 3. No auth configured — pass through.
    next.run(req).await
}

/// ~60s throttle for `last_used_at` writes, given the previous value.
fn should_touch(last_used_at: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    match last_used_at {
        None => true,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(prev) => (now - prev.with_timezone(&chrono::Utc)).num_seconds() >= 60,
            Err(_) => true,
        },
    }
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({"error": "missing or invalid bearer token"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_format() {
        let token = generate_api_token();
        assert!(
            token.starts_with(TOKEN_PREFIX),
            "token should start with '{TOKEN_PREFIX}'"
        );
        let hex_part = &token[TOKEN_PREFIX.len()..];
        assert_eq!(
            hex_part.len(),
            TOKEN_RANDOM_BYTES * 2,
            "hex part should be {} chars",
            TOKEN_RANDOM_BYTES * 2
        );
        assert!(
            hex::decode(hex_part).is_ok(),
            "hex part should be valid hex"
        );
    }

    #[test]
    fn generate_is_unique() {
        let t1 = generate_api_token();
        let t2 = generate_api_token();
        assert_ne!(t1, t2, "two generated tokens should differ");
    }

    #[test]
    fn hash_and_verify_roundtrip() {
        let token = generate_api_token();
        let hash = hash_api_token(&token);
        assert!(
            verify_api_token(&token, &hash),
            "token should verify against its own hash"
        );
    }

    #[test]
    fn rejects_wrong_token() {
        let token = generate_api_token();
        let hash = hash_api_token(&token);
        let wrong = generate_api_token();
        assert!(
            !verify_api_token(&wrong, &hash),
            "wrong token should not verify"
        );
    }

    #[test]
    fn rejects_bad_hash() {
        let token = generate_api_token();
        assert!(
            !verify_api_token(&token, "not-a-valid-hash"),
            "should return false for an unparseable hash"
        );
    }

    #[test]
    fn prefix_is_first_12_chars() {
        let token = "nxa-api_0123456789abcdef";
        assert_eq!(token_prefix(token), "nxa-api_0123");
        assert_eq!(token_prefix(token).len(), 12);
    }

    #[test]
    fn prefix_handles_short_input() {
        assert_eq!(token_prefix("abc"), "abc");
    }

    #[tokio::test]
    async fn seed_is_idempotent() {
        use crate::adapters::state::{SqliteStore, TokenStore};
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}/s.db?mode=rwc", dir.path().display());
        let sqlite = SqliteStore::connect(&url).await.unwrap();
        let ts = std::sync::Arc::new(TokenStore::new(sqlite.pool()));

        seed_legacy_token_if_empty(&ts, "hash-A").await;
        assert_eq!(ts.count().await.unwrap(), 1);
        let rec = ts.get_by_name(LEGACY_TOKEN_NAME).await.unwrap().unwrap();
        assert_eq!(rec.token_hash, "hash-A");

        // Second call must not add a second row.
        seed_legacy_token_if_empty(&ts, "hash-B").await;
        assert_eq!(ts.count().await.unwrap(), 1);
    }
}
