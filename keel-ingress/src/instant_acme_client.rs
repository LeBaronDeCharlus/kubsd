use crate::acme::{AcmeClient, AcmeError, Cert};
use crate::dns::DnsProvider;
use std::path::PathBuf;

pub struct InstantAcmeClient {
    // Read once `request_certificate_async`'s `todo!()` is implemented
    // (Task 19); until then these are unused and would otherwise trip
    // `dead_code`.
    #[allow(dead_code)]
    directory_url: String,
    #[allow(dead_code)]
    account_key_path: PathBuf,
    runtime: tokio::runtime::Runtime,
}

impl InstantAcmeClient {
    pub fn new(directory_url: String, account_key_path: PathBuf) -> Result<Self, AcmeError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AcmeError::Request(e.to_string()))?;
        Ok(Self { directory_url, account_key_path, runtime })
    }
}

impl AcmeClient for InstantAcmeClient {
    fn request_certificate(&self, domain: &str, contact_email: &str, dns: &dyn DnsProvider) -> Result<Cert, AcmeError> {
        self.runtime.block_on(self.request_certificate_async(domain, contact_email, dns))
    }
}

impl InstantAcmeClient {
    async fn request_certificate_async(&self, domain: &str, contact_email: &str, dns: &dyn DnsProvider) -> Result<Cert, AcmeError> {
        // 1. Load the persisted account from `self.account_key_path` if it
        //    exists, or create a new `instant-acme` Account against
        //    `self.directory_url` with `contact_email` and persist its key
        //    to `self.account_key_path` (temp-file-then-rename, matching
        //    every other piece of state this project persists) if not.
        // 2. Place a new order for `domain`.
        // 3. For its DNS-01 authorization: fetch the challenge's key
        //    authorization value, call `dns.create_txt_record`
        //    (synchronously - a plain blocking call from inside this
        //    `async fn` is fine on a `current_thread` runtime handling one
        //    order at a time), then `dns.wait_for_propagation`, then tell
        //    `instant-acme` the challenge is ready.
        // 4. Poll the order until it's valid (or errors/times out).
        // 5. Finalize with a freshly generated key pair, download the
        //    issued certificate chain, call `dns.delete_txt_record` to
        //    clean up regardless of outcome, and return `Cert { cert_pem,
        //    key_pem }`.
        //
        // Left to implementation-time discovery against the real
        // `instant-acme` 0.8 API (see this task's header note) rather than
        // guessed here method-by-method.
        let _ = (domain, contact_email, dns);
        todo!("implement against the real instant-acme 0.8 API - see cargo doc -p instant-acme")
    }
}
