use std::sync::Arc;

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit};
use tracing::info;

use helyos_core::domain::models::Certificate;
use helyos_core::error::{HelyosError, Result};
use helyos_core::ports::route_store::RouteStore;

pub struct AcmeManager {
    email: String,
    store: Arc<dyn RouteStore>,
    staging: bool,
    cipher: Aes256Gcm,
}

impl AcmeManager {
    pub fn new(
        email: &str,
        store: Arc<dyn RouteStore>,
        staging: bool,
        master_key: &[u8; 32],
    ) -> Self {
        let cipher = Aes256Gcm::new_from_slice(master_key).expect("master key must be 32 bytes");
        Self {
            email: email.to_string(),
            store,
            staging,
            cipher,
        }
    }

    pub async fn issue_certificate(&self, domain: &str) -> Result<Certificate> {
        info!(
            domain,
            email = self.email,
            staging = self.staging,
            "initiating ACME certificate issuance"
        );
        Err(HelyosError::Certificate(format!(
            "ACME issuance for '{domain}' requires network access and HTTP challenge validation"
        )))
    }

    pub async fn import_certificate(
        &self,
        domain: &str,
        cert_pem: Vec<u8>,
        key_pem: Vec<u8>,
    ) -> Result<()> {
        // Encrypt the private key using AES-256-GCM with a random nonce.
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let key_pem_enc = self
            .cipher
            .encrypt(&nonce, key_pem.as_ref())
            .map_err(|e| HelyosError::Certificate(format!("failed to encrypt private key: {e}")))?;

        let cert = Certificate {
            domain: domain.to_string(),
            cert_pem,
            key_pem_enc,
            key_nonce: nonce.to_vec(),
            issued_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::days(90),
            acme_account: None,
        };
        self.store.upsert_certificate(&cert).await?;
        info!(domain, "certificate imported (private key encrypted)");
        Ok(())
    }

    pub fn email(&self) -> &str {
        &self.email
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::state::memory_route_store::InMemoryRouteStore;

    fn test_key() -> [u8; 32] {
        [0xAB; 32]
    }

    fn make_acme() -> AcmeManager {
        let store = Arc::new(InMemoryRouteStore::new());
        AcmeManager::new("admin@example.com", store, true, &test_key())
    }

    #[test]
    fn acme_manager_email() {
        let acme = make_acme();
        assert_eq!(acme.email(), "admin@example.com");
    }

    #[tokio::test]
    async fn issue_certificate_returns_error_placeholder() {
        let acme = make_acme();
        let result = acme.issue_certificate("api.example.com").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ACME issuance"));
    }

    #[tokio::test]
    async fn import_certificate_stores_in_route_store() {
        let store = Arc::new(InMemoryRouteStore::new());
        let acme = AcmeManager::new("admin@example.com", store.clone(), true, &test_key());

        acme.import_certificate(
            "api.example.com",
            b"CERT PEM DATA".to_vec(),
            b"KEY PEM DATA".to_vec(),
        )
        .await
        .unwrap();

        let cert = store
            .get_certificate("api.example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cert.cert_pem, b"CERT PEM DATA");
        // The private key should be encrypted, NOT stored as plaintext.
        assert_ne!(cert.key_pem_enc, b"KEY PEM DATA");
        // The nonce should be a proper 12-byte value, not all zeros.
        assert_eq!(cert.key_nonce.len(), 12);
        assert_ne!(cert.key_nonce, vec![0u8; 12]);
        // Verify we can decrypt the stored key back to the original.
        let cipher = Aes256Gcm::new_from_slice(&test_key()).unwrap();
        let nonce = aes_gcm::Nonce::from_slice(&cert.key_nonce);
        let decrypted = cipher.decrypt(nonce, cert.key_pem_enc.as_ref()).unwrap();
        assert_eq!(decrypted, b"KEY PEM DATA");
    }
}
