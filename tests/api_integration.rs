use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use helyos_core::domain::orchestrator::Orchestrator;
use helyos_core::ports::runtime::{
    ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState, EventStream, LogStream,
    RuntimeEvent,
};
use rusqlite::Connection;

use helyosd::adapters::secrets::EncryptedSqliteSecretStore;
use helyosd::adapters::state::{InMemoryRouteStore, SqliteStore};
use helyosd::adapters::transport::LocalTransport;
use helyosd::api::{AppState, routes};

// ---------------------------------------------------------------------------
// MockRuntime
// ---------------------------------------------------------------------------

struct MockRuntime;

#[async_trait]
impl ContainerRuntime for MockRuntime {
    fn runtime_name(&self) -> &'static str {
        "mock"
    }

    async fn pull_image(&self, _image: &str) -> helyos_core::error::Result<()> {
        Ok(())
    }

    async fn create_container(
        &self,
        config: &ContainerConfig,
    ) -> helyos_core::error::Result<String> {
        Ok(format!("mock-{}", config.name))
    }

    async fn start_container(&self, _id: &str) -> helyos_core::error::Result<()> {
        Ok(())
    }

    async fn stop_container(
        &self,
        _id: &str,
        _timeout_secs: u64,
    ) -> helyos_core::error::Result<()> {
        Ok(())
    }

    async fn remove_container(&self, _id: &str, _force: bool) -> helyos_core::error::Result<()> {
        Ok(())
    }

    async fn inspect_container(&self, id: &str) -> helyos_core::error::Result<ContainerInfo> {
        Ok(ContainerInfo {
            id: id.to_string(),
            name: id.to_string(),
            image: "mock:latest".to_string(),
            state: ContainerState::Running,
        })
    }

    async fn logs(&self, _id: &str, _tail: Option<u64>) -> helyos_core::error::Result<LogStream> {
        Ok(Box::pin(stream::empty()))
    }

    async fn container_exists(&self, _name: &str) -> helyos_core::error::Result<bool> {
        Ok(false)
    }

    async fn create_network(&self, _name: &str) -> helyos_core::error::Result<String> {
        Ok("mock-net".to_string())
    }

    async fn remove_network(&self, _name: &str) -> helyos_core::error::Result<()> {
        Ok(())
    }

    async fn connect_to_network(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> helyos_core::error::Result<()> {
        Ok(())
    }

    async fn container_ip(
        &self,
        _container_id: &str,
        _network: &str,
    ) -> helyos_core::error::Result<String> {
        Ok("172.17.0.2".to_string())
    }

    async fn events(&self) -> helyos_core::error::Result<EventStream> {
        let stream: futures::stream::Pending<RuntimeEvent> = stream::pending();
        Ok(Box::pin(stream))
    }
}

// ---------------------------------------------------------------------------
// TestServer
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct TestServer {
    base_url: String,
    token_store: Arc<helyosd::adapters::state::TokenStore>,
    _dir: tempfile::TempDir,
}

impl TestServer {
    async fn new() -> Self {
        Self::build(None).await
    }

    /// Like `new`, but enforces auth using `token` as a legacy admin token
    /// (sets `api_token_hash`). Use for tests that exercise authentication.
    #[allow(dead_code)]
    async fn new_authed(token: &str) -> Self {
        Self::build(Some(helyosd::api::auth::hash_api_token(token))).await
    }

