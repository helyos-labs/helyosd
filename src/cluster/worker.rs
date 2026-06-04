use std::sync::Arc;

use tonic::transport::Channel;
use tracing::{error, info};

use helyos_core::ports::runtime::ContainerRuntime;
use helyos_core::ports::state::StateStore;

use super::heartbeat;
use super::proto;
use super::proto::cluster_service_client::ClusterServiceClient;
use super::server::start_grpc_server;

/// Start the daemon in worker mode.
///
/// 1. Resolve hostname and system resources
/// 2. Connect to master gRPC and register this worker
/// 3. Start local gRPC server (receives pod assignments from master)
/// 4. Start heartbeat loop
/// 5. Wait for either task to finish
pub async fn start_worker(
    master_addr: String,
    token: String,
    listen_addr: String,
    runtime: Arc<dyn ContainerRuntime>,
    state: Arc<dyn StateStore>,
    tls_config: Option<tonic::transport::ClientTlsConfig>,
) -> anyhow::Result<()> {
    // 1. Collect hostname and system resources.
    let hostname = hostname::get()
        .map_err(|e| anyhow::anyhow!("failed to get hostname: {e}"))?
        .to_string_lossy()
        .to_string();
    let resources = heartbeat::collect_resources();

    info!(
        hostname = %hostname,
        cpu_cores = resources.cpu_cores,
        memory_bytes = resources.memory_bytes,
        "worker starting"
    );

    // 2. Register with master.
    let channel = if let Some(ref tls) = tls_config {
        info!("connecting to master with TLS");
        let endpoint = format!("https://{}", master_addr);
        Channel::from_shared(endpoint)
            .map_err(|e| anyhow::anyhow!("invalid endpoint: {e}"))?
            .tls_config(tls.clone())
            .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?
            .connect()
            .await?
    } else {
        info!("connecting to master without TLS");
        let endpoint = format!("http://{}", master_addr);
        Channel::from_shared(endpoint)
            .map_err(|e| anyhow::anyhow!("invalid endpoint: {e}"))?
            .connect()
            .await?
    };
    let mut client = ClusterServiceClient::new(channel);

    let register_req = proto::RegisterRequest {
        node_name: hostname.clone(),
        node_address: listen_addr.clone(),
        token: token.clone(),
        resources: Some(proto::ResourceInfo {
            cpu_cores: resources.cpu_cores,
            memory_bytes: resources.memory_bytes,
            cpu_available: resources.cpu_available,
            memory_available: resources.memory_available,
            running_pods: resources.running_pods,
        }),
    };

    let resp = client.register(register_req).await?.into_inner();
    if !resp.accepted {
        anyhow::bail!("master rejected registration: {}", resp.message);
    }

    let node_id: uuid::Uuid = resp
        .node_id
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid node_id from master: {e}"))?;

    info!(node_id = %node_id, "registered with master");

    // 3. Start local gRPC server for pod assignments.
    // We use the token hash as the shared secret for any callbacks from master.
    let token_hash = super::token::hash_token(&token);
    let grpc_state = Arc::clone(&state);
    let grpc_runtime = Arc::clone(&runtime);
    let grpc_addr = listen_addr.clone();
    let grpc_handle = tokio::spawn(async move {
        if let Err(e) =
            start_grpc_server(&grpc_addr, grpc_runtime, grpc_state, token_hash, None).await
        {
            error!(error = %e, "worker gRPC server failed");
        }
    });

    // 4. Start heartbeat loop.
    let hb_master = master_addr.clone();
    let hb_tls = tls_config.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(1);
        const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
        loop {
            match heartbeat::run_heartbeat_sender(hb_master.clone(), node_id, hb_tls.clone()).await
            {
                Ok(()) => {
                    backoff = std::time::Duration::from_secs(1);
                }
                Err(e) => {
                    error!(error = %e, backoff_secs = backoff.as_secs(), "heartbeat stream disconnected, reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    });

    // 5. Wait for either to finish.
    tokio::select! {
        res = grpc_handle => {
            match res {
                Err(e) => error!(error = %e, "gRPC server task panicked"),
                Ok(_) => info!("gRPC server exited"),
            }
        }
        res = heartbeat_handle => {
            match res {
                Err(e) => error!(error = %e, "heartbeat task panicked"),
                Ok(_) => info!("heartbeat task exited"),
            }
        }
    }

    Ok(())
}
