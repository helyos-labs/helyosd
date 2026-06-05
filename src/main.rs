use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

use helyos_core::domain::models::*;
use helyos_core::domain::orchestrator::Orchestrator;
use helyos_core::ports::cluster::ClusterTransport;
use helyos_core::ports::dns::DnsProvider;
use helyos_core::ports::metrics::MetricsPort;
use helyos_core::ports::runtime::ContainerRuntime;
use helyos_core::ports::secrets::SecretStore;
use helyos_core::ports::state::StateStore;

/// Capacity of the broadcast channel that fans cluster events out to SSE
/// subscribers. Slow consumers that fall this far behind are lagged (dropped),
/// not blocking the producer.
const CLUSTER_EVENT_CHANNEL_CAPACITY: usize = 256;

fn default_data_dir() -> String {
    dirs::home_dir()
        .map(|h| h.join(".helyos").join("data"))
        .unwrap_or_else(|| PathBuf::from("/var/lib/helyos"))
        .to_string_lossy()
        .into_owned()
}

fn default_proxy_config_dir() -> String {
    let mut p = PathBuf::from(default_data_dir());
    p.push("proxy");
    p.to_string_lossy().into_owned()
}

#[derive(Parser)]
#[command(name = "helyosd", about = "Helyos daemon", version)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value = "6443")]
    port: u16,

    #[arg(long, default_value_t = default_data_dir())]
    data_dir: String,

    /// Node mode: single, master, or worker
    #[arg(long, default_value = "single")]
    mode: String,

    /// Master address to join (worker mode only)
    #[arg(long)]
    join: Option<String>,

    /// Join token (worker mode only)
    #[arg(long)]
    token: Option<String>,

    /// gRPC listen port (master and worker modes)
    #[arg(long, default_value = "6444")]
    grpc_port: u16,

    /// DNS mode: "noop" for single-node (Docker DNS), "embedded" for multi-node
    #[arg(long, default_value = "noop")]
    dns_mode: String,

    /// IP address of this node (used for container DNS config in embedded mode)
    #[arg(long)]
    master_ip: Option<String>,

    /// DNS listen address for embedded DNS server
    #[arg(long, default_value = "127.0.0.1:15353")]
    dns_listen: String,

    /// Upstream DNS server for forwarding non-.internal queries
    #[arg(long, default_value = "8.8.8.8:53")]
    dns_upstream: String,

    /// Proxy backend: "nginx", "caddy", "traefik"
    #[arg(long, default_value = "traefik")]
    proxy_backend: String,

    /// Proxy config directory
    #[arg(long, default_value_t = default_proxy_config_dir())]
    proxy_config_dir: String,

    /// ACME email for automatic TLS
    #[arg(long)]
    acme_email: Option<String>,

    /// Cluster CIDR for overlay network
    #[arg(long, default_value = "172.20.0.0/16")]
    cluster_cidr: String,

    /// WireGuard listen port
    #[arg(long, default_value = "51820")]
    wg_port: u16,

    /// Enable overlay network
    #[arg(long)]
    overlay: bool,

    /// API bearer token (or set HELYOS_API_TOKEN env var)
    #[arg(long, env = "HELYOS_API_TOKEN")]
    api_token: Option<String>,

    /// Container runtime to use: docker, containerd, or auto
    #[arg(long, default_value = "auto")]
    runtime: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.mode.as_str() {
        "single" => start_single_node(&cli).await,
        "master" => start_master(&cli).await,
        "worker" => start_worker(&cli).await,
        other => anyhow::bail!("unknown mode: {other}. Use: single, master, or worker"),
    }
}

// ────────────────────── shared helpers ──────────────────────

