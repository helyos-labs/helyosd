use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;

const TOKEN_PREFIX: &str = "nxa-api_";
const TOKEN_RANDOM_BYTES: usize = 32;

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

/// Axum middleware that enforces Bearer token authentication.
///
/// - If `state.api_token_hash` is `None`, the request passes through (no auth configured).
/// - Otherwise, the `Authorization: Bearer <token>` header is required and verified.
pub async fn require_bearer_token(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(ref expected_hash) = state.api_token_hash else {
        // No auth configured — pass through.
        return next.run(req).await;
    };

    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let Some(header_value) = auth_header else {
        return unauthorized_response();
    };

    let Some(token) = header_value.strip_prefix("Bearer ") else {
        return unauthorized_response();
    };

    if verify_api_token(token, expected_hash) {
        next.run(req).await
    } else {
        unauthorized_response()
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
}