    async fn build(api_token_hash: Option<String>) -> Self {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let db_path = dir.path().join("helyosd.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

        // State store
        let sqlite = SqliteStore::connect(&db_url)
            .await
            .expect("failed to connect SqliteStore");
        let token_store = Arc::new(helyosd::adapters::state::TokenStore::new(sqlite.pool()));
        let store: Arc<dyn helyos_core::ports::state::StateStore> = Arc::new(sqlite);

        // Secret store (in-memory rusqlite for tests)
        let secret_conn = Connection::open_in_memory().expect("failed to open secret db");
        let secret_store = EncryptedSqliteSecretStore::new(secret_conn, &[0u8; 32])
            .expect("failed to create secret store");
        let secret_store: Arc<dyn helyos_core::ports::secrets::SecretStore> =
            Arc::new(secret_store);

        // Runtime + transport
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(MockRuntime);
        let transport = LocalTransport::new(runtime.clone());
        let transport: Arc<dyn helyos_core::ports::cluster::ClusterTransport> = Arc::new(transport);

        // Route store
        let route_store: Arc<dyn helyos_core::ports::route_store::RouteStore> =
            Arc::new(InMemoryRouteStore::new());

        let metrics: Arc<dyn helyos_core::ports::metrics::MetricsPort> =
            Arc::new(helyosd::adapters::metrics::PrometheusMetrics::new());

        // Orchestrator
        let handle = Orchestrator::spawn(
            runtime,
            Some(store.clone()),
            Some(secret_store),
            Some(transport),
            None,
            None,
            None,
            Some(route_store),
            None,
        );

        // Build axum app
        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        let state = AppState {
            handle,
            store: store.clone(),
            metrics,
            event_tx,
            api_token_hash,
            token_store: token_store.clone(),
        };
        let app = routes::build(state);

        // Bind to a random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind listener");
        let addr = listener.local_addr().expect("failed to get local addr");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server error");
        });

        Self {
            base_url,
            token_store,
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Build a minimal valid deploy JSON body.
fn deploy_body(project: &str, name: &str, replicas: u32) -> serde_json::Value {
    serde_json::json!({
        "project": project,
        "deployment": { "name": name },
        "replicas": replicas,
        "image": "nginx:latest",
        "ports": [8080]
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_ok() {
    let server = TestServer::new().await;
    let resp = client()
        .get(server.url("/health"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn create_and_list_projects() {
    let server = TestServer::new().await;
    let c = client();

    // Create project
    let resp = c
        .post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "myapp" }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        201,
        "create project failed: {}",
        resp.status()
    );

    // List projects
    let resp = c
        .get(server.url("/api/v1/projects"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("invalid JSON");
    let projects = body.as_array().expect("expected array");
    let names: Vec<&str> = projects
        .iter()
        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        names.contains(&"myapp"),
        "project 'myapp' not found in list: {names:?}"
    );
}

#[tokio::test]
async fn delete_project() {
    let server = TestServer::new().await;
    let c = client();

    // Create
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "todelete" }))
        .send()
        .await
        .expect("request failed");

    // Delete
    let resp = c
        .delete(server.url("/api/v1/projects/todelete"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "delete failed: {}", resp.status());

    // Verify gone
    let resp = c
        .get(server.url("/api/v1/projects"))
        .send()
        .await
        .expect("request failed");
    let body: serde_json::Value = resp.json().await.expect("invalid JSON");
    let projects = body.as_array().expect("expected array");
    let names: Vec<&str> = projects
        .iter()
        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        !names.contains(&"todelete"),
        "project should have been deleted but still in list: {names:?}"
    );
}

#[tokio::test]
async fn suspend_and_resume_project() {
    let server = TestServer::new().await;
    let c = client();

    // Create project
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "lifecycle" }))
        .send()
        .await
        .expect("request failed");

    // Suspend
    let resp = c
        .post(server.url("/api/v1/projects/lifecycle/suspend"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "suspend failed: {}", resp.status());

    // Resume
    let resp = c
        .post(server.url("/api/v1/projects/lifecycle/resume"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "resume failed: {}", resp.status());
}

#[tokio::test]
async fn deploy_and_list_pods() {
    let server = TestServer::new().await;
    let c = client();

    // Create project first
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "podapp" }))
        .send()
        .await
        .expect("request failed");

