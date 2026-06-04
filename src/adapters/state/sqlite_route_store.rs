use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use rusqlite::Connection;
use tokio::sync::Mutex;

use nexa_core::domain::models::{Certificate, Route, SubnetAllocation, TlsMode};
use nexa_core::error::{NexaError, Result};
use nexa_core::ports::route_store::RouteStore;

/// SQLite-backed route store that persists routes, certificates, and subnet
/// allocations across daemon restarts.
///
/// Follows the same `Arc<Mutex<Connection>>` pattern used by
/// [`EncryptedSqliteSecretStore`](crate::adapters::secrets::EncryptedSqliteSecretStore).
pub struct SqliteRouteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteRouteStore {
    /// Create a new `SqliteRouteStore`, creating the backing tables if they do
    /// not already exist.
    pub fn new(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS routes (
                domain      TEXT PRIMARY KEY,
                project     TEXT NOT NULL,
                deployment  TEXT NOT NULL,
                tls_mode    TEXT NOT NULL,
                created_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS certificates (
                domain       TEXT PRIMARY KEY,
                cert_pem     BLOB NOT NULL,
                key_pem_enc  BLOB NOT NULL,
                key_nonce    BLOB NOT NULL,
                issued_at    TEXT NOT NULL,
                expires_at   TEXT NOT NULL,
                acme_account TEXT
            );

            CREATE TABLE IF NOT EXISTS subnet_allocations (
                node_id  TEXT NOT NULL,
                project  TEXT NOT NULL,
                subnet   TEXT NOT NULL UNIQUE,
                PRIMARY KEY (node_id, project)
            );",
        )
        .map_err(|e| NexaError::Runtime(format!("failed to init route store tables: {e}")))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl RouteStore for SqliteRouteStore {
    // ── Routes ──────────────────────────────────────────────

    async fn insert_route(&self, route: &Route) -> Result<()> {
        let conn = self.conn.lock().await;
        let result = conn.execute(
            "INSERT INTO routes (domain, project, deployment, tls_mode, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                route.domain,
                route.project,
                route.deployment,
                route.tls_mode.to_string(),
                route.created_at.to_rfc3339(),
            ],
        );

        match result {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(NexaError::RouteAlreadyExists(route.domain.clone()))
            }
            Err(e) => Err(NexaError::Runtime(format!("insert_route failed: {e}"))),
        }
    }

