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
// Reusable certificate generator
// ---------------------------------------------------------------------------

/// Raw self-signed CA + server cert material (PEM bytes).
pub struct CertMaterial {
    pub ca_pem: Vec<u8>,
    pub server_cert_pem: Vec<u8>,
    pub server_key_pem: Vec<u8>,
}

/// Generate a self-signed CA and a server certificate signed by it. The server
/// cert's SANs always include `localhost`, `127.0.0.1`, and `common_name`, plus
/// every entry in `san_hosts` (each parsed as an IP if possible, else a DNS name).
pub fn generate_ca_and_server_cert(common_name: &str, san_hosts: &[String]) -> Result<CertMaterial> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};
    use std::net::{IpAddr, Ipv4Addr};

    let mut ca_params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, format!("{common_name} CA"));
    ca_params.not_after = rcgen::date_time_ymd(2036, 1, 1);
    let ca_key = KeyPair::generate().context("generate CA key")?;
    let ca_cert = ca_params.self_signed(&ca_key).context("self-sign CA")?;
    let ca_pem = ca_cert.pem().into_bytes();

    let mut sp = CertificateParams::new(Vec::<String>::new()).context("server params")?;
    sp.distinguished_name.push(DnType::CommonName, common_name);
    sp.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let mut names: Vec<String> = vec!["localhost".to_string(), common_name.to_string()];
    names.extend(san_hosts.iter().cloned());
    let mut have_loopback = false;
    for n in &names {
        if let Ok(ip) = n.parse::<IpAddr>() {
            sp.subject_alt_names.push(SanType::IpAddress(ip));
            if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) {
                have_loopback = true;
            }
        } else {
            sp.subject_alt_names
                .push(SanType::DnsName(n.clone().try_into().context("SAN dns")?));
        }
    }
    if !have_loopback {
        sp.subject_alt_names
            .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    let server_key = KeyPair::generate().context("generate server key")?;
    let server_cert = sp
        .signed_by(&server_key, &ca_cert, &ca_key)
        .context("sign server cert")?;

    Ok(CertMaterial {
        ca_pem,
        server_cert_pem: server_cert.pem().into_bytes(),
        server_key_pem: server_key.serialize_pem().into_bytes(),
    })
}

// ---------------------------------------------------------------------------
// HTTP API TLS material
// ---------------------------------------------------------------------------

/// File names for the HTTP API's self-signed material (separate trust domain
/// from the gRPC certs, so the two rotate independently).
const HTTP_CA_FILE: &str = "http-ca.pem";
const HTTP_SERVER_CERT_FILE: &str = "http-server.pem";
const HTTP_SERVER_KEY_FILE: &str = "http-server-key.pem";

/// Path to the HTTP CA cert (served by `GET /api/v1/ca`).
pub fn http_ca_path(data_dir: &Path) -> PathBuf {
    data_dir.join(HTTP_CA_FILE)
}

/// Load the HTTP API's self-signed certs from `data_dir`, generating them
/// (with SANs covering `san_hosts`) on first use.
pub fn load_or_generate_http(data_dir: &Path, san_hosts: &[String]) -> Result<CertMaterial> {
    let ca = data_dir.join(HTTP_CA_FILE);
    let cert = data_dir.join(HTTP_SERVER_CERT_FILE);
    let key = data_dir.join(HTTP_SERVER_KEY_FILE);
    if ca.exists() && cert.exists() && key.exists() {
        info!("loading existing HTTP TLS certificates");
        return Ok(CertMaterial {
            ca_pem: fs::read(&ca).context("read http CA")?,
            server_cert_pem: fs::read(&cert).context("read http server cert")?,
            server_key_pem: fs::read(&key).context("read http server key")?,
        });
    }
    info!("generating self-signed HTTP TLS certificates (SANs: {san_hosts:?})");
    let m = generate_ca_and_server_cert("helyos", san_hosts)?;
    fs::write(&ca, &m.ca_pem).context("write http CA")?;
    fs::write(&cert, &m.server_cert_pem).context("write http server cert")?;
    fs::write(&key, &m.server_key_pem).context("write http server key")?;
    Ok(m)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn generate_and_persist(ca_path: &Path, cert_path: &Path, key_path: &Path) -> Result<GrpcTlsCerts> {
    let m = generate_ca_and_server_cert("helyos", &[])?;
    fs::write(ca_path, &m.ca_pem).context("write CA cert")?;
    fs::write(cert_path, &m.server_cert_pem).context("write server cert")?;
    fs::write(key_path, &m.server_key_pem).context("write server key")?;
    info!(ca = %ca_path.display(), cert = %cert_path.display(), "gRPC TLS certificates generated");
    Ok(GrpcTlsCerts {
        ca_pem: m.ca_pem,
        server_cert_pem: m.server_cert_pem,
        server_key_pem: m.server_key_pem,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_ca_and_server_cert_with_sans() {
        let m = generate_ca_and_server_cert("helyos", &["example.internal".into(), "10.0.0.5".into()])
            .expect("generate");
        assert!(!m.ca_pem.is_empty() && !m.server_cert_pem.is_empty() && !m.server_key_pem.is_empty());
        let ca = String::from_utf8(m.ca_pem.clone()).unwrap();
        assert!(ca.contains("BEGIN CERTIFICATE"));
        assert!(String::from_utf8(m.server_key_pem).unwrap().contains("PRIVATE KEY"));
    }

    #[test]
    fn http_certs_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_generate_http(dir.path(), &["h.example".into()]).unwrap();
        assert!(dir.path().join("http-ca.pem").exists());
        let b = load_or_generate_http(dir.path(), &["h.example".into()]).unwrap();
        assert_eq!(a.ca_pem, b.ca_pem);
    }
}