/// Initialise the data directory, SQLite state store, and container runtime.
/// Returns (data_dir, state, token_store, runtime).
async fn init_infrastructure(
    cli: &Cli,
) -> anyhow::Result<(
    PathBuf,
    Arc<dyn StateStore>,
    Arc<helyosd::adapters::state::TokenStore>,
    Arc<dyn ContainerRuntime>,
)> {
    use helyosd::adapters::runtime::{RuntimeDetector, RuntimeKind};

    std::fs::create_dir_all(&cli.data_dir)?;

    let data_dir = PathBuf::from(&cli.data_dir);

    let db_path = format!("{}/helyos.db", cli.data_dir);
    let database_url = format!("sqlite:{}?mode=rwc", db_path);
    let sqlite = helyosd::adapters::state::SqliteStore::connect(&database_url).await?;
    let token_store = Arc::new(helyosd::adapters::state::TokenStore::new(sqlite.pool()));
    let store: Arc<dyn StateStore> = Arc::new(sqlite);
    info!(path = db_path, "state store initialized");

    let kind: RuntimeKind = cli
        .runtime
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let resolved = RuntimeDetector::resolve(kind).unwrap_or(RuntimeKind::Docker);
    let runtime = RuntimeDetector::build(resolved, &cli.data_dir).await?;
    info!(
        runtime = runtime.runtime_name(),
        "container runtime initialized"
    );

    Ok((data_dir, store, token_store, runtime))
}

/// Load or generate the master encryption key and create the encrypted secret
/// store.  Returns the store **and** the raw master key so that other
/// subsystems (e.g. TLS certificate storage) can reuse it.
fn init_secrets(cli: &Cli, data_dir: &Path) -> anyhow::Result<(Arc<dyn SecretStore>, [u8; 32])> {
    let master_key = helyosd::crypto::master_key::load_or_generate(data_dir)?;
    info!("master key loaded");

    let secret_conn = rusqlite::Connection::open(format!("{}/secrets.db", cli.data_dir))
        .map_err(|e| anyhow::anyhow!("failed to open secrets db: {e}"))?;
    let secret_store: Arc<dyn SecretStore> = Arc::new(
        helyosd::adapters::secrets::EncryptedSqliteSecretStore::new(secret_conn, &master_key)?,
    );
    info!("secret store initialized");

    Ok((secret_store, master_key))
}

/// Initialise the proxy backend and SQLite-backed route store.
fn init_proxy(
    cli: &Cli,
) -> anyhow::Result<(
    Arc<dyn helyos_core::ports::proxy::ProxyBackend>,
    Arc<dyn helyos_core::ports::route_store::RouteStore>,
)> {
    use helyosd::adapters::proxy::{CaddyBackend, NginxBackend, TraefikBackend};
    use helyosd::adapters::state::SqliteRouteStore;

    std::fs::create_dir_all(&cli.proxy_config_dir)?;

    let proxy: Arc<dyn helyos_core::ports::proxy::ProxyBackend> = match cli.proxy_backend.as_str() {
        "nginx" => Arc::new(NginxBackend::new(
            PathBuf::from(&cli.proxy_config_dir),
            "nginx".into(),
        )),
        "caddy" => {
            let caddyfile = PathBuf::from(&cli.proxy_config_dir).join("Caddyfile");
            Arc::new(CaddyBackend::new(caddyfile, "http://localhost:2019".into()))
        }
        // traefik is the default backend (handles "traefik" and any unknown value)
        _ => {
            let config_path = PathBuf::from(&cli.proxy_config_dir).join("helyos-dynamic.yml");
            Arc::new(TraefikBackend::new(config_path))
        }
    };

    let route_db_path = format!("{}/routes.db", cli.data_dir);
    let route_conn = rusqlite::Connection::open(&route_db_path)
        .map_err(|e| anyhow::anyhow!("failed to open routes db: {e}"))?;
    let route_store: Arc<dyn helyos_core::ports::route_store::RouteStore> =
        Arc::new(SqliteRouteStore::new(route_conn)?);

    info!(backend = %cli.proxy_backend, path = route_db_path, "proxy backend and route store initialized");
    Ok((proxy, route_store))
}

