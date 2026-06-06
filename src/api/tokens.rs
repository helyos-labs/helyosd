//! HTTP handlers for multi-user API token management and identity (`whoami`).

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use super::AppState as SharedState;
use super::auth;
use crate::adapters::state::{ApiTokenRecord, NewApiToken};

type AppStateExtractor = State<SharedState>;

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    name: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    ttl_secs: Option<i64>,
}

/// POST /api/v1/tokens — mint a named token. The secret is returned ONCE.
pub async fn create_token(
    State(state): AppStateExtractor,
    Json(req): Json<CreateTokenRequest>,
) -> impl IntoResponse {
    if req.name.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "token name must not be empty" })),
        )
            .into_response();
    }

    let token = auth::generate_api_token();
    let new = NewApiToken {
        name: req.name.clone(),
        token_hash: auth::hash_api_token(&token),
        token_prefix: auth::token_prefix(&token),
        // Admin-only in v1: stored but not enforced.
        scope: req.scope.unwrap_or_else(|| "admin".to_string()),
        expires_at: req
            .ttl_secs
            .filter(|s| *s > 0)
            .map(|s| (chrono::Utc::now() + chrono::Duration::seconds(s)).to_rfc3339()),
    };

    match state.token_store.create(new).await {
        Ok(rec) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": rec.id,
                "name": rec.name,
                "token": token,
                "scope": rec.scope,
                "created_at": rec.created_at,
                "expires_at": rec.expires_at,
            })),
        )
            .into_response(),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("UNIQUE") || msg.contains("constraint") {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                status,
                Json(serde_json::json!({ "error": format!("could not create token: {msg}") })),
            )
                .into_response()
        }
    }
}

/// GET /api/v1/tokens — list tokens (never the secret/hash).
pub async fn list_tokens(State(state): AppStateExtractor) -> impl IntoResponse {
    match state.token_store.list().await {
        Ok(tokens) => Json(tokens).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// DELETE /api/v1/tokens/{name} — soft-revoke.
pub async fn revoke_token(
    State(state): AppStateExtractor,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.token_store.revoke_by_name(&name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no active token named '{name}'") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /api/v1/version — unauthenticated reachability/TLS probe.
pub async fn version() -> impl IntoResponse {
    Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "api": "v1" }))
}

/// GET /api/v1/ca — unauthenticated. Returns the HTTP self-signed CA PEM and its
/// SHA-256 fingerprint so a client can pin trust.
pub async fn ca_cert(State(state): AppStateExtractor) -> impl IntoResponse {
    match &state.http_ca_pem {
        Some(pem) => {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(pem);
            let hexed = hex::encode(digest);
            let pretty = hexed
                .as_bytes()
                .chunks(2)
                .map(|c| std::str::from_utf8(c).unwrap())
                .collect::<Vec<_>>()
                .join(":");
            Json(serde_json::json!({
                "pem": String::from_utf8_lossy(pem),
                "sha256": pretty,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no self-signed CA (TLS off or BYO cert)" })),
        )
            .into_response(),
    }
}

/// GET /api/v1/whoami — identity of the calling token.
pub async fn whoami(req: Request) -> impl IntoResponse {
    if let Some(rec) = req.extensions().get::<ApiTokenRecord>() {
        Json(serde_json::json!({
            "name": rec.name,
            "scope": rec.scope,
            "created_at": rec.created_at,
            "expires_at": rec.expires_at,
            "last_used_at": rec.last_used_at,
        }))
        .into_response()
    } else {
        // Authenticated via legacy hash with no seeded row, or auth disabled.
        Json(serde_json::json!({ "name": auth::LEGACY_TOKEN_NAME, "scope": "admin" }))
            .into_response()
    }
}
