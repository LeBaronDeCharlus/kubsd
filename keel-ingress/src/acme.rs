use crate::dns::DnsProvider;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub struct Cert {
    pub cert_pem: String,
    pub key_pem: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum AcmeError {
    #[error("DNS-01 challenge failed: {0}")]
    Dns(#[from] crate::dns::DnsError),
    #[error("ACME request failed: {0}")]
    Request(String),
}

pub trait AcmeClient {
    fn request_certificate(&self, domain: &str, contact_email: &str, dns: &dyn DnsProvider) -> Result<Cert, AcmeError>;
}

#[derive(Default)]
pub struct FakeAcmeClient {
    fail: std::sync::Mutex<bool>,
}

impl FakeAcmeClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail(&self, fail: bool) {
        *self.fail.lock().unwrap() = fail;
    }
}

impl AcmeClient for FakeAcmeClient {
    fn request_certificate(&self, domain: &str, _contact_email: &str, dns: &dyn DnsProvider) -> Result<Cert, AcmeError> {
        if *self.fail.lock().unwrap() {
            return Err(AcmeError::Request(format!("simulated ACME failure for '{domain}'")));
        }
        let challenge_name = format!("_acme-challenge.{domain}");
        dns.create_txt_record(&challenge_name, "fake-token")?;
        dns.wait_for_propagation(&challenge_name, "fake-token")?;
        dns.delete_txt_record(&challenge_name)?;
        Ok(Cert {
            cert_pem: format!("-----BEGIN CERTIFICATE-----\nFAKE CERT FOR {domain}\n-----END CERTIFICATE-----\n"),
            key_pem: "-----BEGIN PRIVATE KEY-----\nFAKE KEY\n-----END PRIVATE KEY-----\n".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::FakeDnsProvider;

    #[test]
    fn request_certificate_succeeds_and_drives_the_dns_challenge() {
        let dns = FakeDnsProvider::new();
        let acme = FakeAcmeClient::new();
        let cert = acme.request_certificate("example.com", "admin@example.com", &dns).unwrap();
        assert!(cert.cert_pem.contains("example.com"));
        // The challenge record must be cleaned up by the time the cert comes back.
        assert!(dns.wait_for_propagation("_acme-challenge.example.com", "fake-token").is_err());
    }

    #[test]
    fn request_certificate_can_be_made_to_fail_for_backoff_tests() {
        let dns = FakeDnsProvider::new();
        let acme = FakeAcmeClient::new();
        acme.set_fail(true);
        assert!(acme.request_certificate("example.com", "admin@example.com", &dns).is_err());
    }

    #[test]
    fn request_certificate_surfaces_a_dns_provider_failure() {
        let dns = FakeDnsProvider::new();
        dns.set_fail_create(true);
        let acme = FakeAcmeClient::new();
        assert!(matches!(
            acme.request_certificate("example.com", "admin@example.com", &dns),
            Err(AcmeError::Dns(_))
        ));
    }
}