/// Spawn the orchestrator together with its health checker and event watcher.
#[allow(clippy::too_many_arguments)]
fn spawn_orchestrator(
    runtime: &Arc<dyn ContainerRuntime>,
    store: &Arc<dyn StateStore>,
    secret_store: Arc<dyn SecretStore>,
    dns: Option<Arc<dyn DnsProvider>>,
    master_ip: Option<String>,
    proxy: Option<Arc<dyn helyos_core::ports::proxy::ProxyBackend>>,
    route_store: Option<Arc<dyn helyos_core::ports::route_store::RouteStore>>,
    metrics: Option<Arc<dyn MetricsPort>>,
    event_tx: tokio::sync::broadcast::Sender<helyosd::api::ClusterEvent>,
) -> helyos_core::domain::orchestrator::OrchestratorHandle {
    let transport: Arc<dyn ClusterTransport> = Arc::new(
        helyosd::adapters::transport::LocalTransport::new(Arc::clone(runtime)),
    );
    let handle = Orchestrator::spawn(
        Arc::clone(runtime),
        Some(Arc::clone(store)),
        Some(secret_store),
        Some(transport),
        dns,
        master_ip,
        proxy,
        route_store,
        metrics.clone(),
    );

    // Spawn health checker background task
    match helyosd::adapters::health::HealthChecker::new(handle.clone()) {
        Ok(checker) => {
            let health_checker = Arc::new(checker);
            tokio::spawn(async move { health_checker.run().await });
            info!("health checker started");
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to build health checker HTTP client, health checks disabled");
        }
    }

    // Start container event watcher
    helyosd::adapters::event_watcher::spawn_event_watcher(
        Arc::clone(runtime),
        handle.command_sender(),
        metrics,
        Some(event_tx),
    );
    info!("container event watcher started");

    handle
}

/// Initialise the DNS provider based on --dns-mode CLI flag.
async fn init_dns(cli: &Cli) -> anyhow::Result<(Option<Arc<dyn DnsProvider>>, Option<String>)> {
    match cli.dns_mode.as_str() {
        "embedded" => {
            let listen_addr: std::net::SocketAddr = cli
                .dns_listen
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --dns-listen address: {e}"))?;
            let upstream_addr: std::net::SocketAddr = cli
                .dns_upstream
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --dns-upstream address: {e}"))?;

            let provider =
                helyosd::adapters::dns::HickoryDnsProvider::new(listen_addr, upstream_addr);
            provider.start().await?;
            info!(listen = %cli.dns_listen, upstream = %cli.dns_upstream, "embedded DNS server started");

            let master_ip = cli.master_ip.clone();
            Ok((Some(Arc::new(provider) as Arc<dyn DnsProvider>), master_ip))
        }
        _ => {
            info!("using noop DNS (single-node, containers use Docker DNS)");
            Ok((None, None))
        }
    }
}

/// Initialise the API bearer token.
///
/// - If `--api-token` is provided on the CLI (or via `HELYOS_API_TOKEN` env):
///   hash it, persist the hash, and return the hash.
/// - Else if a hash already exists in the store: load and return it.
/// - Else: generate a fresh token, hash it, persist, log the token once, and
///   return the hash.
async fn init_api_token(
    cli: &Cli,
    store: &Arc<dyn StateStore>,
    token_store: &Arc<helyosd::adapters::state::TokenStore>,
) -> anyhow::Result<Option<String>> {
    use helyosd::api::auth;

    let hash = if let Some(ref token) = cli.api_token {
        let hash = auth::hash_api_token(token);
        store.set_cluster_config("api_token_hash", &hash).await?;
        info!("API token hash stored (token provided via CLI/env)");
        hash
    } else if let Some(hash) = store.get_cluster_config("api_token_hash").await? {
        info!("loaded existing API token hash from store");
        hash
    } else {
        // No token configured and none stored — generate a new one.
        let token = auth::generate_api_token();
        let hash = auth::hash_api_token(&token);
        store.set_cluster_config("api_token_hash", &hash).await?;
        info!("Generated new API token — save this, it will not be shown again:");
        info!("  HELYOS_API_TOKEN={token}");
        hash
    };

    // Make the pre-existing single token visible/revocable as a named row.
    auth::seed_legacy_token_if_empty(token_store, &hash).await;

    Ok(Some(hash))
}

