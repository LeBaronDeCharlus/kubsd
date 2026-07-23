use crate::acme::{AcmeClient, AcmeError, Cert};
use crate::dns::DnsProvider;
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use std::path::PathBuf;
use std::time::Duration;

/// How long `poll_ready`/`poll_certificate` retry before giving up. Longer
/// than `RetryPolicy::new()`'s 5s default: a real DNS-01 challenge involves
/// real DNS propagation (`DnsProvider::wait_for_propagation` has already
/// completed by the time we call `poll_ready`, but Let's Encrypt's own
/// re-check of the TXT record over the public DNS system, plus its own
/// validation queueing, can still take longer than 5s in practice).
const POLL_TIMEOUT: Duration = Duration::from_secs(90);

pub struct InstantAcmeClient {
    directory_url: String,
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
    /// Loads the persisted ACME account from `self.account_key_path` if it
    /// exists, or creates a new one and persists it (temp-file-then-rename,
    /// matching every other piece of state this project persists) if not -
    /// so a `keel-agentd` restart reuses the same account rather than
    /// registering a fresh one with the ACME server on every run.
    async fn load_or_create_account(&self, contact_email: &str) -> Result<Account, AcmeError> {
        if let Ok(existing) = std::fs::read_to_string(&self.account_key_path) {
            let credentials: AccountCredentials =
                serde_json::from_str(&existing).map_err(|e| AcmeError::Request(format!("malformed account credentials: {e}")))?;
            let account = Account::builder()
                .map_err(|e| AcmeError::Request(e.to_string()))?
                .from_credentials(credentials)
                .await
                .map_err(|e| AcmeError::Request(e.to_string()))?;
            return Ok(account);
        }

        let contact = format!("mailto:{contact_email}");
        let new_account = NewAccount { contact: &[&contact], terms_of_service_agreed: true, only_return_existing: false };
        let (account, credentials) = Account::builder()
            .map_err(|e| AcmeError::Request(e.to_string()))?
            .create(&new_account, self.directory_url.clone(), None)
            .await
            .map_err(|e| AcmeError::Request(e.to_string()))?;

        let serialized =
            serde_json::to_string(&credentials).map_err(|e| AcmeError::Request(format!("failed to serialize account credentials: {e}")))?;
        if let Some(parent) = self.account_key_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AcmeError::Request(e.to_string()))?;
        }
        let tmp_path = self.account_key_path.with_extension("tmp");
        std::fs::write(&tmp_path, &serialized).map_err(|e| AcmeError::Request(e.to_string()))?;
        std::fs::rename(&tmp_path, &self.account_key_path).map_err(|e| AcmeError::Request(e.to_string()))?;

        Ok(account)
    }

    async fn request_certificate_async(&self, domain: &str, contact_email: &str, dns: &dyn DnsProvider) -> Result<Cert, AcmeError> {
        let account = self.load_or_create_account(contact_email).await?;

        let identifier = Identifier::Dns(domain.to_string());
        let mut order = account
            .new_order(&NewOrder::new(&[identifier]))
            .await
            .map_err(|e| AcmeError::Request(e.to_string()))?;

        let challenge_name = format!("_acme-challenge.{domain}");
        let mut authorizations = order.authorizations();
        let mut dns_values = Vec::new();
        while let Some(result) = authorizations.next().await {
            let mut authorization = result.map_err(|e| AcmeError::Request(e.to_string()))?;
            let mut challenge = authorization
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| AcmeError::Request(format!("no DNS-01 challenge offered for '{domain}'")))?;
            let dns_value = challenge.key_authorization().dns_value();
            dns.create_txt_record(&challenge_name, &dns_value)?;
            dns_values.push(dns_value.clone());
            dns.wait_for_propagation(&challenge_name, &dns_value)?;
            challenge.set_ready().await.map_err(|e| AcmeError::Request(e.to_string()))?;
        }

        let issuance_result = self.finalize_and_download(&mut order).await;

        // Clean up the TXT record this order created, regardless of
        // whether issuance succeeded - a failed order must not leave a
        // stale challenge record sitting on the zone forever.
        if !dns_values.is_empty() {
            let _ = dns.delete_txt_record(&challenge_name);
        }

        issuance_result
    }

    async fn finalize_and_download(&self, order: &mut instant_acme::Order) -> Result<Cert, AcmeError> {
        let retry_policy = RetryPolicy::new().timeout(POLL_TIMEOUT);
        let status = order.poll_ready(&retry_policy).await.map_err(|e| AcmeError::Request(e.to_string()))?;
        if status != OrderStatus::Ready {
            return Err(AcmeError::Request(format!("order did not become ready, status: {status:?}")));
        }

        let key_pem = order.finalize().await.map_err(|e| AcmeError::Request(e.to_string()))?;
        let cert_pem = order.poll_certificate(&retry_policy).await.map_err(|e| AcmeError::Request(e.to_string()))?;

        Ok(Cert { cert_pem, key_pem })
    }
}
