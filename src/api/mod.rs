pub mod auth;
mod handlers;
pub mod routes;

use std::sync::Arc;

use helyos_core::domain::orchestrator::OrchestratorHandle;
use helyos_core::ports::metrics::MetricsPort;
use helyos_core::ports::state::StateStore;
use tokio::sync::broadcast;

use crate::adapters::state::TokenStore;

#[derive(Clone)]
pub struct AppState {
    pub handle: OrchestratorHandle,
    pub store: Arc<dyn StateStore>,
    pub token_store: Arc<TokenStore>,
    pub metrics: Arc<dyn MetricsPort>,
    pub event_tx: broadcast::Sender<ClusterEvent>,
    pub api_token_hash: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ClusterEvent {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub name: String,
    pub action: String,
    pub message: String,
}

pub async fn serve(
    handle: OrchestratorHandle,
    store: Arc<dyn StateStore>,
    token_store: Arc<TokenStore>,
    metrics: Arc<dyn MetricsPort>,
    event_tx: broadcast::Sender<ClusterEvent>,
    api_token_hash: Option<String>,
    addr: &str,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let state = AppState {
        handle,
        store,
        token_store,
        metrics,
        event_tx,
        api_token_hash,
    };
    let app = routes::build(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("helyosd API listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