/// Create a cancellation token and spawn a task that cancels it on SIGINT or
/// SIGTERM.  Returns the token so callers can derive `.cancelled()` futures.
fn spawn_shutdown_handler() -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        info!("shutdown signal received — stopping gracefully");
        t.cancel();
    });
    token
}

// ────────────────────── single-node mode ──────────────────────

async fn start_single_node(cli: &Cli) -> anyhow::Result<()> {
    info!(
        "starting helyosd in single-node mode on {}:{}",
        cli.host, cli.port
    );

    let (data_dir, store, token_store, runtime) = init_infrastructure(cli).await?;
    let (secret_store, master_key) = init_secrets(cli, &data_dir)?;
    let (dns, master_ip) = init_dns(cli).await?;
    let (proxy, route_store) = init_proxy(cli)?;
    let metrics: Arc<dyn MetricsPort> =
        Arc::new(helyosd::adapters::metrics::PrometheusMetrics::new());
    let (event_tx, _) = tokio::sync::broadcast::channel::<helyosd::api::ClusterEvent>(
        CLUSTER_EVENT_CHANNEL_CAPACITY,
    );
    let handle = spawn_orchestrator(
        &runtime,
        &store,
        secret_store,
        dns,
        master_ip,
        Some(Arc::clone(&proxy)),
        Some(Arc::clone(&route_store)),
        Some(metrics.clone()),
        event_tx.clone(),
    );

    if let Some(ref email) = cli.acme_email {
        let acme = Arc::new(helyosd::adapters::tls::AcmeManager::new(
            email,
            Arc::clone(&route_store),
            false,
            &master_key,
        ));
        helyosd::adapters::tls::spawn_renewal_task(
            Arc::clone(&route_store),
            acme,
            std::time::Duration::from_secs(86400),
            30,
        );
        info!(email, "TLS auto-renewal enabled");
    }

    let api_token_hash = init_api_token(cli, &store, &token_store).await?;
    let shutdown = spawn_shutdown_handler();

    let addr = format!("{}:{}", cli.host, cli.port);
    helyosd::api::serve(
        handle,
        Arc::clone(&store),
        Arc::clone(&token_store),
        metrics,
        event_tx.clone(),
        api_token_hash,
        &addr,
        shutdown.cancelled_owned(),
    )
    .await
}

// ────────────────────── master mode ──────────────────────