    // Deploy with 2 replicas
    let body = serde_json::to_string(&deploy_body("podapp", "web", 2)).unwrap();
    let resp = c
        .post(server.url("/api/v1/deploy"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        201,
        "deploy failed: {} — {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Give orchestrator time to process
    tokio::time::sleep(Duration::from_millis(200)).await;

    // List deployments
    let resp = c
        .get(server.url("/api/v1/deployments?project=podapp"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let deployments: serde_json::Value = resp.json().await.expect("invalid JSON");
    let dep_arr = deployments.as_array().expect("expected array");
    assert_eq!(
        dep_arr.len(),
        1,
        "expected 1 deployment, got {}",
        dep_arr.len()
    );

    // List pods
    let resp = c
        .get(server.url("/api/v1/pods?project=podapp"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let pods: serde_json::Value = resp.json().await.expect("invalid JSON");
    let pod_arr = pods.as_array().expect("expected array");
    assert_eq!(
        pod_arr.len(),
        2,
        "expected 2 pods for 2 replicas, got {}: {:?}",
        pod_arr.len(),
        pods
    );
}

#[tokio::test]
async fn scale_deployment() {
    let server = TestServer::new().await;
    let c = client();

    // Create project
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "scaleapp" }))
        .send()
        .await
        .expect("request failed");

    // Deploy with 1 replica
    let body = serde_json::to_string(&deploy_body("scaleapp", "svc", 1)).unwrap();
    c.post(server.url("/api/v1/deploy"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("request failed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Scale to 3
    let resp = c
        .post(server.url("/api/v1/projects/scaleapp/deployments/svc/scale"))
        .json(&serde_json::json!({ "replicas": 3 }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        200,
        "scale failed: {} — {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify 3 pods
    let resp = c
        .get(server.url("/api/v1/pods?project=scaleapp"))
        .send()
        .await
        .expect("request failed");
    let pods: serde_json::Value = resp.json().await.expect("invalid JSON");
    let pod_arr = pods.as_array().expect("expected array");
    assert_eq!(
        pod_arr.len(),
        3,
        "expected 3 pods after scale, got {}: {:?}",
        pod_arr.len(),
        pods
    );
}

#[tokio::test]
async fn stop_and_remove_deployment() {
    let server = TestServer::new().await;
    let c = client();

    // Create project
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "rmapp" }))
        .send()
        .await
        .expect("request failed");

    // Deploy
    let body = serde_json::to_string(&deploy_body("rmapp", "worker", 1)).unwrap();
    let resp = c
        .post(server.url("/api/v1/deploy"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 201, "deploy failed: {}", resp.status());

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Stop
    let resp = c
        .post(server.url("/api/v1/projects/rmapp/deployments/worker/stop"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "stop failed: {}", resp.status());

    // Remove
    let resp = c
        .delete(server.url("/api/v1/projects/rmapp/deployments/worker"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "remove failed: {}", resp.status());
}

#[tokio::test]
async fn secrets_crud() {
    let server = TestServer::new().await;
    let c = client();

    // Create project
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "secretapp" }))
        .send()
        .await
        .expect("request failed");

    // Set secret
    let resp = c
        .post(server.url("/api/v1/projects/secretapp/secrets/DB_PASS"))
        .json(&serde_json::json!({ "value": "super-secret-123" }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "set secret failed: {}", resp.status());

    // List secrets — should contain "DB_PASS"
    let resp = c
        .get(server.url("/api/v1/projects/secretapp/secrets"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("invalid JSON");
    let names = body.as_array().expect("expected array of secret names");
    let name_strs: Vec<&str> = names.iter().filter_map(|n| n.as_str()).collect();
    assert!(
        name_strs.contains(&"DB_PASS"),
        "DB_PASS not in secret list: {name_strs:?}"
    );

    // Delete secret
    let resp = c
        .delete(server.url("/api/v1/projects/secretapp/secrets/DB_PASS"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        200,
        "delete secret failed: {}",
        resp.status()
    );

    // Verify gone
    let resp = c
        .get(server.url("/api/v1/projects/secretapp/secrets"))
        .send()
        .await
        .expect("request failed");
    let body: serde_json::Value = resp.json().await.expect("invalid JSON");
    let names = body.as_array().expect("expected array");
    let name_strs: Vec<&str> = names.iter().filter_map(|n| n.as_str()).collect();
    assert!(
        !name_strs.contains(&"DB_PASS"),
        "DB_PASS should have been deleted but still present: {name_strs:?}"
    );
}

#[tokio::test]
async fn route_management() {
    let server = TestServer::new().await;
    let c = client();

    // Create project
    c.post(server.url("/api/v1/projects"))
        .json(&serde_json::json!({ "name": "routeapp" }))
        .send()
        .await
        .expect("request failed");

    // Deploy
    let body = serde_json::to_string(&deploy_body("routeapp", "frontend", 1)).unwrap();
    c.post(server.url("/api/v1/deploy"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("request failed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Add route
    let resp = c
        .post(server.url("/api/v1/routes"))
        .json(&serde_json::json!({
            "domain": "app.example.com",
            "project": "routeapp",
            "deployment": "frontend",
            "tls_mode": "none"
        }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 201, "add route failed: {}", resp.status());

    // List routes
    let resp = c
        .get(server.url("/api/v1/routes"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let routes: serde_json::Value = resp.json().await.expect("invalid JSON");
    let route_arr = routes.as_array().expect("expected array");
    let domains: Vec<&str> = route_arr
        .iter()
        .filter_map(|r| r.get("domain").and_then(|d| d.as_str()))
        .collect();
    assert!(
        domains.contains(&"app.example.com"),
        "route domain not found in list: {domains:?}"
    );

    // Delete route
    let resp = c
        .delete(server.url("/api/v1/routes/app.example.com"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "delete route failed: {}", resp.status());
}

#[tokio::test]
async fn scheduler_config() {
    let server = TestServer::new().await;
    let c = client();

    // GET default config — must have a "strategy" field
    let resp = c
        .get(server.url("/api/v1/cluster/scheduler"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let config: serde_json::Value = resp.json().await.expect("invalid JSON");
    assert!(
        config.get("strategy").is_some(),
        "default scheduler config missing 'strategy' field: {config}"
    );

    // POST new config using strategy
    let resp = c
        .post(server.url("/api/v1/cluster/scheduler"))
        .json(&serde_json::json!({ "strategy": "binpack" }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        200,
        "set scheduler config failed: {} — {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Verify strategy changed
    let resp = c
        .get(server.url("/api/v1/cluster/scheduler"))
        .send()
        .await
        .expect("request failed");
    let config: serde_json::Value = resp.json().await.expect("invalid JSON");
    assert_eq!(
        config.get("strategy").and_then(|s| s.as_str()),
        Some("binpack"),
        "strategy should be 'binpack' after update: {config}"
    );
}

#[tokio::test]
async fn deploy_invalid_spec_returns_400() {
    let server = TestServer::new().await;
    let c = client();

    let resp = c
        .post(server.url("/api/v1/deploy"))
        .header("content-type", "application/json")
        .body("this is not valid json or yaml at all !!!")
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        400,
        "expected 400 for invalid spec, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_format() {
    let server = TestServer::new().await;
    let c = client();

    let _ = c.get(server.url("/health")).send().await;

    let resp = c
        .get(server.url("/metrics"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/plain"),
        "expected text/plain content-type, got: {content_type}"
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("helyos_http_requests_total"),
        "expected helyos_http_requests_total in metrics output"
    );
    assert!(
        body.contains("helyos_http_request_duration_seconds"),
        "expected duration histogram in metrics output"
    );
}

// ---------------------------------------------------------------------------
// Multi-token auth (Task 4)
// ---------------------------------------------------------------------------

use helyosd::adapters::state::NewApiToken;
use helyosd::api::auth::{hash_api_token, token_prefix};

/// Seed an active token row directly and return the plaintext to present.
async fn seed_token(server: &TestServer, name: &str, secret: &str) {
    server
        .token_store
        .create(NewApiToken {
            name: name.to_string(),
            token_hash: hash_api_token(secret),
            token_prefix: token_prefix(secret),
            scope: "admin".to_string(),
            expires_at: None,
        })
        .await
        .expect("seed token");
}

#[tokio::test]
async fn stored_token_authenticates() {
    let server = TestServer::new_authed("nxa-api_adminadminadmin").await;
    let secret = "nxa-api_storedstoredstored";
    seed_token(&server, "ci", secret).await;

    let resp = client()
        .get(server.url("/api/v1/projects"))
        .bearer_auth(secret)
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "valid stored token must be accepted");
}

#[tokio::test]
async fn bogus_token_is_rejected() {
    let server = TestServer::new_authed("nxa-api_adminadminadmin").await;
    let resp = client()
        .get(server.url("/api/v1/projects"))
        .bearer_auth("nxa-api_nope")
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 401, "unknown token must be rejected");
}

#[tokio::test]
async fn revoked_token_is_rejected() {
    let server = TestServer::new_authed("nxa-api_adminadminadmin").await;
    let secret = "nxa-api_revokerevokerevoke";
    seed_token(&server, "temp", secret).await;
    assert!(server.token_store.revoke_by_name("temp").await.unwrap());

    let resp = client()
        .get(server.url("/api/v1/projects"))
        .bearer_auth(secret)
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 401, "revoked token must be rejected");
}

#[tokio::test]
async fn legacy_admin_token_still_works() {
    let admin = "nxa-api_adminadminadmin";
    let server = TestServer::new_authed(admin).await;
    let resp = client()
        .get(server.url("/api/v1/projects"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "legacy api_token_hash must still authenticate");
}

// ---------------------------------------------------------------------------
// Token management endpoints (Task 5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_use_list_revoke_token_flow() {
    let admin = "nxa-api_adminadminadmin";
    let server = TestServer::new_authed(admin).await;
    let c = client();

    // Create a token as admin.
    let resp = c
        .post(server.url("/api/v1/tokens"))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "name": "deploy-bot" }))
        .send()
        .await
        .expect("create failed");
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let secret = body["token"].as_str().expect("token in body").to_string();
    assert_eq!(body["name"], "deploy-bot");

    // The new token authenticates.
    let resp = c
        .get(server.url("/api/v1/projects"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // whoami reflects identity.
    let who: serde_json::Value = c
        .get(server.url("/api/v1/whoami"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["name"], "deploy-bot");

    // List shows it, and never the secret.
    let list: serde_json::Value = c
        .get(server.url("/api/v1/tokens"))
        .bearer_auth(admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"deploy-bot"));
    assert!(list.as_array().unwrap().iter().all(|t| t.get("token_hash").is_none()));

    // Revoke it → 204, then it stops working.
    let resp = c
        .delete(server.url("/api/v1/tokens/deploy-bot"))
        .bearer_auth(admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    let resp = c
        .get(server.url("/api/v1/projects"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "revoked token must stop authenticating");
}

#[tokio::test]
async fn create_duplicate_name_conflicts() {
    let admin = "nxa-api_adminadminadmin";
    let server = TestServer::new_authed(admin).await;
    let c = client();
    let body = serde_json::json!({ "name": "dup" });
    let first = c.post(server.url("/api/v1/tokens")).bearer_auth(admin).json(&body).send().await.unwrap();
    assert_eq!(first.status(), 201);
    let second = c.post(server.url("/api/v1/tokens")).bearer_auth(admin).json(&body).send().await.unwrap();
    assert_eq!(second.status(), 409);
}

#[tokio::test]
async fn revoke_unknown_token_is_404() {
    let admin = "nxa-api_adminadminadmin";
    let server = TestServer::new_authed(admin).await;
    let resp = client()
        .delete(server.url("/api/v1/tokens/ghost"))
        .bearer_auth(admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn create_empty_name_is_422() {
    let admin = "nxa-api_adminadminadmin";
    let server = TestServer::new_authed(admin).await;
    let resp = client()
        .post(server.url("/api/v1/tokens"))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "name": "  " }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422, "blank token name must be rejected");
}
