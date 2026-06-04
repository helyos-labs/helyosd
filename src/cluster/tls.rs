//! Self-signed TLS certificate generation and loading for gRPC cluster
//! communication.
//!
//! On first start the master generates a self-signed CA and a server
//! certificate signed by that CA.  The resulting PEM files are persisted in the
//! data directory so that subsequent restarts reuse the same material.
//!
//! Workers receive the CA certificate out-of-band (e.g. via the join flow) and
//! use it to verify the master's identity.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};
use tracing::info;

/// File names used inside the data directory.
const CA_CERT_FILE: &str = "grpc-ca.pem";
const SERVER_CERT_FILE: &str = "grpc-server.pem";
const SERVER_KEY_FILE: &str = "grpc-server-key.pem";

/// Loaded certificate material ready for use with tonic.
pub struct GrpcTlsCerts {
    pub ca_pem: Vec<u8>,
    pub server_cert_pem: Vec<u8>,
    pub server_key_pem: Vec<u8>,
}

impl GrpcTlsCerts {
    /// Build a [`ServerTlsConfig`] from the loaded material.
    pub fn server_tls_config(&self) -> Result<ServerTlsConfig> {
        let identity = Identity::from_pem(&self.server_cert_pem, &self.server_key_pem);
        let config = ServerTlsConfig::new().identity(identity);
        Ok(config)
    }

    /// Build a [`ClientTlsConfig`] that trusts this CA.
    ///
    /// The `domain_name` should match the SAN in the server certificate.  For
    /// self-signed certs we default to `"helyos"`.
    pub fn client_tls_config(&self) -> Result<ClientTlsConfig> {
        let ca = Certificate::from_pem(&self.ca_pem);
        let config = ClientTlsConfig::new()
            .ca_certificate(ca)
            .domain_name("helyos");
        Ok(config)
    }
}

/// Load existing certificates from `data_dir`, or generate new self-signed
/// ones if they do not exist.
pub fn load_or_generate(data_dir: &Path) -> Result<GrpcTlsCerts> {
    let ca_path = data_dir.join(CA_CERT_FILE);
    let cert_path = data_dir.join(SERVER_CERT_FILE);
    let key_path = data_dir.join(SERVER_KEY_FILE);

    if ca_path.exists() && cert_path.exists() && key_path.exists() {
        info!("loading existing gRPC TLS certificates");
        let ca_pem = fs::read(&ca_path).context("read CA cert")?;
        let server_cert_pem = fs::read(&cert_path).context("read server cert")?;
        let server_key_pem = fs::read(&key_path).context("read server key")?;
        return Ok(GrpcTlsCerts {
            ca_pem,
            server_cert_pem,
            server_key_pem,
        });
    }

    info!("generating self-signed gRPC TLS certificates");
    generate_and_persist(&ca_path, &cert_path, &key_path)
}

/// Returns the path where the CA certificate is stored.
pub fn ca_cert_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CA_CERT_FILE)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn generate_and_persist(ca_path: &Path, cert_path: &Path, key_path: &Path) -> Result<GrpcTlsCerts> {
    use rcgen::{CertificateParams, DnType, IsCa, KeyPair};

    // --- Generate CA ---
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Helyos gRPC CA");
    // Valid for ~10 years.
    ca_params.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let ca_key_pair = KeyPair::generate().context("generate CA key pair")?;
    let ca_cert = ca_params
        .self_signed(&ca_key_pair)
        .context("self-sign CA")?;

    let ca_pem = ca_cert.pem().into_bytes();

    // --- Generate server certificate signed by the CA ---
    let mut server_params =
        CertificateParams::new(vec!["helyos".to_string()]).context("server params")?;
    server_params
        .distinguished_name
        .push(DnType::CommonName, "helyos");
    // Also accept connections via localhost / 127.0.0.1 for local development.
    server_params
        .subject_alt_names
        .push(rcgen::SanType::DnsName(
            "localhost".try_into().context("SAN localhost")?,
        ));
    server_params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::new(127, 0, 0, 1),
        )));
    server_params.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let server_key_pair = KeyPair::generate().context("generate server key pair")?;
    let server_cert = server_params
        .signed_by(&server_key_pair, &ca_cert, &ca_key_pair)
        .context("sign server cert")?;

    let server_cert_pem = server_cert.pem().into_bytes();
    let server_key_pem = server_key_pair.serialize_pem().into_bytes();

    // --- Persist ---
    fs::write(ca_path, &ca_pem).context("write CA cert")?;
    fs::write(cert_path, &server_cert_pem).context("write server cert")?;
    fs::write(key_path, &server_key_pem).context("write server key")?;

    info!(
        ca = %ca_path.display(),
        cert = %cert_path.display(),
        "gRPC TLS certificates generated"
    );

    Ok(GrpcTlsCerts {
        ca_pem,
        server_cert_pem,
        server_key_pem,
    })
}