    async fn get_route(&self, domain: &str) -> Result<Option<Route>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT domain, project, deployment, tls_mode, created_at
                 FROM routes WHERE domain = ?1",
            )
            .map_err(|e| NexaError::Runtime(format!("get_route prepare failed: {e}")))?;

        let result = stmt.query_row(rusqlite::params![domain], |row| Ok(row_to_route(row)));

        match result {
            Ok(route) => Ok(Some(route?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NexaError::Runtime(format!("get_route failed: {e}"))),
        }
    }

    async fn list_routes(&self, project: Option<&str>) -> Result<Vec<Route>> {
        let conn = self.conn.lock().await;
        match project {
            Some(p) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT domain, project, deployment, tls_mode, created_at
                         FROM routes WHERE project = ?1 ORDER BY domain",
                    )
                    .map_err(|e| NexaError::Runtime(format!("list_routes prepare failed: {e}")))?;

                let rows = stmt
                    .query_map(rusqlite::params![p], |row| Ok(row_to_route(row)))
                    .map_err(|e| NexaError::Runtime(format!("list_routes failed: {e}")))?;

                rows.map(|r| r.map_err(|e| NexaError::Runtime(format!("row read failed: {e}")))?)
                    .collect()
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT domain, project, deployment, tls_mode, created_at
                         FROM routes ORDER BY domain",
                    )
                    .map_err(|e| NexaError::Runtime(format!("list_routes prepare failed: {e}")))?;

                let rows = stmt
                    .query_map([], |row| Ok(row_to_route(row)))
                    .map_err(|e| NexaError::Runtime(format!("list_routes failed: {e}")))?;

                rows.map(|r| r.map_err(|e| NexaError::Runtime(format!("row read failed: {e}")))?)
                    .collect()
            }
        }
    }

    async fn delete_route(&self, domain: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let affected = conn
            .execute(
                "DELETE FROM routes WHERE domain = ?1",
                rusqlite::params![domain],
            )
            .map_err(|e| NexaError::Runtime(format!("delete_route failed: {e}")))?;
        Ok(affected > 0)
    }

    // ── Certificates ────────────────────────────────────────

    async fn upsert_certificate(&self, cert: &Certificate) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO certificates (domain, cert_pem, key_pem_enc, key_nonce, issued_at, expires_at, acme_account)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(domain) DO UPDATE SET
                cert_pem     = excluded.cert_pem,
                key_pem_enc  = excluded.key_pem_enc,
                key_nonce    = excluded.key_nonce,
                issued_at    = excluded.issued_at,
                expires_at   = excluded.expires_at,
                acme_account = excluded.acme_account",
            rusqlite::params![
                cert.domain,
                cert.cert_pem,
                cert.key_pem_enc,
                cert.key_nonce,
                cert.issued_at.to_rfc3339(),
                cert.expires_at.to_rfc3339(),
                cert.acme_account,
            ],
        )
        .map_err(|e| NexaError::Runtime(format!("upsert_certificate failed: {e}")))?;
        Ok(())
    }

    async fn get_certificate(&self, domain: &str) -> Result<Option<Certificate>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT domain, cert_pem, key_pem_enc, key_nonce, issued_at, expires_at, acme_account
                 FROM certificates WHERE domain = ?1",
            )
            .map_err(|e| NexaError::Runtime(format!("get_certificate prepare failed: {e}")))?;

        let result = stmt.query_row(rusqlite::params![domain], |row| Ok(row_to_certificate(row)));

        match result {
            Ok(cert) => Ok(Some(cert?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NexaError::Runtime(format!("get_certificate failed: {e}"))),
        }
    }

    async fn list_expiring_certificates(&self, within_days: i64) -> Result<Vec<Certificate>> {
        let threshold = Utc::now() + Duration::days(within_days);
        let threshold_str = threshold.to_rfc3339();

        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT domain, cert_pem, key_pem_enc, key_nonce, issued_at, expires_at, acme_account
                 FROM certificates WHERE expires_at <= ?1",
            )
            .map_err(|e| {
                NexaError::Runtime(format!("list_expiring_certificates prepare failed: {e}"))
            })?;

        let rows = stmt
            .query_map(rusqlite::params![threshold_str], |row| {
                Ok(row_to_certificate(row))
            })
            .map_err(|e| NexaError::Runtime(format!("list_expiring_certificates failed: {e}")))?;

        rows.map(|r| r.map_err(|e| NexaError::Runtime(format!("row read failed: {e}")))?)
            .collect()
    }

    async fn delete_certificate(&self, domain: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let affected = conn
            .execute(
                "DELETE FROM certificates WHERE domain = ?1",
                rusqlite::params![domain],
            )
            .map_err(|e| NexaError::Runtime(format!("delete_certificate failed: {e}")))?;
        Ok(affected > 0)
    }

    // ── Subnets ─────────────────────────────────────────────

    async fn allocate_subnet(&self, alloc: &SubnetAllocation) -> Result<()> {
        let conn = self.conn.lock().await;

        // Check if the subnet CIDR is already in use (by any node/project).
        let subnet_taken: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM subnet_allocations WHERE subnet = ?1",
                rusqlite::params![alloc.subnet],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| NexaError::Runtime(format!("allocate_subnet check failed: {e}")))?
            > 0;

        if subnet_taken {
            return Err(NexaError::Network(format!(
                "subnet {} already in use",
                alloc.subnet
            )));
        }

        let result = conn.execute(
            "INSERT INTO subnet_allocations (node_id, project, subnet) VALUES (?1, ?2, ?3)",
            rusqlite::params![alloc.node_id, alloc.project, alloc.subnet],
        );

        match result {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(NexaError::Network(format!(
                    "subnet already allocated for node {} project {}",
                    alloc.node_id, alloc.project
                )))
            }
            Err(e) => Err(NexaError::Runtime(format!("allocate_subnet failed: {e}"))),
        }
    }

    async fn get_node_subnet(
        &self,
        node_id: &str,
        project: &str,
    ) -> Result<Option<SubnetAllocation>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT node_id, project, subnet
                 FROM subnet_allocations WHERE node_id = ?1 AND project = ?2",
            )
            .map_err(|e| NexaError::Runtime(format!("get_node_subnet prepare failed: {e}")))?;

        let result = stmt.query_row(rusqlite::params![node_id, project], |row| {
            Ok(SubnetAllocation {
                node_id: row.get(0)?,
                project: row.get(1)?,
                subnet: row.get(2)?,
            })
        });

        match result {
            Ok(alloc) => Ok(Some(alloc)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NexaError::Runtime(format!("get_node_subnet failed: {e}"))),
        }
    }

    async fn list_subnets(&self) -> Result<Vec<SubnetAllocation>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT node_id, project, subnet FROM subnet_allocations ORDER BY node_id")
            .map_err(|e| NexaError::Runtime(format!("list_subnets prepare failed: {e}")))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(SubnetAllocation {
                    node_id: row.get(0)?,
                    project: row.get(1)?,
                    subnet: row.get(2)?,
                })
            })
            .map_err(|e| NexaError::Runtime(format!("list_subnets failed: {e}")))?;

        rows.map(|r| r.map_err(|e| NexaError::Runtime(format!("row read failed: {e}"))))
            .collect()
    }

    async fn deallocate_subnet(&self, node_id: &str, project: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let affected = conn
            .execute(
                "DELETE FROM subnet_allocations WHERE node_id = ?1 AND project = ?2",
                rusqlite::params![node_id, project],
            )
            .map_err(|e| NexaError::Runtime(format!("deallocate_subnet failed: {e}")))?;
        Ok(affected > 0)
    }
}

