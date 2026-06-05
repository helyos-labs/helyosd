pub mod auth;
mod handlers;
pub mod routes;
pub mod tokens;

use std::sync::Arc;

use helyos_core::domain::orchestrator::OrchestratorHandle;
use helyos_core::ports::metrics::MetricsPort;
use helyos_core::ports::state::StateStore;
use tokio::sync::broadcast;

use crate::adapters::state::TokenStore;

/// Optional TLS material for the HTTP API (PEM bytes).
pub struct ApiTls {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

#[derive(Clone)]
pub struct AppState {
    pub handle: OrchestratorHandle,
    pub store: Arc<dyn StateStore>,
    pub token_store: Arc<TokenStore>,
    pub metrics: Arc<dyn MetricsPort>,
    pub event_tx: broadcast::Sender<ClusterEvent>,
    pub api_token_hash: Option<String>,
    /// PEM of the HTTP API's self-signed CA, served by `GET /api/v1/ca`.
    /// `None` when TLS is off or a BYO cert is used.
    pub http_ca_pem: Option<Vec<u8>>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ClusterEvent {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub name: String,
    pub action: String,
    pub message: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    handle: OrchestratorHandle,
    store: Arc<dyn StateStore>,
    token_store: Arc<TokenStore>,
    metrics: Arc<dyn MetricsPort>,
    event_tx: broadcast::Sender<ClusterEvent>,
    api_token_hash: Option<String>,
    addr: &str,
    api_tls: Option<ApiTls>,
    http_ca_pem: Option<Vec<u8>>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let state = AppState {
        handle,
        store,
        token_store,
        metrics,
        event_tx,
        api_token_hash,
        http_ca_pem,
    };
    let app = routes::build(state);

    match api_tls {
        None => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tracing::info!("helyosd API listening on http://{addr}");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        Some(tls) => {
            let config =
                axum_server::tls_rustls::RustlsConfig::from_pem(tls.cert_pem, tls.key_pem).await?;
            // Resolve `addr` (accepts both "ip:port" and "hostname:port"), unlike
            // a bare SocketAddr parse which requires a numeric IP.
            let socket = tokio::net::lookup_host(addr)
                .await?
                .next()
                .ok_or_else(|| anyhow::anyhow!("could not resolve bind address: {addr}"))?;
            let handle = axum_server::Handle::new();
            let h2 = handle.clone();
            tokio::spawn(async move {
                shutdown.await;
                h2.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
            });
            tracing::info!("helyosd API listening on https://{addr}");
            axum_server::bind_rustls(socket, config)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod serve_tls_tests {
    #[tokio::test]
    async fn rustls_config_from_generated_pem() {
        // Install a rustls crypto provider before any TLS operation.
        // When both aws-lc-rs and ring are in the dependency tree rustls
        // cannot auto-select one; we pick ring (already a transitive dep).
        let _ = rustls::crypto::ring::default_provider().install_default();

        let m = crate::cluster::tls::generate_ca_and_server_cert("helyos", &["127.0.0.1".into()]).unwrap();
        let cfg = axum_server::tls_rustls::RustlsConfig::from_pem(m.server_cert_pem, m.server_key_pem).await;
        assert!(cfg.is_ok(), "generated cert/key must load into rustls");
    }
}