async fn start_master(cli: &Cli) -> anyhow::Result<()> {
    info!(
        "starting helyosd in master mode on {}:{} (gRPC {})",
        cli.host, cli.port, cli.grpc_port
    );

    let (data_dir, store, token_store, runtime) = init_infrastructure(cli).await?;
    let (secret_store, master_key) = init_secrets(cli, &data_dir)?;
    let (dns, master_ip) = init_dns(cli).await?;
    let (proxy, route_store) = init_proxy(cli)?;
    let metrics: Arc<dyn MetricsPort> =
        Arc::new(helyosd::adapters::metrics::PrometheusMetrics::new());
    let (event_tx, _) = tokio::sync::broadcast::channel::<helyosd::api::ClusterEvent>(
        CLUSTER_EVENT_CHANNEL_CAPACITY,
    );
    let handle = spawn_orchestrator(
        &runtime,
        &store,
        secret_store,
        dns,
        master_ip,
        Some(Arc::clone(&proxy)),
        Some(Arc::clone(&route_store)),
        Some(metrics.clone()),
        event_tx.clone(),
    );

    if let Some(ref email) = cli.acme_email {
        let acme = Arc::new(helyosd::adapters::tls::AcmeManager::new(
            email,
            Arc::clone(&route_store),
            false,
            &master_key,
        ));
        helyosd::adapters::tls::spawn_renewal_task(
            Arc::clone(&route_store),
            acme,
            std::time::Duration::from_secs(86400),
            30,
        );
        info!(email, "TLS auto-renewal enabled");
    }

    // Register self as a master node.
    let hostname = hostname::get()
        .map_err(|e| anyhow::anyhow!("failed to get hostname: {e}"))?
        .to_string_lossy()
        .to_string();
    let resources = helyosd::cluster::heartbeat::collect_resources();
    let master_node = Node::new(
        hostname.clone(),
        format!("{}:{}", cli.host, cli.grpc_port),
        NodeRole::Master,
        resources,
    );
    let _ = store.insert_node(&master_node).await;
    info!(node_id = %master_node.id, name = %hostname, "master node registered");

    // Generate or load the join token.
    let token_hash = match store.get_cluster_config("join_token_hash").await? {
        Some(hash) => {
            info!("loaded existing join token from cluster config");
            hash
        }
        None => {
            let token = helyosd::cluster::token::generate_token();
            let hash = helyosd::cluster::token::hash_token(&token);
            store.set_cluster_config("join_token_hash", &hash).await?;
            info!("join token generated — workers can join with:");
            info!(
                "  helyosd --mode worker --join {}:{} --token {}",
                cli.host, cli.grpc_port, token
            );
            hash
        }
    };

    // Generate or load self-signed TLS certificates for gRPC.
    let grpc_tls_certs = helyosd::cluster::tls::load_or_generate(&data_dir)?;
    let server_tls_config = grpc_tls_certs.server_tls_config()?;

    // Start gRPC server as background task.
    let grpc_addr = format!("{}:{}", cli.host, cli.grpc_port);
    let grpc_runtime = Arc::clone(&runtime);
    let grpc_state = Arc::clone(&store);
    let grpc_token_hash = token_hash.clone();
    tokio::spawn(async move {
        if let Err(e) = helyosd::cluster::server::start_grpc_server(
            &grpc_addr,
            grpc_runtime,
            grpc_state,
            grpc_token_hash,
            Some(server_tls_config),
        )
        .await
        {
            tracing::error!(error = %e, "gRPC cluster server failed");
        }
    });

    // Start heartbeat monitor as background task.
    let hb_state = Arc::clone(&store);
    let reschedule_handle = handle.clone();
    let reschedule_store = Arc::clone(&store);
    let reschedule: helyosd::cluster::heartbeat::RescheduleFn = Arc::new(move |node_id, pods| {
        use helyos_core::domain::models::PodStatus;

        tracing::warn!(
            node_id = %node_id,
            pod_count = pods.len(),
            "dead node — rescheduling pods"
        );

        // Collect unique deployment identifiers from the affected pods.
        let mut seen_deployments = std::collections::HashSet::new();
        let mut deployments_to_reschedule: Vec<(String, String)> = Vec::new();
        for pod in &pods {
            let key = (pod.project.clone(), pod.deployment_name.clone());
            if seen_deployments.insert(key.clone()) {
                deployments_to_reschedule.push(key);
            }
        }

        let handle = reschedule_handle.clone();
        let store = Arc::clone(&reschedule_store);

        // Clone the pods so we can move them into the async task.
        let dead_pods = pods;

        // The callback is synchronous, so spawn an async task to perform
        // the actual rescheduling via the orchestrator.
        tokio::spawn(async move {
            // 1. Mark all pods from the dead node as Failed in the state store.
            for pod in &dead_pods {
                let mut pod_copy = pod.clone();
                if pod_copy.status == PodStatus::Failed {
                    continue; // already marked
                }
                pod_copy.status = PodStatus::Failed;
                pod_copy.node_id = None;
                if let Err(e) = store.update_pod(&pod_copy).await {
                    tracing::error!(
                        pod_id = %pod_copy.id,
                        error = %e,
                        "failed to mark pod as Failed after node death"
                    );
                } else {
                    tracing::info!(
                        pod_id = %pod_copy.id,
                        project = %pod_copy.project,
                        deployment = %pod_copy.deployment_name,
                        "marked pod as Failed (node dead)"
                    );
                }
            }

            // 2. For each affected deployment, trigger a redeploy so the
            //    orchestrator reconciles and places new pods on healthy nodes.
            for (project, name) in &deployments_to_reschedule {
                // Fetch the current deployment to get its spec.
                let deployments = handle.list_deployments(Some(project.clone())).await;
                let deployment = match deployments.iter().find(|d| d.name() == name) {
                    Some(d) => d,
                    None => {
                        tracing::warn!(
                            project = %project,
                            deployment = %name,
                            "deployment not found during reschedule — skipping"
                        );
                        continue;
                    }
                };

                tracing::info!(
                    project = %project,
                    deployment = %name,
                    replicas = deployment.spec.replicas,
                    "redeploying after node death"
                );

                match handle.deploy(deployment.spec.clone()).await {
                    Ok(_) => {
                        tracing::info!(
                            project = %project,
                            deployment = %name,
                            "reschedule deploy succeeded"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            project = %project,
                            deployment = %name,
                            error = %e,
                            "reschedule deploy failed"
                        );
                    }
                }
            }
        });
    });
    tokio::spawn(async move {
        helyosd::cluster::heartbeat::run_monitor(hb_state, reschedule).await;
    });
    info!("heartbeat monitor started");

    let api_token_hash = init_api_token(cli, &store, &token_store).await?;
    let shutdown = spawn_shutdown_handler();

    // Start the HTTP API (blocks until shutdown signal).
    let addr = format!("{}:{}", cli.host, cli.port);
    helyosd::api::serve(
        handle,
        Arc::clone(&store),
        Arc::clone(&token_store),
        metrics,
        event_tx.clone(),
        api_token_hash,
        &addr,
        shutdown.cancelled_owned(),
    )
    .await
}