// ── Helper functions ────────────────────────────────────────

fn row_to_route(row: &rusqlite::Row<'_>) -> Result<Route> {
    let domain: String = row
        .get(0)
        .map_err(|e| NexaError::Runtime(format!("invalid domain: {e}")))?;
    let project: String = row
        .get(1)
        .map_err(|e| NexaError::Runtime(format!("invalid project: {e}")))?;
    let deployment: String = row
        .get(2)
        .map_err(|e| NexaError::Runtime(format!("invalid deployment: {e}")))?;
    let tls_mode_str: String = row
        .get(3)
        .map_err(|e| NexaError::Runtime(format!("invalid tls_mode: {e}")))?;
    let created_at_str: String = row
        .get(4)
        .map_err(|e| NexaError::Runtime(format!("invalid created_at: {e}")))?;

    let tls_mode: TlsMode = tls_mode_str
        .parse()
        .map_err(|e: String| NexaError::Runtime(format!("invalid tls_mode value: {e}")))?;
    let created_at = created_at_str
        .parse()
        .map_err(|e: chrono::ParseError| NexaError::Runtime(format!("invalid created_at: {e}")))?;

    Ok(Route {
        domain,
        project,
        deployment,
        tls_mode,
        created_at,
    })
}

fn row_to_certificate(row: &rusqlite::Row<'_>) -> Result<Certificate> {
    let domain: String = row
        .get(0)
        .map_err(|e| NexaError::Runtime(format!("invalid domain: {e}")))?;
    let cert_pem: Vec<u8> = row
        .get(1)
        .map_err(|e| NexaError::Runtime(format!("invalid cert_pem: {e}")))?;
    let key_pem_enc: Vec<u8> = row
        .get(2)
        .map_err(|e| NexaError::Runtime(format!("invalid key_pem_enc: {e}")))?;
    let key_nonce: Vec<u8> = row
        .get(3)
        .map_err(|e| NexaError::Runtime(format!("invalid key_nonce: {e}")))?;
    let issued_at_str: String = row
        .get(4)
        .map_err(|e| NexaError::Runtime(format!("invalid issued_at: {e}")))?;
    let expires_at_str: String = row
        .get(5)
        .map_err(|e| NexaError::Runtime(format!("invalid expires_at: {e}")))?;
    let acme_account: Option<String> = row
        .get(6)
        .map_err(|e| NexaError::Runtime(format!("invalid acme_account: {e}")))?;

    let issued_at = issued_at_str
        .parse()
        .map_err(|e: chrono::ParseError| NexaError::Runtime(format!("invalid issued_at: {e}")))?;
    let expires_at = expires_at_str
        .parse()
        .map_err(|e: chrono::ParseError| NexaError::Runtime(format!("invalid expires_at: {e}")))?;

    Ok(Certificate {
        domain,
        cert_pem,
        key_pem_enc,
        key_nonce,
        issued_at,
        expires_at,
        acme_account,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> SqliteRouteStore {
        let conn = Connection::open_in_memory().unwrap();
        SqliteRouteStore::new(conn).unwrap()
    }

    #[tokio::test]
    async fn insert_and_get_route() {
        let store = make_store();
        let route = Route::new("api.example.com", "ecommerce", "api", TlsMode::Auto);
        store.insert_route(&route).await.unwrap();

        let fetched = store.get_route("api.example.com").await.unwrap().unwrap();
        assert_eq!(fetched.domain, "api.example.com");
        assert_eq!(fetched.project, "ecommerce");
        assert_eq!(fetched.tls_mode, TlsMode::Auto);
    }

    #[tokio::test]
    async fn insert_duplicate_route_fails() {
        let store = make_store();
        let route = Route::new("api.example.com", "ecommerce", "api", TlsMode::None);
        store.insert_route(&route).await.unwrap();
        assert!(store.insert_route(&route).await.is_err());
    }

    #[tokio::test]
    async fn list_routes_filter_by_project() {
        let store = make_store();
        store
            .insert_route(&Route::new("a.example.com", "proj-a", "api", TlsMode::None))
            .await
            .unwrap();
        store
            .insert_route(&Route::new("b.example.com", "proj-b", "web", TlsMode::Auto))
            .await
            .unwrap();

        let all = store.list_routes(None).await.unwrap();
        assert_eq!(all.len(), 2);

        let proj_a = store.list_routes(Some("proj-a")).await.unwrap();
        assert_eq!(proj_a.len(), 1);
        assert_eq!(proj_a[0].domain, "a.example.com");
    }

    #[tokio::test]
    async fn delete_route() {
        let store = make_store();
        store
            .insert_route(&Route::new("api.example.com", "p", "d", TlsMode::None))
            .await
            .unwrap();
        assert!(store.delete_route("api.example.com").await.unwrap());
        assert!(!store.delete_route("api.example.com").await.unwrap());
        assert!(store.get_route("api.example.com").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_and_get_certificate() {
        let store = make_store();
        let cert = Certificate {
            domain: "api.example.com".into(),
            cert_pem: b"CERT".to_vec(),
            key_pem_enc: b"KEY".to_vec(),
            key_nonce: b"NONCE".to_vec(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(90),
            acme_account: None,
        };
        store.upsert_certificate(&cert).await.unwrap();

        let fetched = store
            .get_certificate("api.example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.cert_pem, b"CERT");
    }

    #[tokio::test]
    async fn upsert_certificate_overwrites() {
        let store = make_store();
        let cert1 = Certificate {
            domain: "api.example.com".into(),
            cert_pem: b"CERT_V1".to_vec(),
            key_pem_enc: b"KEY".to_vec(),
            key_nonce: b"NONCE".to_vec(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(90),
            acme_account: None,
        };
        store.upsert_certificate(&cert1).await.unwrap();

        let cert2 = Certificate {
            domain: "api.example.com".into(),
            cert_pem: b"CERT_V2".to_vec(),
            key_pem_enc: b"KEY2".to_vec(),
            key_nonce: b"NONCE2".to_vec(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(90),
            acme_account: Some("acct".into()),
        };
        store.upsert_certificate(&cert2).await.unwrap();

        let fetched = store
            .get_certificate("api.example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.cert_pem, b"CERT_V2");
        assert_eq!(fetched.acme_account, Some("acct".into()));
    }

    #[tokio::test]
    async fn list_expiring_certificates() {
        let store = make_store();
        let expiring_soon = Certificate {
            domain: "soon.example.com".into(),
            cert_pem: b"C".to_vec(),
            key_pem_enc: b"K".to_vec(),
            key_nonce: b"N".to_vec(),
            issued_at: Utc::now() - Duration::days(60),
            expires_at: Utc::now() + Duration::days(20),
            acme_account: None,
        };
        let not_expiring = Certificate {
            domain: "ok.example.com".into(),
            cert_pem: b"C".to_vec(),
            key_pem_enc: b"K".to_vec(),
            key_nonce: b"N".to_vec(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(80),
            acme_account: None,
        };
        store.upsert_certificate(&expiring_soon).await.unwrap();
        store.upsert_certificate(&not_expiring).await.unwrap();

        let expiring = store.list_expiring_certificates(30).await.unwrap();
        assert_eq!(expiring.len(), 1);
        assert_eq!(expiring[0].domain, "soon.example.com");
    }

    #[tokio::test]
    async fn delete_certificate() {
        let store = make_store();
        let cert = Certificate {
            domain: "api.example.com".into(),
            cert_pem: b"C".to_vec(),
            key_pem_enc: b"K".to_vec(),
            key_nonce: b"N".to_vec(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(90),
            acme_account: None,
        };
        store.upsert_certificate(&cert).await.unwrap();
        assert!(store.delete_certificate("api.example.com").await.unwrap());
        assert!(!store.delete_certificate("api.example.com").await.unwrap());
    }

    #[tokio::test]
    async fn allocate_and_list_subnets() {
        let store = make_store();
        let alloc = SubnetAllocation {
            node_id: "node-1".into(),
            project: "ecommerce".into(),
            subnet: "172.20.1.0/24".into(),
        };
        store.allocate_subnet(&alloc).await.unwrap();

        let subnets = store.list_subnets().await.unwrap();
        assert_eq!(subnets.len(), 1);
        assert_eq!(subnets[0].subnet, "172.20.1.0/24");
    }

    #[tokio::test]
    async fn allocate_duplicate_subnet_fails() {
        let store = make_store();
        let alloc = SubnetAllocation {
            node_id: "node-1".into(),
            project: "ecommerce".into(),
            subnet: "172.20.1.0/24".into(),
        };
        store.allocate_subnet(&alloc).await.unwrap();
        assert!(store.allocate_subnet(&alloc).await.is_err());
    }

    #[tokio::test]
    async fn allocate_same_subnet_different_node_fails() {
        let store = make_store();
        store
            .allocate_subnet(&SubnetAllocation {
                node_id: "node-1".into(),
                project: "ecommerce".into(),
                subnet: "172.20.1.0/24".into(),
            })
            .await
            .unwrap();
        let result = store
            .allocate_subnet(&SubnetAllocation {
                node_id: "node-2".into(),
                project: "ecommerce".into(),
                subnet: "172.20.1.0/24".into(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_node_subnet() {
        let store = make_store();
        let alloc = SubnetAllocation {
            node_id: "node-1".into(),
            project: "ecommerce".into(),
            subnet: "172.20.1.0/24".into(),
        };
        store.allocate_subnet(&alloc).await.unwrap();

        let fetched = store
            .get_node_subnet("node-1", "ecommerce")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.subnet, "172.20.1.0/24");

        let missing = store
            .get_node_subnet("node-999", "ecommerce")
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn deallocate_subnet() {
        let store = make_store();
        store
            .allocate_subnet(&SubnetAllocation {
                node_id: "node-1".into(),
                project: "p".into(),
                subnet: "172.20.1.0/24".into(),
            })
            .await
            .unwrap();
        assert!(store.deallocate_subnet("node-1", "p").await.unwrap());
        assert!(!store.deallocate_subnet("node-1", "p").await.unwrap());
    }
}
