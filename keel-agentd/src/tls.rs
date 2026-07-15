use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Once};

static CRYPTO_PROVIDER_INIT: Once = Once::new();

pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        // Ignore the error: it only occurs if some other crate (e.g. another
        // `keel-*` crate linked into the same process, as happens in
        // `keelctl`'s integration tests) already installed a default
        // provider first. Either way, a process-wide default is now in
        // place, which is all this function promises.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn load_server_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ServerConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| format!("failed to build client certificate verifier: {e}"))?;
    rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("failed to build TLS server config: {e}"))
}

pub fn load_client_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ClientConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| format!("failed to build TLS client config: {e}"))
}

pub fn server_name_from_addr(addr: &str) -> Result<ServerName<'static>, String> {
    let host = addr.rsplit_once(':').map(|(host, _port)| host).unwrap_or(addr);
    let ip: std::net::IpAddr =
        host.parse().map_err(|e| format!("expected an IP address in '{addr}', got '{host}': {e}"))?;
    Ok(ServerName::IpAddress(ip.into()))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let file = File::open(path).map_err(|e| format!("failed to open certificate file {}: {e}", path.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse certificate file {}: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!("failed to find any PEM-encoded certificates in {}", path.display()));
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let file = File::open(path).map_err(|e| format!("failed to open key file {}: {e}", path.display()))?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(|e| format!("failed to parse key file {}: {e}", path.display()))?
        .ok_or_else(|| format!("no private key found in {}", path.display()))
}

fn load_root_store(ca_path: &Path) -> Result<RootCertStore, String> {
    let certs = load_certs(ca_path)?;
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).map_err(|e| format!("failed to add CA certificate from {}: {e}", ca_path.display()))?;
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
    }

    #[test]
    fn load_server_config_succeeds_with_valid_fixtures() {
        load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .expect("expected a valid server config");
    }

    #[test]
    fn load_server_config_fails_on_a_missing_cert_file() {
        let err = load_server_config(&fixture("does-not-exist.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap_err();
        assert!(err.contains("does-not-exist.crt"), "got: {err}");
    }

    #[test]
    fn load_client_config_succeeds_with_valid_fixtures() {
        load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .expect("expected a valid client config");
    }

    #[test]
    fn load_client_config_fails_on_a_malformed_ca_file() {
        let bad_ca = std::env::temp_dir().join(format!("keel-agentd-tls-test-bad-ca-{}", std::process::id()));
        std::fs::write(&bad_ca, "not a certificate").unwrap();
        let err = load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &bad_ca)
            .unwrap_err();
        assert!(err.contains("failed to"), "got: {err}");
    }

    #[test]
    fn server_name_from_addr_parses_the_host_and_drops_the_port() {
        let name = server_name_from_addr("192.168.64.2:7620").unwrap();
        assert_eq!(name, rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::new(192, 168, 64, 2).into()));
    }

    #[test]
    fn server_name_from_addr_rejects_a_non_ip_host() {
        assert!(server_name_from_addr("not-an-ip:7620").is_err());
    }
}