// ────────────────────── worker mode ──────────────────────

async fn start_worker(cli: &Cli) -> anyhow::Result<()> {
    let master_addr = cli
        .join
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--join is required in worker mode"))?
        .to_string();
    let token = cli
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--token is required in worker mode"))?
        .to_string();

    info!(
        "starting helyosd in worker mode, joining master at {}",
        master_addr
    );

    std::fs::create_dir_all(&cli.data_dir)?;

    // Worker gets its own local state store and runtime (respects --runtime flag).
    let db_path = format!("{}/helyos.db", cli.data_dir);
    let database_url = format!("sqlite:{}?mode=rwc", db_path);
    let store = helyosd::adapters::state::SqliteStore::connect(&database_url).await?;
    let store: Arc<dyn StateStore> = Arc::new(store);
    info!(path = db_path, "worker state store initialized");

    use helyosd::adapters::runtime::{RuntimeDetector, RuntimeKind};
    let kind: RuntimeKind = cli
        .runtime
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let resolved = RuntimeDetector::resolve(kind).unwrap_or(RuntimeKind::Docker);
    let runtime = RuntimeDetector::build(resolved, &cli.data_dir).await?;
    info!(
        runtime = runtime.runtime_name(),
        "worker container runtime initialized"
    );

    let listen_addr = format!("{}:{}", cli.host, cli.grpc_port);

    // Load the CA certificate for TLS verification when connecting to the master.
    // If the CA cert file exists in the data directory, enable TLS; otherwise
    // fall back to plaintext (useful for development/testing).
    let data_dir = PathBuf::from(&cli.data_dir);
    let ca_cert_path = helyosd::cluster::tls::ca_cert_path(&data_dir);
    let client_tls = if ca_cert_path.exists() {
        let ca_pem = std::fs::read(&ca_cert_path)
            .map_err(|e| anyhow::anyhow!("failed to read CA cert: {e}"))?;
        let ca = tonic::transport::Certificate::from_pem(ca_pem);
        let config = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(ca)
            .domain_name("helyos");
        info!("worker TLS enabled (CA cert loaded)");
        Some(config)
    } else {
        info!("no CA cert found — connecting to master without TLS");
        None
    };

    helyosd::cluster::worker::start_worker(
        master_addr,
        token,
        listen_addr,
        runtime,
        store,
        client_tls,
    )
    .await
}
