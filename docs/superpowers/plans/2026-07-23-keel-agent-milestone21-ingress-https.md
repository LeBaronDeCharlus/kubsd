# Milestone 21: Ingress and Automatic HTTPS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `kind: Ingress` spec, a new `keel-ingress` crate (DNS-01/ACME against OVH and Let's Encrypt), and a `keel-agentd` reconciliation path that runs a singleton nginx jail terminating HTTPS for every applied `Ingress`, proxying to its backend `Service`'s existing per-node VIP.

**Architecture:** `keel-spec` gains `IngressSpec` and syntactic validation. `keel-ingress` gains `DnsProvider`/`AcmeClient` traits with `Fake` and real (OVH REST API, `instant-acme`) implementations, plus a pure nginx-config-templating function. `keel-agentd`'s `Reconciler` gains an `ingress_records` map reconciled every `Tick` alongside jail records: it ensures a singleton `keel-ingress` jail exists (by injecting a synthesized `JailSpec` through the existing `apply()`/`reconcile_one()` machinery, reusing Milestone 4/17's provisioning/rollback path unchanged), issues/renews certificates with `BackoffState`, and regenerates/reloads nginx's config. A new `ServiceVipSlot` (mirroring the existing `PodCidrSlot`) carries the Milestone 16 per-node proxy's fresh `Service` → `VIP:port` table from `registration.rs`'s heartbeat loop to both the HTTP apply-time validator and the reconciler's nginx-config step.

**Tech Stack:** Rust 2021 workspace. New dependencies, confined to the new `keel-ingress` crate only: `ureq` 3.x (blocking HTTP with rustls, for OVH's REST API) and `sha1` (OVH request signing) with no async runtime; `tokio` 1.x + `instant-acme` 0.8 (async, for the real ACME client only - every other crate in this workspace stays fully synchronous, and `AcmeClient::request_certificate`'s trait signature stays synchronous, bridging into `instant-acme`'s async API internally via a dedicated `tokio::runtime::Runtime`).

## Global Constraints

- Match this project's established per-subsystem shape exactly: a plain trait, a `Fake*` in-memory implementation usable from any OS, and a real implementation gated to production usage - the same split `keel-jail`/`keel-net`/`keel-zfs` already use.
- Every error enum uses `thiserror::Error`, one variant per real failure mode, mirroring `keel_spec::SpecError` / `keel_jail::JailError` / `keel-agentd::reconciler::ReconcileError`.
- Crash-safe persistence is temp-file-write-then-`rename`, exactly like `keel-agentd/src/store.rs::save` and `keel-agentd/src/replica_target_store.rs::save`. Per-kind records live in their own subdirectory of `state_dir` (like `replica-targets/`), never flat alongside `JailRecord`'s own `<name>.yaml` files, to avoid a name collision between a `Jail` and an `Ingress` sharing a `metadata.name`.
- FreeBSD-only real-hardware tests are gated `#![cfg(target_os = "freebsd")]` and documented as "run as root on the dev VM", exactly like `keel-jail/tests/freebsd_lifecycle.rs` and `keel-net/tests/freebsd_net.rs`.
- The dev VM (`root@192.168.64.2`) has confirmed outbound internet access to both `https://acme-v02.api.letsencrypt.org` and `https://eu.api.ovh.com` (checked live on 2026-07-23 with `fetch`); real ACME/OVH verification is viable there.
- No real OVH account or domain exists yet in this environment. Tasks 1-18 need none (fakes only). Task 19 (real FreeBSD VM verification) is where this plan pauses to ask the user for a domain delegated to OVH DNS and OVH API credentials (application key/secret, consumer key) before proceeding - do not fabricate placeholder values and press ahead.
- Add `"keel-ingress"` to the root `Cargo.toml`'s `[workspace] members` list before the first `cargo build`/`cargo test` in Task 3.
- Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` after every task; both must be clean before committing (this project's established bar - see every prior milestone's README entry noting "N tests passing, clippy clean").

---

### Task 1: `keel-spec`: `IngressSpec` types and syntactic validation

**Files:**
- Modify: `keel-spec/src/types.rs` (append `IngressSpec`, `IngressSpecBody`, `IngressBackend`, `IngressTls`)
- Modify: `keel-spec/src/error.rs` (append `InvalidHost`, `InvalidEmail` variants to `SpecError`)
- Modify: `keel-spec/src/validate.rs` (append `validate_host`, `validate_email`)
- Modify: `keel-spec/src/lib.rs` (append `parse_and_validate_ingress`, re-export new types, extend `sniff_kind`'s doc comment)

**Interfaces:**
- Produces: `keel_spec::IngressSpec { api_version: String, kind: String, metadata: Metadata, spec: IngressSpecBody }`; `IngressSpecBody { host: String, backend: IngressBackend, tls: IngressTls }`; `IngressBackend { service: String, port: u16 }`; `IngressTls { email: String }`; `keel_spec::parse_and_validate_ingress(yaml: &str) -> Result<IngressSpec, SpecError>`; `keel_spec::validate_host(host: &str) -> Result<(), SpecError>`; `keel_spec::validate_email(email: &str) -> Result<(), SpecError>`.
- Consumes: nothing new - reuses `validate_name`, `SpecError`, the existing `serde_yaml`/`thiserror` deps already in `keel-spec/Cargo.toml`.

Note: `parse_and_validate_ingress` intentionally does **not** check `backend.service` against currently-known `Service` names - `keel-spec` has no access to runtime state (the same reason `parse_and_validate` doesn't check `network.address` against a node's `pod_cidr`; that check lives in `keel-agentd::http::handle_apply` instead). The `backend.service` existence check is added in Task 6, at the `keel-agentd` HTTP layer.

- [ ] **Step 1: Write the failing tests in `keel-spec/src/lib.rs`**

```rust
const VALID_INGRESS_YAML: &str = r#"
apiVersion: keel/v1
kind: Ingress
metadata:
  name: blog
spec:
  host: example.com
  backend:
    service: hugo-site
    port: 8080
  tls:
    email: admin@example.com
"#;

#[test]
fn parse_and_validate_ingress_accepts_a_well_formed_ingress() {
    let spec = parse_and_validate_ingress(VALID_INGRESS_YAML).unwrap();
    assert_eq!(spec.metadata.name, "blog");
    assert_eq!(spec.spec.host, "example.com");
    assert_eq!(spec.spec.backend.service, "hugo-site");
    assert_eq!(spec.spec.backend.port, 8080);
    assert_eq!(spec.spec.tls.email, "admin@example.com");
}

#[test]
fn parse_and_validate_ingress_rejects_an_invalid_name() {
    let yaml = VALID_INGRESS_YAML.replace("name: blog", "name: Invalid_Name");
    assert!(matches!(parse_and_validate_ingress(&yaml), Err(SpecError::InvalidName(_))));
}

#[test]
fn parse_and_validate_ingress_rejects_a_malformed_host() {
    let yaml = VALID_INGRESS_YAML.replace("host: example.com", "host: not a host!");
    assert!(matches!(parse_and_validate_ingress(&yaml), Err(SpecError::InvalidHost(_))));
}

#[test]
fn parse_and_validate_ingress_rejects_a_host_with_no_dot() {
    let yaml = VALID_INGRESS_YAML.replace("host: example.com", "host: localhost");
    assert!(matches!(parse_and_validate_ingress(&yaml), Err(SpecError::InvalidHost(_))));
}

#[test]
fn parse_and_validate_ingress_rejects_port_zero() {
    let yaml = VALID_INGRESS_YAML.replace("port: 8080", "port: 0");
    assert!(matches!(parse_and_validate_ingress(&yaml), Err(SpecError::InvalidPort(0))));
}

#[test]
fn parse_and_validate_ingress_rejects_a_malformed_email() {
    let yaml = VALID_INGRESS_YAML.replace("email: admin@example.com", "email: not-an-email");
    assert!(matches!(parse_and_validate_ingress(&yaml), Err(SpecError::InvalidEmail(_))));
}

#[test]
fn sniff_kind_reads_ingress() {
    assert_eq!(sniff_kind(VALID_INGRESS_YAML).unwrap(), "Ingress");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-spec parse_and_validate_ingress`
Expected: FAIL with "cannot find function `parse_and_validate_ingress`" (and `SpecError::InvalidHost`/`InvalidEmail` don't exist yet).

- [ ] **Step 3: Add the types to `keel-spec/src/types.rs`**

Append:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngressSpec {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: IngressSpecBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngressSpecBody {
    pub host: String,
    pub backend: IngressBackend,
    pub tls: IngressTls,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngressBackend {
    pub service: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngressTls {
    pub email: String,
}
```

- [ ] **Step 4: Add `InvalidHost`/`InvalidEmail` to `keel-spec/src/error.rs`**

Append to the `SpecError` enum (before the closing `}`):

```rust
    #[error("invalid host '{0}': must be a syntactically well-formed DNS name")]
    InvalidHost(String),
    #[error("invalid email '{0}': must be a syntactically well-formed email address")]
    InvalidEmail(String),
```

- [ ] **Step 5: Add `validate_host`/`validate_email` to `keel-spec/src/validate.rs`**

Append:

```rust
pub fn validate_host(host: &str) -> Result<(), SpecError> {
    let labels: Vec<&str> = host.split('.').collect();
    let valid = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && label.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                && label.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
        });
    if valid {
        Ok(())
    } else {
        Err(SpecError::InvalidHost(host.to_string()))
    }
}

pub fn validate_email(email: &str) -> Result<(), SpecError> {
    let valid = match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && !local.contains(char::is_whitespace)
                && !domain.contains(char::is_whitespace)
                && domain.contains('.')
        }
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SpecError::InvalidEmail(email.to_string()))
    }
}
```

Add corresponding unit tests in `validate.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn accepts_well_formed_hosts() {
    assert!(validate_host("example.com").is_ok());
    assert!(validate_host("blog.example.com").is_ok());
}

#[test]
fn rejects_malformed_hosts() {
    assert!(validate_host("localhost").is_err()); // no dot
    assert!(validate_host("not a host!").is_err());
    assert!(validate_host(".example.com").is_err()); // empty label
    assert!(validate_host("-example.com").is_err()); // leading hyphen label
}

#[test]
fn accepts_well_formed_emails() {
    assert!(validate_email("admin@example.com").is_ok());
}

#[test]
fn rejects_malformed_emails() {
    assert!(validate_email("not-an-email").is_err());
    assert!(validate_email("admin@").is_err());
    assert!(validate_email("@example.com").is_err());
    assert!(validate_email("admin@no-dot").is_err());
}
```

- [ ] **Step 6: Add `parse_and_validate_ingress` to `keel-spec/src/lib.rs`**

Append (near `parse_and_validate_service`):

```rust
pub fn parse_and_validate_ingress(yaml: &str) -> Result<IngressSpec, SpecError> {
    let spec: IngressSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    validate::validate_host(&spec.spec.host)?;
    validate::validate_email(&spec.spec.tls.email)?;
    if spec.spec.backend.port == 0 {
        return Err(SpecError::InvalidPort(0));
    }
    Ok(spec)
}
```

Update the `pub use` lines at the top of `lib.rs`:

```rust
pub use types::{
    IngressBackend, IngressSpec, IngressSpecBody, IngressTls, JailSpec, JailTemplate, Metadata,
    NetworkSpec, RestartPolicy, ResourcesSpec, ServiceSpec, ServiceSpecBody, Spec,
    TemplateNetworkSpec, VolumeMount,
};
pub use validate::{validate_address, validate_email, validate_host, validate_name, validate_transition, validate_volumes};
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p keel-spec`
Expected: PASS, all tests including the new ones from Step 1 and Step 5.

- [ ] **Step 8: Run clippy**

Run: `cargo clippy -p keel-spec --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add keel-spec/src/types.rs keel-spec/src/error.rs keel-spec/src/validate.rs keel-spec/src/lib.rs
git commit -m "feat(keel-spec): add IngressSpec parsing and validation"
```

---

### Task 2: `keel-ingress` crate scaffolding, `DnsProvider` trait, `FakeDnsProvider`

**Files:**
- Modify: `Cargo.toml` (add `"keel-ingress"` to `[workspace] members`)
- Create: `keel-ingress/Cargo.toml`
- Create: `keel-ingress/src/lib.rs`
- Create: `keel-ingress/src/dns.rs`

**Interfaces:**
- Produces: `keel_ingress::DnsProvider` trait; `keel_ingress::DnsError`; `keel_ingress::FakeDnsProvider`.
- Consumes: nothing from other tasks.

- [ ] **Step 1: Add the crate to the workspace**

Edit `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["keel-spec", "keel-jail", "keel-zfs", "keel-net", "keel-agentd", "keelctl", "keel-controlplane", "keel-ingress"]
```

- [ ] **Step 2: Create `keel-ingress/Cargo.toml`**

```toml
[package]
name = "keel-ingress"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"
```

(`ureq`, `sha1`, `tokio`, `instant-acme` are added in Task 6/7 when the real implementations need them - the fakes in this task and Task 3/4 need no extra dependency, matching how `keel-jail`'s `Cargo.toml` only lists `thiserror` even though its real `ProcessJailRuntime` shells out to `jail(8)`.)

- [ ] **Step 3: Write the failing test in `keel-ingress/src/dns.rs`**

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum DnsError {
    #[error("TXT record '{0}' not found")]
    NotFound(String),
    #[error("DNS provider request failed: {0}")]
    Request(String),
    #[error("timed out waiting for '{0}' to propagate")]
    PropagationTimeout(String),
}

pub trait DnsProvider {
    fn create_txt_record(&self, name: &str, value: &str) -> Result<(), DnsError>;
    fn delete_txt_record(&self, name: &str) -> Result<(), DnsError>;
    fn wait_for_propagation(&self, name: &str, value: &str) -> Result<(), DnsError>;
}

#[derive(Default)]
pub struct FakeDnsProvider {
    records: Mutex<HashMap<String, String>>,
    fail_create: Mutex<bool>,
}

impl FakeDnsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail_create(&self, fail: bool) {
        *self.fail_create.lock().unwrap() = fail;
    }
}

impl DnsProvider for FakeDnsProvider {
    fn create_txt_record(&self, name: &str, value: &str) -> Result<(), DnsError> {
        if *self.fail_create.lock().unwrap() {
            return Err(DnsError::Request(format!("simulated failure creating '{name}'")));
        }
        self.records.lock().unwrap().insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn delete_txt_record(&self, name: &str) -> Result<(), DnsError> {
        self.records.lock().unwrap().remove(name);
        Ok(())
    }

    fn wait_for_propagation(&self, name: &str, value: &str) -> Result<(), DnsError> {
        match self.records.lock().unwrap().get(name) {
            Some(v) if v == value => Ok(()),
            Some(_) | None => Err(DnsError::NotFound(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_wait_for_propagation_succeeds() {
        let dns = FakeDnsProvider::new();
        dns.create_txt_record("_acme-challenge.example.com", "token-value").unwrap();
        assert!(dns.wait_for_propagation("_acme-challenge.example.com", "token-value").is_ok());
    }

    #[test]
    fn wait_for_propagation_fails_before_create() {
        let dns = FakeDnsProvider::new();
        assert_eq!(
            dns.wait_for_propagation("_acme-challenge.example.com", "token-value"),
            Err(DnsError::NotFound("_acme-challenge.example.com".to_string()))
        );
    }

    #[test]
    fn delete_then_wait_for_propagation_fails() {
        let dns = FakeDnsProvider::new();
        dns.create_txt_record("_acme-challenge.example.com", "token-value").unwrap();
        dns.delete_txt_record("_acme-challenge.example.com").unwrap();
        assert!(dns.wait_for_propagation("_acme-challenge.example.com", "token-value").is_err());
    }

    #[test]
    fn create_txt_record_can_be_made_to_fail_for_retry_tests() {
        let dns = FakeDnsProvider::new();
        dns.set_fail_create(true);
        assert!(dns.create_txt_record("_acme-challenge.example.com", "token-value").is_err());
    }
}
```

- [ ] **Step 4: Create `keel-ingress/src/lib.rs`**

```rust
pub mod dns;

pub use dns::{DnsError, DnsProvider, FakeDnsProvider};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-ingress`
Expected: PASS, 4 tests.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p keel-ingress --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml keel-ingress/Cargo.toml keel-ingress/src/lib.rs keel-ingress/src/dns.rs
git commit -m "feat(keel-ingress): add crate scaffolding and DnsProvider/FakeDnsProvider"
```

---

### Task 3: `keel-ingress`: `AcmeClient` trait, `Cert`/`AcmeError`, `FakeAcmeClient`

**Files:**
- Create: `keel-ingress/src/acme.rs`
- Modify: `keel-ingress/src/lib.rs`

**Interfaces:**
- Consumes: `keel_ingress::DnsProvider` (Task 2).
- Produces: `keel_ingress::Cert { cert_pem: String, key_pem: String }`; `keel_ingress::AcmeError`; `keel_ingress::AcmeClient` trait; `keel_ingress::FakeAcmeClient`.

- [ ] **Step 1: Write the failing test in `keel-ingress/src/acme.rs`**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-ingress acme`
Expected: FAIL to compile - `acme` module doesn't exist yet.

- [ ] **Step 3: Update `keel-ingress/src/lib.rs`**

```rust
pub mod acme;
pub mod dns;

pub use acme::{AcmeClient, AcmeError, Cert, FakeAcmeClient};
pub use dns::{DnsError, DnsProvider, FakeDnsProvider};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-ingress`
Expected: PASS, 7 tests total (4 from Task 2 + 3 new).

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p keel-ingress --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add keel-ingress/src/acme.rs keel-ingress/src/lib.rs
git commit -m "feat(keel-ingress): add AcmeClient trait and FakeAcmeClient"
```

---

### Task 4: `keel-ingress`: nginx config templating

**Files:**
- Create: `keel-ingress/src/config.rs`
- Modify: `keel-ingress/src/lib.rs`

**Interfaces:**
- Produces: `keel_ingress::IngressBackendConfig { host: String, vip: String, port: u16, cert_path: String, key_path: String }`; `keel_ingress::render_nginx_config(backends: &[IngressBackendConfig]) -> String`.
- Consumes: nothing from other tasks (pure function over plain data - `keel-agentd` is responsible for resolving `host`/`vip`/`port`/cert paths and passing them in; see Task 12).

This is a pure, deterministic string-templating function so it can be unit-tested without any FreeBSD/jail/nginx dependency, matching how `keel-agentd::proxy::reconcile_services` itself is unit-testable purely in terms of data structures.

- [ ] **Step 1: Write the failing test in `keel-ingress/src/config.rs`**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct IngressBackendConfig {
    pub host: String,
    pub vip: String,
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
}

pub fn render_nginx_config(backends: &[IngressBackendConfig]) -> String {
    let mut config = String::from(
        "user www; worker_processes 1;\nevents { worker_connections 1024; }\nhttp {\n    server {\n        listen 80 default_server;\n        return 301 https://$host$request_uri;\n    }\n",
    );
    for backend in backends {
        config.push_str(&format!(
            "    server {{\n        listen 443 ssl;\n        server_name {host};\n        ssl_certificate {cert_path};\n        ssl_certificate_key {key_path};\n        location / {{\n            proxy_pass http://{vip}:{port};\n        }}\n    }}\n",
            host = backend.host,
            cert_path = backend.cert_path,
            key_path = backend.key_path,
            vip = backend.vip,
            port = backend.port,
        ));
    }
    config.push_str("}\n");
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(host: &str, vip: &str, port: u16) -> IngressBackendConfig {
        IngressBackendConfig {
            host: host.to_string(),
            vip: vip.to_string(),
            port,
            cert_path: format!("/usr/local/etc/nginx/certs/{host}.crt"),
            key_path: format!("/usr/local/etc/nginx/certs/{host}.key"),
        }
    }

    #[test]
    fn empty_backends_still_produces_a_valid_shaped_config_with_the_http_redirect() {
        let config = render_nginx_config(&[]);
        assert!(config.contains("listen 80 default_server;"));
        assert!(config.contains("return 301 https://$host$request_uri;"));
    }

    #[test]
    fn one_backend_produces_one_server_block_with_proxy_pass_to_its_vip_and_port() {
        let config = render_nginx_config(&[backend("example.com", "10.0.0.9", 8080)]);
        assert!(config.contains("server_name example.com;"));
        assert!(config.contains("proxy_pass http://10.0.0.9:8080;"));
        assert!(config.contains("ssl_certificate /usr/local/etc/nginx/certs/example.com.crt;"));
        assert!(config.contains("ssl_certificate_key /usr/local/etc/nginx/certs/example.com.key;"));
    }

    #[test]
    fn multiple_backends_each_get_their_own_server_block() {
        let config = render_nginx_config(&[backend("a.example.com", "10.0.0.9", 8080), backend("b.example.com", "10.0.0.10", 9090)]);
        assert!(config.contains("server_name a.example.com;"));
        assert!(config.contains("proxy_pass http://10.0.0.9:8080;"));
        assert!(config.contains("server_name b.example.com;"));
        assert!(config.contains("proxy_pass http://10.0.0.10:9090;"));
    }

    #[test]
    fn rendering_is_deterministic_for_the_same_input() {
        let backends = vec![backend("example.com", "10.0.0.9", 8080)];
        assert_eq!(render_nginx_config(&backends), render_nginx_config(&backends));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-ingress config`
Expected: FAIL - `config` module doesn't exist yet.

- [ ] **Step 3: Update `keel-ingress/src/lib.rs`**

```rust
pub mod acme;
pub mod config;
pub mod dns;

pub use acme::{AcmeClient, AcmeError, Cert, FakeAcmeClient};
pub use config::{render_nginx_config, IngressBackendConfig};
pub use dns::{DnsError, DnsProvider, FakeDnsProvider};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-ingress`
Expected: PASS, 11 tests total.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p keel-ingress --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add keel-ingress/src/config.rs keel-ingress/src/lib.rs
git commit -m "feat(keel-ingress): add nginx config templating"
```

---

### Task 5: `keel-agentd`: `ServiceVipSlot`

**Files:**
- Create: `keel-agentd/src/service_vips.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `keel_controlplane::wire::ServiceProxyEntry { name: String, vip: String, port: u16, replicas: Vec<ServiceReplica> }` (already exists).
- Produces: `keel_agentd::ServiceVipSlot` (`Clone`), with `set_all(&self, entries: &[ServiceProxyEntry])`, `get(&self, name: &str) -> Option<(String, u16)>`, `names(&self) -> std::collections::HashSet<String>`.

This mirrors `keel-agentd/src/podcidr.rs::PodCidrSlot` exactly: an `Arc<Mutex<_>>`-backed, cheaply-cloneable slot that one writer (the heartbeat loop, Task 8) updates and multiple readers (the HTTP layer, Task 9; the reconciler, Task 12) consult without going through the `Command` channel.

- [ ] **Step 1: Write the failing test in `keel-agentd/src/service_vips.rs`**

```rust
use keel_controlplane::wire::ServiceProxyEntry;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct ServiceVipSlot(Arc<Mutex<HashMap<String, (String, u16)>>>);

impl ServiceVipSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_all(&self, entries: &[ServiceProxyEntry]) {
        let mut map = self.0.lock().unwrap();
        map.clear();
        for entry in entries {
            map.insert(entry.name.clone(), (entry.vip.clone(), entry.port));
        }
    }

    pub fn get(&self, name: &str) -> Option<(String, u16)> {
        self.0.lock().unwrap().get(name).cloned()
    }

    pub fn names(&self) -> HashSet<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_controlplane::wire::ServiceReplica;

    fn entry(name: &str, vip: &str, port: u16) -> ServiceProxyEntry {
        ServiceProxyEntry { name: name.to_string(), vip: vip.to_string(), port, replicas: vec![] }
    }

    #[test]
    fn a_fresh_slot_knows_no_services() {
        let slot = ServiceVipSlot::new();
        assert_eq!(slot.get("hugo-site"), None);
        assert!(slot.names().is_empty());
    }

    #[test]
    fn set_all_then_get_returns_the_vip_and_port() {
        let slot = ServiceVipSlot::new();
        slot.set_all(&[entry("hugo-site", "10.0.0.9", 8080)]);
        assert_eq!(slot.get("hugo-site"), Some(("10.0.0.9".to_string(), 8080)));
    }

    #[test]
    fn set_all_replaces_the_previous_table_rather_than_merging() {
        let slot = ServiceVipSlot::new();
        slot.set_all(&[entry("hugo-site", "10.0.0.9", 8080)]);
        slot.set_all(&[entry("umami", "10.0.0.10", 3000)]);
        assert_eq!(slot.get("hugo-site"), None);
        assert_eq!(slot.get("umami"), Some(("10.0.0.10".to_string(), 3000)));
    }

    #[test]
    fn names_lists_every_currently_known_service() {
        let slot = ServiceVipSlot::new();
        slot.set_all(&[entry("hugo-site", "10.0.0.9", 8080), entry("umami", "10.0.0.10", 3000)]);
        assert_eq!(slot.names(), HashSet::from(["hugo-site".to_string(), "umami".to_string()]));
    }

    #[test]
    fn clones_share_the_same_underlying_slot() {
        let slot = ServiceVipSlot::new();
        let clone = slot.clone();
        clone.set_all(&[entry("hugo-site", "10.0.0.9", 8080)]);
        assert_eq!(slot.get("hugo-site"), Some(("10.0.0.9".to_string(), 8080)));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd service_vips`
Expected: FAIL - module doesn't exist yet.

- [ ] **Step 3: Register the module in `keel-agentd/src/lib.rs`**

Add `pub mod service_vips;` alongside the other `pub mod` lines, and `pub use service_vips::ServiceVipSlot;` alongside `pub use podcidr::PodCidrSlot;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd service_vips`
Expected: PASS, 5 tests.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/service_vips.rs keel-agentd/src/lib.rs
git commit -m "feat(keel-agentd): add ServiceVipSlot"
```

---

### Task 6: `keel-agentd`: wire `ServiceVipSlot` into the heartbeat loop

**Files:**
- Modify: `keel-agentd/src/registration.rs`
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_agentd::ServiceVipSlot` (Task 5).
- Produces: `registration::spawn` gains a `service_vips: ServiceVipSlot` parameter, populated every heartbeat tick.

- [ ] **Step 1: Write the failing test in `keel-agentd/src/registration.rs`**

Add near the existing heartbeat-related tests (search for `registers_and_then_keeps_heartbeating` to place this alongside it):

```rust
#[test]
fn heartbeat_populates_the_service_vip_slot_from_the_proxy_table() {
    let control_plane_addr = start_test_control_plane();
    let commands = spawn_test_worker("heartbeat_populates_the_service_vip_slot");
    let client_config = node_client_config();
    let service_vips = crate::ServiceVipSlot::new();

    registration::spawn(
        "node-1".to_string(),
        "127.0.0.1:0".to_string(),
        "127.0.0.1:0".to_string(),
        control_plane_addr.clone(),
        Duration::from_millis(50),
        1.0,
        1_073_741_824,
        node_reloading_tls(),
        commands.clone(),
        crate::PodCidrSlot::new(),
        service_vips.clone(),
    );

    // Apply a Service on the real control plane so the next heartbeat's
    // proxy table is non-empty, then wait a few ticks for it to land.
    let apply_body = "apiVersion: keel/v1\nkind: Service\nmetadata:\n  name: hugo-site\nspec:\n  replicas: 1\n  port: 8080\n  template:\n    image: base/test\n    command: [\"/bin/true\"]\n    network:\n      vnet: true\n      bridge: keel0\n    resources:\n      cpu: \"1\"\n      memory: \"128M\"\n    restartPolicy: Always\n";
    let _ = send_request(&control_plane_addr, "PUT", "/services/hugo-site", apply_body, &client_config);

    let mut found = None;
    for _ in 0..40 {
        if let Some(vip) = service_vips.get("hugo-site") {
            found = Some(vip);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(found.is_some(), "expected 'hugo-site' to appear in the ServiceVipSlot within the timeout");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p keel-agentd heartbeat_populates_the_service_vip_slot`
Expected: FAIL to compile - `registration::spawn` doesn't take a `ServiceVipSlot` parameter yet.

- [ ] **Step 3: Update `registration::spawn`'s signature and body**

In `keel-agentd/src/registration.rs`, change the `spawn` signature:

```rust
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    replicate_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    reloading_tls: Arc<tls::ReloadingTls>,
    commands: Sender<crate::worker::Command>,
    pod_cidr_slot: crate::PodCidrSlot,
    service_vips: crate::ServiceVipSlot,
) -> JoinHandle<()> {
```

And update the heartbeat-success arm:

```rust
                match heartbeat_once(&control_plane_addr, &node_id, &commands, &client_config) {
                    Ok(entries) => {
                        service_vips.set_all(&entries);
                        crate::proxy::reconcile_services(&entries, &mut proxied_services, &commands);
                    }
                    Err(e) => {
                        eprintln!("keel-agentd: heartbeat failed: {e}");
                        registered = false;
                    }
                }
```

- [ ] **Step 4: Update the call site in `keel-agentd/src/main.rs`**

```rust
        let service_vips = keel_agentd::ServiceVipSlot::new();
        keel_agentd::registration::spawn(
            node_id,
            advertise_addr.clone(),
            replicate_addr.clone(),
            control_plane_addr,
            Duration::from_secs(5),
            capacity_cpu,
            capacity_memory,
            std::sync::Arc::clone(&reloading_tls),
            commands.clone(),
            pod_cidr_slot.clone(),
            service_vips.clone(),
        );
```

(`service_vips` is threaded further into the HTTP layer in Task 9 and the reconciler in Task 12 - this task only makes it exist and get populated. For now, add `let _ = &service_vips;` is not needed; the variable is used again by later edits in this same `main.rs` in Task 9/12, so leave it declared here uncommented - a fresh `cargo build` after this task alone will warn `unused variable` only if those later tasks haven't landed yet, which is expected and resolved by Task 9.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p keel-agentd heartbeat_populates_the_service_vip_slot -- --test-threads=1`
Expected: PASS.

- [ ] **Step 6: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS (existing tests that call `registration::spawn` directly - search for other call sites in `registration.rs`'s own test module - need `crate::ServiceVipSlot::new()` appended to their argument lists; update every such call site found by the compiler error list before re-running).

- [ ] **Step 7: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add keel-agentd/src/registration.rs keel-agentd/src/main.rs
git commit -m "feat(keel-agentd): populate ServiceVipSlot from the heartbeat loop"
```

---

### Task 7: `keel-agentd`: `IngressRecord` and crash-safe ingress store

**Files:**
- Create: `keel-agentd/src/ingress_record.rs`
- Create: `keel-agentd/src/ingress_store.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `keel_spec::IngressSpec` (Task 1).
- Produces: `keel_agentd::IngressRecord { spec: IngressSpec, cert_expires_at_unix: Option<i64> }`; `ingress_store::{load_all, save, remove}` with the same signatures as `store::{load_all, save, remove}`/`replica_target_store::{load_all, save}`.

`cert_expires_at_unix` starts `None` (no certificate issued yet) and is set once Task 11's ACME flow succeeds. Storing it directly on `IngressRecord` - rather than a second per-Ingress file - mirrors how `JailRecord` already bundles the spec with its own orchestration metadata (`epair_ordinal`).

- [ ] **Step 1: Write the failing test in `keel-agentd/src/ingress_record.rs`**

```rust
use keel_spec::IngressSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngressRecord {
    pub spec: IngressSpec,
    pub cert_expires_at_unix: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{IngressBackend, IngressSpecBody, IngressTls, Metadata};

    fn sample_spec(name: &str) -> IngressSpec {
        IngressSpec {
            api_version: "keel/v1".to_string(),
            kind: "Ingress".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: IngressSpecBody {
                host: "example.com".to_string(),
                backend: IngressBackend { service: "hugo-site".to_string(), port: 8080 },
                tls: IngressTls { email: "admin@example.com".to_string() },
            },
        }
    }

    #[test]
    fn ingress_record_round_trips_through_yaml() {
        let record = IngressRecord { spec: sample_spec("blog"), cert_expires_at_unix: Some(1_800_000_000) };
        let yaml = serde_yaml::to_string(&record).unwrap();
        let parsed: IngressRecord = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn a_fresh_record_has_no_certificate_expiry() {
        let record = IngressRecord { spec: sample_spec("blog"), cert_expires_at_unix: None };
        assert_eq!(record.cert_expires_at_unix, None);
    }
}
```

- [ ] **Step 2: Write the failing test in `keel-agentd/src/ingress_store.rs`**

```rust
use crate::ingress_record::IngressRecord;
use crate::store::StoreError;
use std::fs;
use std::path::{Path, PathBuf};

fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("ingress")
}

pub fn load_all(state_dir: &Path) -> Result<Vec<IngressRecord>, StoreError> {
    let dir = dir(state_dir);
    fs::create_dir_all(&dir).map_err(|e| StoreError::Io(dir.clone(), e))?;
    let mut records = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| StoreError::Io(dir.clone(), e))? {
        let entry = entry.map_err(|e| StoreError::Io(dir.clone(), e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|e| StoreError::Io(path.clone(), e))?;
        let record: IngressRecord = serde_yaml::from_str(&content).map_err(|e| StoreError::Parse(path.clone(), e))?;
        records.push(record);
    }
    Ok(records)
}

pub fn save(state_dir: &Path, record: &IngressRecord) -> Result<(), StoreError> {
    let dir = dir(state_dir);
    fs::create_dir_all(&dir).map_err(|e| StoreError::Io(dir.clone(), e))?;
    let path = dir.join(format!("{}.yaml", record.spec.metadata.name));
    let tmp_path = dir.join(format!("{}.yaml.tmp", record.spec.metadata.name));
    let content = serde_yaml::to_string(record).expect("IngressRecord serialization should not fail");
    fs::write(&tmp_path, content).map_err(|e| StoreError::Io(tmp_path.clone(), e))?;
    fs::rename(&tmp_path, &path).map_err(|e| StoreError::Io(path.clone(), e))?;
    Ok(())
}

pub fn remove(state_dir: &Path, spec_name: &str) -> Result<(), StoreError> {
    let path = dir(state_dir).join(format!("{spec_name}.yaml"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{IngressBackend, IngressSpecBody, IngressTls, Metadata};

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-ingress-store-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample(name: &str) -> IngressRecord {
        IngressRecord {
            spec: keel_spec::IngressSpec {
                api_version: "keel/v1".to_string(),
                kind: "Ingress".to_string(),
                metadata: Metadata { name: name.to_string() },
                spec: IngressSpecBody {
                    host: "example.com".to_string(),
                    backend: IngressBackend { service: "hugo-site".to_string(), port: 8080 },
                    tls: IngressTls { email: "admin@example.com".to_string() },
                },
            },
            cert_expires_at_unix: None,
        }
    }

    #[test]
    fn save_then_load_all_roundtrips() {
        let dir = test_state_dir("save_then_load_all_roundtrips");
        let record = sample("blog");
        save(&dir, &record).unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![record]);
    }

    #[test]
    fn save_then_remove_then_load_all_is_empty() {
        let dir = test_state_dir("save_then_remove_then_load_all_is_empty");
        let record = sample("blog");
        save(&dir, &record).unwrap();
        remove(&dir, "blog").unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![]);
    }

    #[test]
    fn ingress_records_live_in_their_own_subdirectory_not_alongside_jail_records() {
        let dir = test_state_dir("ingress_records_live_in_their_own_subdirectory");
        save(&dir, &sample("blog")).unwrap();
        assert_eq!(crate::store::load_all(&dir).unwrap(), vec![]);
    }

    #[test]
    fn save_overwrites_rather_than_duplicating() {
        let dir = test_state_dir("save_overwrites_rather_than_duplicating");
        let mut record = sample("blog");
        save(&dir, &record).unwrap();
        record.cert_expires_at_unix = Some(1_800_000_000);
        save(&dir, &record).unwrap();
        let loaded = load_all(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cert_expires_at_unix, Some(1_800_000_000));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-agentd ingress_record ingress_store`
Expected: FAIL - modules don't exist yet.

- [ ] **Step 4: Register the modules in `keel-agentd/src/lib.rs`**

Add `pub mod ingress_record;` and `pub mod ingress_store;`, and `pub use ingress_record::IngressRecord;`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-agentd ingress_record ingress_store`
Expected: PASS, 6 tests total.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/src/ingress_record.rs keel-agentd/src/ingress_store.rs keel-agentd/src/lib.rs
git commit -m "feat(keel-agentd): add IngressRecord and crash-safe ingress store"
```

---

### Task 8: `keel-agentd`: `Reconciler` gains `ingress_records`, `apply_ingress`/`get_ingress`/`list_ingress`/`delete_ingress`

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: `IngressRecord`, `ingress_store` (Task 7); `keel_spec::parse_and_validate_ingress`'s validated `IngressSpec` shape (Task 1, though this task takes an already-parsed `IngressSpec`, matching how `Reconciler::apply` takes an already-parsed `JailSpec`).
- Produces: `Reconciler::apply_ingress(&mut self, spec: IngressSpec) -> Result<(), ReconcileError>`; `Reconciler::get_ingress(&self, name: &str) -> Option<IngressRecord>`; `Reconciler::list_ingress(&self) -> Vec<IngressRecord>`; `Reconciler::delete_ingress(&mut self, name: &str) -> Result<(), ReconcileError>`.

No jail provisioning, cert issuance, or nginx config generation yet - this task only adds the per-name CRUD layer for `Ingress` specs, exactly like `apply`/`delete`/(the implicit `get`/`list` via `self.records`) already exist for `JailRecord` before `provision`/`reconcile_one` come into play. Jail-lifecycle wiring is Task 9; certs are Task 11; nginx config is Task 12.

- [ ] **Step 1: Write the failing tests in `keel-agentd/src/reconciler.rs`**

Add near the existing `#[cfg(test)] mod tests` (search for `apply_persists_and_tracks_the_record` to place these alongside it):

```rust
fn sample_ingress_spec(name: &str, host: &str) -> keel_spec::IngressSpec {
    keel_spec::IngressSpec {
        api_version: "keel/v1".to_string(),
        kind: "Ingress".to_string(),
        metadata: keel_spec::Metadata { name: name.to_string() },
        spec: keel_spec::IngressSpecBody {
            host: host.to_string(),
            backend: keel_spec::IngressBackend { service: "hugo-site".to_string(), port: 8080 },
            tls: keel_spec::IngressTls { email: "admin@example.com".to_string() },
        },
    }
}

#[test]
fn apply_ingress_persists_and_tracks_the_record() {
    let dir = test_state_dir("apply_ingress_persists_and_tracks_the_record");
    let mut reconciler = test_reconciler(&dir);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    assert_eq!(reconciler.list_ingress().len(), 1);
    assert_eq!(reconciler.get_ingress("blog").unwrap().spec.spec.host, "example.com");
}

#[test]
fn apply_ingress_survives_a_simulated_restart() {
    let dir = test_state_dir("apply_ingress_survives_a_simulated_restart");
    {
        let mut reconciler = test_reconciler(&dir);
        reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    }
    let reloaded = test_reconciler(&dir);
    assert_eq!(reloaded.list_ingress().len(), 1);
}

#[test]
fn get_ingress_on_an_unknown_name_returns_none() {
    let dir = test_state_dir("get_ingress_on_an_unknown_name_returns_none");
    let reconciler = test_reconciler(&dir);
    assert_eq!(reconciler.get_ingress("missing"), None);
}

#[test]
fn delete_ingress_removes_the_record() {
    let dir = test_state_dir("delete_ingress_removes_the_record");
    let mut reconciler = test_reconciler(&dir);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.delete_ingress("blog").unwrap();
    assert_eq!(reconciler.list_ingress().len(), 0);
}

#[test]
fn delete_ingress_on_an_unknown_name_is_not_found() {
    let dir = test_state_dir("delete_ingress_on_an_unknown_name_is_not_found");
    let mut reconciler = test_reconciler(&dir);
    assert!(matches!(reconciler.delete_ingress("missing"), Err(ReconcileError::NotFound(_))));
}

#[test]
fn re_applying_an_existing_ingress_updates_its_host_and_keeps_the_same_name() {
    let dir = test_state_dir("re_applying_an_existing_ingress_updates_its_host");
    let mut reconciler = test_reconciler(&dir);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.apply_ingress(sample_ingress_spec("blog", "blog.example.com")).unwrap();
    assert_eq!(reconciler.list_ingress().len(), 1);
    assert_eq!(reconciler.get_ingress("blog").unwrap().spec.spec.host, "blog.example.com");
}
```

Check whether `test_reconciler`/`test_state_dir` helpers already exist in this test module (they do - used by the `Jail` tests); reuse them as-is rather than duplicating.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd apply_ingress get_ingress delete_ingress`
Expected: FAIL to compile - none of these methods exist yet.

- [ ] **Step 3: Add the `ingress_records` field and CRUD methods to `Reconciler`**

Add the import at the top of `reconciler.rs`:

```rust
use crate::ingress_record::IngressRecord;
use crate::ingress_store;
use keel_spec::IngressSpec;
```

Add the field to the `Reconciler` struct:

```rust
pub struct Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager, M: MountManager> {
    jails: J,
    zfs: Z,
    net: N,
    mounts: M,
    pool: String,
    state_dir: PathBuf,
    records: HashMap<String, JailRecord>,
    backoff: HashMap<String, BackoffState>,
    next_epair_ordinal: u32,
    ingress_records: HashMap<String, IngressRecord>,
    ingress_backoff: HashMap<String, BackoffState>,
}
```

Update `Reconciler::new` to load ingress records too:

```rust
    pub fn new(jails: J, zfs: Z, net: N, mounts: M, pool: String, state_dir: PathBuf) -> Result<Self, ReconcileError> {
        let loaded = store::load_all(&state_dir)?;
        let next_epair_ordinal = loaded.iter().map(|r| r.epair_ordinal).max().map(|m| m + 1).unwrap_or(1);
        let records = loaded.into_iter().map(|r| (r.spec.metadata.name.clone(), r)).collect();
        let ingress_loaded = ingress_store::load_all(&state_dir)?;
        let ingress_records = ingress_loaded.into_iter().map(|r| (r.spec.metadata.name.clone(), r)).collect();
        Ok(Self {
            jails,
            zfs,
            net,
            mounts,
            pool,
            state_dir,
            records,
            backoff: HashMap::new(),
            next_epair_ordinal,
            ingress_records,
            ingress_backoff: HashMap::new(),
        })
    }
```

Add the CRUD methods (near `set_replicate_to`):

```rust
    pub fn apply_ingress(&mut self, spec: IngressSpec) -> Result<(), ReconcileError> {
        keel_spec::validate_name(&spec.metadata.name)?;
        keel_spec::validate_host(&spec.spec.host)?;
        keel_spec::validate_email(&spec.spec.tls.email)?;
        if spec.spec.backend.port == 0 {
            return Err(keel_spec::SpecError::InvalidPort(0).into());
        }
        let cert_expires_at_unix = self.ingress_records.get(&spec.metadata.name).and_then(|r| r.cert_expires_at_unix);
        let record = IngressRecord { spec: spec.clone(), cert_expires_at_unix };
        ingress_store::save(&self.state_dir, &record)?;
        self.ingress_records.insert(spec.metadata.name.clone(), record);
        Ok(())
    }

    pub fn get_ingress(&self, name: &str) -> Option<IngressRecord> {
        self.ingress_records.get(name).cloned()
    }

    pub fn list_ingress(&self) -> Vec<IngressRecord> {
        self.ingress_records.values().cloned().collect()
    }

    pub fn delete_ingress(&mut self, name: &str) -> Result<(), ReconcileError> {
        if !self.ingress_records.contains_key(name) {
            return Err(ReconcileError::NotFound(name.to_string()));
        }
        ingress_store::remove(&self.state_dir, name)?;
        self.ingress_records.remove(name);
        self.ingress_backoff.remove(name);
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd apply_ingress get_ingress delete_ingress re_applying`
Expected: PASS, 6 tests.

- [ ] **Step 5: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/src/reconciler.rs
git commit -m "feat(keel-agentd): add Ingress CRUD to the Reconciler"
```

---

### Task 9: `keel-agentd`: `Command::ApplyIngress`/`GetIngress`/`DeleteIngress` and HTTP routes

**Files:**
- Modify: `keel-agentd/src/worker.rs`
- Modify: `keel-agentd/src/wire.rs`
- Modify: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `Reconciler::apply_ingress`/`get_ingress`/`list_ingress`/`delete_ingress` (Task 8); `ServiceVipSlot::names()` (Task 5/6).
- Produces: `Command::ApplyIngress(IngressSpec, Sender<Result<(), ReconcileError>>)`, `Command::GetIngress(Option<String>, Sender<Vec<IngressStatus>>)`, `Command::DeleteIngress(String, Sender<Result<(), ReconcileError>>)`; `wire::IngressStatus { record: IngressRecord }`; HTTP routes `PUT /ingress/{name}`, `GET /ingress` and `GET /ingress/{name}`, `DELETE /ingress/{name}` on both `http::run` (Unix socket) and `http::run_tls`.

- [ ] **Step 1: Write the failing test in `keel-agentd/src/http.rs`**

Add near `handle_apply`'s own tests (search for the existing `mod tests` in `http.rs`, likely testing `handle_apply`/`handle_get`/`handle_delete` against a running `run`/`run_tls` server on a Unix socket):

```rust
#[test]
fn put_ingress_then_get_it_back() {
    let (socket, service_vips) = start_test_server_with_service_vips("put_ingress_then_get_it_back", &[("hugo-site", "10.0.0.9", 8080)]);
    let body = "apiVersion: keel/v1\nkind: Ingress\nmetadata:\n  name: blog\nspec:\n  host: example.com\n  backend:\n    service: hugo-site\n    port: 8080\n  tls:\n    email: admin@example.com\n";
    let (status, _) = send_request(&socket, "PUT", "/ingress/blog", body).unwrap();
    assert_eq!(status, 200);
    let (status, response_body) = send_request(&socket, "GET", "/ingress/blog", "").unwrap();
    assert_eq!(status, 200);
    assert!(response_body.contains("host: example.com"));
    let _ = service_vips;
}

#[test]
fn put_ingress_rejects_an_unknown_backend_service() {
    let (socket, _service_vips) = start_test_server_with_service_vips("put_ingress_rejects_an_unknown_backend_service", &[]);
    let body = "apiVersion: keel/v1\nkind: Ingress\nmetadata:\n  name: blog\nspec:\n  host: example.com\n  backend:\n    service: does-not-exist\n    port: 8080\n  tls:\n    email: admin@example.com\n";
    let (status, response_body) = send_request(&socket, "PUT", "/ingress/blog", body).unwrap();
    assert_eq!(status, 400);
    assert!(response_body.contains("does-not-exist"));
}

#[test]
fn delete_ingress_then_get_is_404() {
    let (socket, _service_vips) = start_test_server_with_service_vips("delete_ingress_then_get_is_404", &[("hugo-site", "10.0.0.9", 8080)]);
    let body = "apiVersion: keel/v1\nkind: Ingress\nmetadata:\n  name: blog\nspec:\n  host: example.com\n  backend:\n    service: hugo-site\n    port: 8080\n  tls:\n    email: admin@example.com\n";
    let _ = send_request(&socket, "PUT", "/ingress/blog", body).unwrap();
    let (status, _) = send_request(&socket, "DELETE", "/ingress/blog", "").unwrap();
    assert_eq!(status, 200);
    let (status, _) = send_request(&socket, "GET", "/ingress/blog", "").unwrap();
    assert_eq!(status, 404);
}
```

Add the test helper (mirroring whatever existing helper starts a Unix-socket test server; adjust to match `http.rs`'s exact existing helper name/shape found by reading the file, e.g. alongside `start_test_server_with_pod_cidr`):

```rust
fn start_test_server_with_service_vips(name: &str, entries: &[(&str, &str, u16)]) -> (PathBuf, crate::ServiceVipSlot) {
    let dir = test_state_dir(name);
    let reconciler = crate::Reconciler::new(
        FakeJailRuntime::new(), FakeZfsManager::new(), FakeNetManager::new(), FakeMountManager::new(),
        "zroot".to_string(), dir,
    ).unwrap();
    let (_handle, commands) = crate::worker::spawn(reconciler, FakeZfsManager::new(), "zroot".to_string());
    let socket = std::env::temp_dir().join(format!("keel-agentd-http-test-{name}.sock"));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    let service_vips = crate::ServiceVipSlot::new();
    let proxy_entries: Vec<_> = entries.iter().map(|(n, vip, port)| keel_controlplane::wire::ServiceProxyEntry {
        name: n.to_string(), vip: vip.to_string(), port: *port, replicas: vec![],
    }).collect();
    service_vips.set_all(&proxy_entries);
    let replica_targets = crate::ReplicaTargetRegistry::load(std::env::temp_dir().join(format!("keel-agentd-http-test-replica-{name}"))).unwrap();
    let thread_service_vips = service_vips.clone();
    thread::spawn(move || run(listener, commands, PodCidrSlot::new(), thread_service_vips, replica_targets));
    thread::sleep(Duration::from_millis(50));
    (socket, service_vips)
}
```

(This helper's exact shape must match what already exists in `http.rs` for the other `start_test_server_*` helpers - read the file first and adapt field names/imports accordingly; `run`'s signature is changing in this same task's Step 3 to take `service_vips`, so this helper compiles only once that change lands.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd put_ingress delete_ingress_then_get`
Expected: FAIL to compile - no `/ingress` route, `run` doesn't take `service_vips` yet.

- [ ] **Step 3: Add `Command` variants and `IngressStatus` wire type**

In `keel-agentd/src/worker.rs`, add to the `Command` enum:

```rust
    ApplyIngress(keel_spec::IngressSpec, Sender<Result<(), ReconcileError>>),
    GetIngress(Option<String>, Sender<Vec<crate::wire::IngressStatus>>),
    DeleteIngress(String, Sender<Result<(), ReconcileError>>),
```

Add to `handle_command`'s `match`:

```rust
        Command::ApplyIngress(spec, reply) => {
            let result = reconciler.apply_ingress(spec);
            let _ = reconciler.reconcile(Instant::now());
            let _ = reply.send(result);
        }
        Command::GetIngress(name, reply) => {
            let statuses = match name {
                Some(n) => reconciler.get_ingress(&n).map(|record| crate::wire::IngressStatus { record }).into_iter().collect(),
                None => reconciler.list_ingress().into_iter().map(|record| crate::wire::IngressStatus { record }).collect(),
            };
            let _ = reply.send(statuses);
        }
        Command::DeleteIngress(name, reply) => {
            let result = reconciler.delete_ingress(&name);
            let _ = reconciler.reconcile(Instant::now());
            let _ = reply.send(result);
        }
```

In `keel-agentd/src/wire.rs`, add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressStatus {
    pub record: crate::ingress_record::IngressRecord,
}
```

(Match the existing derives/imports already present in `wire.rs` for `JailStatus` - reuse the same `serde::{Serialize, Deserialize}` import.)

- [ ] **Step 4: Add HTTP routes in `keel-agentd/src/http.rs`**

Thread `service_vips: ServiceVipSlot` through `run`/`run_tls`/`handle_connection`/`handle_connection_tls`/`route`, exactly the way `pod_cidr_slot` already is (add the parameter in the same positions, clone it into each spawned thread the same way). Add to `route`'s `match`:

```rust
        ("PUT", ["ingress", name]) => handle_apply_ingress(name, &request.body, commands, service_vips),
        ("GET", ["ingress"]) => handle_get_ingress(None, commands),
        ("GET", ["ingress", name]) => handle_get_ingress(Some(name.to_string()), commands),
        ("DELETE", ["ingress", name]) => handle_delete_ingress(name, commands),
```

Add the handlers (near `handle_apply`):

```rust
fn handle_apply_ingress(
    path_name: &str,
    body: &[u8],
    commands: &Sender<Command>,
    service_vips: &crate::ServiceVipSlot,
) -> (u16, Vec<u8>) {
    let spec: keel_spec::IngressSpec = match serde_yaml::from_slice(body) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    if spec.metadata.name != path_name {
        return error_response(
            400,
            format!("path name '{path_name}' does not match spec.metadata.name '{}'", spec.metadata.name),
        );
    }
    if !service_vips.names().contains(&spec.spec.backend.service) {
        return error_response(
            400,
            format!("backend.service '{}' does not name a currently-known Service", spec.spec.backend.service),
        );
    }

    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ApplyIngress(spec, reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}

fn handle_get_ingress(name: Option<String>, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::GetIngress(name.clone(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    let statuses = match reply_rx.recv() {
        Ok(s) => s,
        Err(_) => return error_response(500, "reconciler worker did not respond".to_string()),
    };
    match name {
        Some(n) => match statuses.into_iter().next() {
            Some(status) => yaml_response(200, &status.record.spec),
            None => error_response(404, format!("ingress '{n}' not found")),
        },
        None => yaml_response(200, &statuses.into_iter().map(|s| s.record.spec).collect::<Vec<_>>()),
    }
}

fn handle_delete_ingress(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::DeleteIngress(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}
```

(`handle_get_ingress` returns the bare `spec`, not the wrapping `IngressStatus`/`record`, matching `handle_get`'s existing choice to return `JailStatus` directly rather than an extra wrapper - read `handle_get`'s exact response shape first and match its convention precisely; adjust if `handle_get` actually does return the full status struct.)

- [ ] **Step 5: Update every `run`/`run_tls` call site (production and tests) to pass `service_vips`**

`main.rs`'s `thread::spawn(move || keel_agentd::http::run(listener, commands, pod_cidr_slot, replica_targets));` becomes `thread::spawn(move || keel_agentd::http::run(listener, commands, pod_cidr_slot, service_vips.clone(), replica_targets));` and similarly for the `run_tls` call - `service_vips` was already created in Task 6's Step 4; this task is what actually uses it beyond the registration loop.

Fix every other `run(...)`/`run_tls(...)` call the compiler flags (existing tests in `http.rs` and `main.rs`) by inserting a `ServiceVipSlot::new()` (or a populated one, for the new tests) in the same argument position.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p keel-agentd put_ingress delete_ingress_then_get`
Expected: PASS.

- [ ] **Step 7: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS.

- [ ] **Step 8: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add keel-agentd/src/worker.rs keel-agentd/src/wire.rs keel-agentd/src/http.rs keel-agentd/src/main.rs
git commit -m "feat(keel-agentd): add PUT/GET/DELETE /ingress HTTP routes"
```

---

### Task 10: `keel-agentd`: synthesize the singleton `keel-ingress` jail

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: `Reconciler::apply` (existing, Milestone 4); `self.ingress_records` (Task 8).
- Produces: `Reconciler::ensure_ingress_jail(&mut self) -> Result<(), ReconcileError>`, called from `reconcile()`.

The synthesized `JailSpec`'s `metadata.name` must be `"ingress"`, **not** `"keel-ingress"` - `record::jail_name` already prepends `"keel-"` to whatever name it's given (`jail_name("ingress") == "keel-ingress"`), so naming the spec itself `"keel-ingress"` would double-prefix to `keel-keel-ingress`. This was checked directly against `record::jail_name`'s current implementation before writing this task.

The base image (`base/keel-ingress`, an nginx install) is out of scope to build per this milestone's Non-Goals - exactly like every other base image in this project, it's a manual prerequisite. `Reconciler::provision`'s existing `BaseImageNotFound` error surfaces cleanly if it's missing, matching how a missing user `Jail` base image already behaves.

- [ ] **Step 1: Write the failing test in `keel-agentd/src/reconciler.rs`**

```rust
#[test]
fn reconcile_provisions_the_singleton_ingress_jail_once_an_ingress_spec_exists() {
    let dir = test_state_dir("reconcile_provisions_the_singleton_ingress_jail");
    let mut reconciler = test_reconciler(&dir);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.reconcile(Instant::now());
    assert!(reconciler.jails.jail_exists("keel-ingress").unwrap());
}

#[test]
fn reconcile_does_not_provision_the_ingress_jail_when_no_ingress_spec_exists() {
    let dir = test_state_dir("reconcile_does_not_provision_the_ingress_jail_when_none_exist");
    let mut reconciler = test_reconciler(&dir);
    reconciler.reconcile(Instant::now());
    assert!(!reconciler.jails.jail_exists("keel-ingress").unwrap());
}

#[test]
fn reconcile_provisions_the_ingress_jail_only_once_even_across_multiple_ticks() {
    let dir = test_state_dir("reconcile_provisions_the_ingress_jail_only_once");
    let mut reconciler = test_reconciler(&dir);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.reconcile(Instant::now());
    reconciler.reconcile(Instant::now());
    assert_eq!(reconciler.records.get("ingress").unwrap().epair_ordinal, reconciler.records.get("ingress").unwrap().epair_ordinal);
}

#[test]
fn deleting_the_last_ingress_spec_does_not_retroactively_destroy_the_ingress_jail() {
    let dir = test_state_dir("deleting_the_last_ingress_spec_does_not_destroy_the_jail");
    let mut reconciler = test_reconciler(&dir);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.reconcile(Instant::now());
    reconciler.delete_ingress("blog").unwrap();
    reconciler.reconcile(Instant::now());
    assert!(reconciler.jails.jail_exists("keel-ingress").unwrap());
}
```

(The last test documents a deliberate simplification: this milestone never tears the ingress jail back down once provisioned, matching how this project generally favors "never destroy a shared resource speculatively" - see `NetManager::ensure_bridge_exists`'s own doc comment making the identical call for bridges. If a reviewer wants auto-teardown-when-empty instead, that's a explicit follow-up, not silently assumed here.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd reconcile_provisions_the_singleton_ingress_jail reconcile_does_not_provision deleting_the_last_ingress_spec`
Expected: FAIL - `keel-ingress` jail is never provisioned yet (`ensure_ingress_jail` doesn't exist).

- [ ] **Step 3: Implement `ensure_ingress_jail` and call it from `reconcile`**

```rust
    fn ensure_ingress_jail(&mut self) -> Result<(), ReconcileError> {
        if self.ingress_records.is_empty() || self.records.contains_key("ingress") {
            return Ok(());
        }
        let spec = keel_spec::JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: keel_spec::Metadata { name: "ingress".to_string() },
            spec: keel_spec::Spec {
                image: "base/keel-ingress".to_string(),
                command: vec!["/usr/local/sbin/nginx".to_string(), "-g".to_string(), "daemon off;".to_string()],
                network: keel_spec::NetworkSpec {
                    vnet: true,
                    bridge: "keel0".to_string(),
                    address: "10.0.0.2/24".to_string(),
                },
                resources: keel_spec::ResourcesSpec { cpu: "1".to_string(), memory: "256M".to_string() },
                restart_policy: keel_spec::RestartPolicy::Always,
                volumes: vec![],
                replicate_to: None,
            },
        };
        self.apply(spec)
    }

    pub fn reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)> {
        if let Err(e) = self.ensure_ingress_jail() {
            eprintln!("keel-agentd: failed to ensure the ingress jail exists: {e}");
        }
        let names: Vec<String> = self.records.keys().cloned().collect();
        let mut failures = Vec::new();
        for name in names {
            if let Err(e) = self.reconcile_one(&name, now) {
                failures.push((name, e));
            }
        }
        failures
    }
```

Note: `10.0.0.2/24` is a placeholder internal address, deliberately picked outside the range Milestone 14's per-node pod-CIDR allocation and Milestone 16's service-CIDR allocation use (both documented in their own specs) - confirm against those two specs' exact reserved ranges before finalizing this literal, and adjust if it collides; this is implementation-time discovery flagged explicitly rather than assumed correct on the first guess, consistent with how Milestone 20 flagged its own unverified specifics.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd reconcile_provisions_the_singleton_ingress_jail reconcile_does_not_provision deleting_the_last_ingress_spec`
Expected: PASS.

- [ ] **Step 5: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/src/reconciler.rs
git commit -m "feat(keel-agentd): provision the singleton keel-ingress jail once an Ingress exists"
```

---

### Task 11: `keel-agentd`: certificate issuance and renewal reconcile pass

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`
- Modify: `keel-agentd/Cargo.toml` (add `keel-ingress` path dependency)

**Interfaces:**
- Consumes: `keel_ingress::{AcmeClient, DnsProvider, Cert}` (Task 2/3); `Reconciler::ingress_records`/`ingress_backoff` (Task 8).
- Produces: `Reconciler` gains constructor parameters `acme: Box<dyn AcmeClient + Send>, dns: Box<dyn DnsProvider + Send>` (or equivalent trait-object fields - see Step 3) and a `reconcile_certs(&mut self, now: Instant)` method called from `reconcile()`; certs are written to `<rootfs>/usr/local/etc/nginx/certs/<host>.{crt,key}` where `rootfs` is `record::jail_rootfs_path(&self.pool, "ingress")`.

30-day threshold: `cert_expires_at_unix.is_none() || cert_expires_at_unix.unwrap() - now_unix < 30 * 24 * 60 * 60`. Since `Reconciler` otherwise only ever uses `std::time::Instant` (monotonic, no wall-clock meaning) and cert expiry is inherently wall-clock, this task's tests inject a wall-clock "now" as a plain `i64` unix timestamp parameter rather than reusing `Instant`, exactly the way `BackoffState::status`'s own doc comment already explains why `Instant` "has no wall-clock meaning to report" - the same reasoning applies here in reverse (a cert's expiry is meaningless as an `Instant`).

- [ ] **Step 1: Add the `keel-ingress` dependency**

In `keel-agentd/Cargo.toml`, add under `[dependencies]`:

```toml
keel-ingress = { path = "../keel-ingress" }
```

- [ ] **Step 2: Write the failing tests in `keel-agentd/src/reconciler.rs`**

```rust
use keel_ingress::{AcmeClient, DnsProvider, FakeAcmeClient, FakeDnsProvider};

fn test_reconciler_with_acme(dir: &std::path::Path, acme: FakeAcmeClient, dns: FakeDnsProvider) -> Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager, FakeMountManager> {
    let mut reconciler = test_reconciler(dir);
    reconciler.acme = Box::new(acme);
    reconciler.dns = Box::new(dns);
    reconciler
}

#[test]
fn reconcile_certs_issues_a_certificate_for_a_new_ingress_with_no_expiry_yet() {
    let dir = test_state_dir("reconcile_certs_issues_a_certificate_for_a_new_ingress");
    let mut reconciler = test_reconciler_with_acme(&dir, FakeAcmeClient::new(), FakeDnsProvider::new());
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.reconcile_certs(1_800_000_000);
    assert!(reconciler.get_ingress("blog").unwrap().cert_expires_at_unix.is_some());
}

#[test]
fn reconcile_certs_does_not_reissue_a_certificate_with_more_than_30_days_left() {
    let dir = test_state_dir("reconcile_certs_does_not_reissue_a_fresh_certificate");
    let acme = FakeAcmeClient::new();
    let mut reconciler = test_reconciler_with_acme(&dir, FakeAcmeClient::new(), FakeDnsProvider::new());
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    let now = 1_800_000_000;
    reconciler.reconcile_certs(now);
    let first_expiry = reconciler.get_ingress("blog").unwrap().cert_expires_at_unix;

    // Swap in a FakeAcmeClient that would panic-detectably if called again:
    // instead, just confirm the expiry is unchanged after a second pass with
    // fewer than 30 days having elapsed.
    reconciler.reconcile_certs(now + 60);
    assert_eq!(reconciler.get_ingress("blog").unwrap().cert_expires_at_unix, first_expiry);
    let _ = acme;
}

#[test]
fn reconcile_certs_reissues_within_30_days_of_expiry() {
    let dir = test_state_dir("reconcile_certs_reissues_within_30_days_of_expiry");
    let mut reconciler = test_reconciler_with_acme(&dir, FakeAcmeClient::new(), FakeDnsProvider::new());
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    let now = 1_800_000_000;
    reconciler.reconcile_certs(now);
    let first_expiry = reconciler.get_ingress("blog").unwrap().cert_expires_at_unix.unwrap();

    // Jump to 29 days before that expiry -- inside the 30-day threshold.
    let near_expiry = first_expiry - 29 * 24 * 60 * 60;
    reconciler.reconcile_certs(near_expiry);
    let second_expiry = reconciler.get_ingress("blog").unwrap().cert_expires_at_unix.unwrap();
    assert!(second_expiry > first_expiry, "renewal should push the expiry further into the future");
}

#[test]
fn reconcile_certs_backs_off_on_failure_without_blocking_other_ingresses() {
    let dir = test_state_dir("reconcile_certs_backs_off_on_failure_without_blocking_others");
    let failing_acme = FakeAcmeClient::new();
    failing_acme.set_fail(true);
    let mut reconciler = test_reconciler_with_acme(&dir, failing_acme, FakeDnsProvider::new());
    reconciler.apply_ingress(sample_ingress_spec("broken", "broken.example.com")).unwrap();
    reconciler.reconcile_certs(1_800_000_000);
    assert_eq!(reconciler.get_ingress("broken").unwrap().cert_expires_at_unix, None);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-agentd reconcile_certs`
Expected: FAIL to compile - `Reconciler` has no `acme`/`dns` fields or `reconcile_certs` method yet.

- [ ] **Step 4: Add `acme`/`dns` fields and `reconcile_certs`**

Add fields to the `Reconciler` struct (right after `ingress_backoff`):

```rust
    acme: Box<dyn keel_ingress::AcmeClient + Send>,
    dns: Box<dyn keel_ingress::DnsProvider + Send>,
```

Update `Reconciler::new`'s signature to take them:

```rust
    pub fn new(
        jails: J,
        zfs: Z,
        net: N,
        mounts: M,
        pool: String,
        state_dir: PathBuf,
        acme: Box<dyn keel_ingress::AcmeClient + Send>,
        dns: Box<dyn keel_ingress::DnsProvider + Send>,
    ) -> Result<Self, ReconcileError> {
```

(add `acme, dns,` to the `Ok(Self { ... })` construction), and update every existing call site (`main.rs`, and every `test_reconciler`/`test_reconciler_with_acme` helper across `reconciler.rs`, `worker.rs`, `proxy.rs`, `http.rs`) to pass `Box::new(keel_ingress::FakeAcmeClient::new())` / `Box::new(keel_ingress::FakeDnsProvider::new())` in tests, and the real implementations (Task 15/16) in `main.rs` - Task 11 itself still uses `FakeAcmeClient`/`FakeDnsProvider` in `main.rs` too (the real ones don't exist until Task 15/16), so add a `TODO`-free but temporary direct construction: `Box::new(keel_ingress::FakeAcmeClient::new())` in `main.rs` for now, replaced by the real ones in Task 17.

Add `reconcile_certs` and wire it into `reconcile`:

```rust
    fn reconcile_certs(&mut self, now_unix: i64) {
        const RENEWAL_THRESHOLD_SECS: i64 = 30 * 24 * 60 * 60;
        let names: Vec<String> = self.ingress_records.keys().cloned().collect();
        for name in names {
            let can_retry = self.ingress_backoff.entry(name.clone()).or_insert_with(BackoffState::new).can_retry(Instant::now());
            if !can_retry {
                continue;
            }
            let record = self.ingress_records[&name].clone();
            let needs_issuance = match record.cert_expires_at_unix {
                None => true,
                Some(expires_at) => expires_at - now_unix < RENEWAL_THRESHOLD_SECS,
            };
            if !needs_issuance {
                continue;
            }
            self.ingress_backoff.get_mut(&name).unwrap().record_attempt(Instant::now());
            match self.acme.request_certificate(&record.spec.spec.host, &record.spec.spec.tls.email, self.dns.as_ref()) {
                Ok(cert) => {
                    if let Err(e) = self.write_cert_to_ingress_jail(&record.spec.spec.host, &cert) {
                        eprintln!("keel-agentd: failed to write certificate for ingress '{name}' into the ingress jail: {e}");
                        continue;
                    }
                    let mut updated = record;
                    updated.cert_expires_at_unix = Some(now_unix + 90 * 24 * 60 * 60);
                    if let Err(e) = ingress_store::save(&self.state_dir, &updated) {
                        eprintln!("keel-agentd: failed to persist certificate expiry for ingress '{name}': {e}");
                        continue;
                    }
                    self.ingress_records.insert(name, updated);
                }
                Err(e) => eprintln!("keel-agentd: certificate issuance failed for ingress '{name}': {e}"),
            }
        }
    }

    fn write_cert_to_ingress_jail(&self, host: &str, cert: &keel_ingress::Cert) -> Result<(), ReconcileError> {
        let certs_dir = record::jail_rootfs_path(&self.pool, "ingress").join("usr/local/etc/nginx/certs");
        std::fs::create_dir_all(&certs_dir).map_err(|e| ReconcileError::Store(StoreError::Io(certs_dir.clone(), e)))?;
        let crt_path = certs_dir.join(format!("{host}.crt"));
        let key_path = certs_dir.join(format!("{host}.key"));
        std::fs::write(&crt_path, &cert.cert_pem).map_err(|e| ReconcileError::Store(StoreError::Io(crt_path, e)))?;
        std::fs::write(&key_path, &cert.key_pem).map_err(|e| ReconcileError::Store(StoreError::Io(key_path, e)))?;
        Ok(())
    }
```

(`90 * 24 * 60 * 60` is a stand-in for "Let's Encrypt's real 90-day validity period" since `FakeAcmeClient` doesn't return an actual expiry - the real `AcmeClient` implementation in Task 16 must parse the actual `notAfter` field out of the issued certificate rather than hardcoding 90 days; flagged here so Task 16 doesn't silently keep this placeholder.)

`reconcile_certs` takes a wall-clock `now_unix: i64` parameter rather than being folded silently into `reconcile(&mut self, now: Instant)` - call it explicitly from `reconcile`:

```rust
    pub fn reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)> {
        if let Err(e) = self.ensure_ingress_jail() {
            eprintln!("keel-agentd: failed to ensure the ingress jail exists: {e}");
        }
        let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.reconcile_certs(now_unix);
        let names: Vec<String> = self.records.keys().cloned().collect();
        let mut failures = Vec::new();
        for name in names {
            if let Err(e) = self.reconcile_one(&name, now) {
                failures.push((name, e));
            }
        }
        failures
    }
```

(Tests call `reconcile_certs` directly with an injected `now_unix`, bypassing `reconcile`'s own `SystemTime::now()` call, exactly so the 30-day-threshold tests above don't depend on the real wall clock.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-agentd reconcile_certs`
Expected: PASS.

- [ ] **Step 6: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS (fix every `Reconciler::new(...)` call site the compiler flags across the crate, in both `src/` and `tests/`, adding the two new `Box::new(Fake...::new())` arguments).

- [ ] **Step 7: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add keel-agentd/src/reconciler.rs keel-agentd/Cargo.toml
git commit -m "feat(keel-agentd): issue and renew Ingress certificates via AcmeClient"
```

---

### Task 12: `keel-agentd`: nginx config regeneration, validation, reload

**Files:**
- Create: `keel-agentd/src/nginx.rs`
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: `keel_ingress::{render_nginx_config, IngressBackendConfig}` (Task 4); `ServiceVipSlot` (Task 5, threaded into `Reconciler` this task).
- Produces: `keel_agentd::nginx::NginxController` trait (`write_config`, `test_config`, `reload`) with `FakeNginxController`/`JexecNginxController`; `Reconciler::reconcile_ingress_config(&mut self)`, called from `reconcile()`.

- [ ] **Step 1: Write the failing test in `keel-agentd/src/nginx.rs`**

```rust
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NginxError {
    #[error("failed to write nginx config: {0}")]
    Write(String),
    #[error("nginx -t validation failed: {0}")]
    ValidationFailed(String),
    #[error("nginx -s reload failed: {0}")]
    ReloadFailed(String),
}

pub trait NginxController {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError>;
    fn test_config(&self, jail_name: &str) -> Result<(), NginxError>;
    fn reload(&self, jail_name: &str) -> Result<(), NginxError>;
}

#[derive(Default)]
pub struct FakeNginxController {
    written: Mutex<std::collections::HashMap<String, String>>,
    fail_test: Mutex<bool>,
    reload_count: Mutex<std::collections::HashMap<String, u32>>,
}

impl FakeNginxController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail_test(&self, fail: bool) {
        *self.fail_test.lock().unwrap() = fail;
    }

    pub fn last_written_config(&self, jail_name: &str) -> Option<String> {
        self.written.lock().unwrap().get(jail_name).cloned()
    }

    pub fn reload_count(&self, jail_name: &str) -> u32 {
        *self.reload_count.lock().unwrap().get(jail_name).unwrap_or(&0)
    }
}

impl NginxController for FakeNginxController {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError> {
        self.written.lock().unwrap().insert(jail_name.to_string(), config.to_string());
        Ok(())
    }

    fn test_config(&self, _jail_name: &str) -> Result<(), NginxError> {
        if *self.fail_test.lock().unwrap() {
            return Err(NginxError::ValidationFailed("simulated nginx -t failure".to_string()));
        }
        Ok(())
    }

    fn reload(&self, jail_name: &str) -> Result<(), NginxError> {
        *self.reload_count.lock().unwrap().entry(jail_name.to_string()).or_insert(0) += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_config_then_last_written_config_returns_it() {
        let nginx = FakeNginxController::new();
        nginx.write_config("keel-ingress", "config-v1").unwrap();
        assert_eq!(nginx.last_written_config("keel-ingress"), Some("config-v1".to_string()));
    }

    #[test]
    fn test_config_fails_when_set_to_fail() {
        let nginx = FakeNginxController::new();
        nginx.set_fail_test(true);
        assert!(nginx.test_config("keel-ingress").is_err());
    }

    #[test]
    fn reload_count_increments_on_each_call() {
        let nginx = FakeNginxController::new();
        nginx.reload("keel-ingress").unwrap();
        nginx.reload("keel-ingress").unwrap();
        assert_eq!(nginx.reload_count("keel-ingress"), 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd nginx::tests`
Expected: FAIL - module doesn't exist yet.

- [ ] **Step 3: Register the module**

Add `pub mod nginx;` to `keel-agentd/src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd nginx::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Write the failing test for `reconcile_ingress_config` in `keel-agentd/src/reconciler.rs`**

```rust
fn test_reconciler_with_acme_and_nginx(
    dir: &std::path::Path,
    acme: FakeAcmeClient,
    dns: FakeDnsProvider,
) -> (Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager, FakeMountManager>, std::sync::Arc<crate::nginx::FakeNginxController>) {
    let mut reconciler = test_reconciler_with_acme(dir, acme, dns);
    let nginx = std::sync::Arc::new(crate::nginx::FakeNginxController::new());
    reconciler.nginx = Box::new(std::sync::Arc::clone(&nginx));
    reconciler.service_vips = crate::ServiceVipSlot::new();
    (reconciler, nginx)
}

#[test]
fn reconcile_ingress_config_writes_and_reloads_nginx_once_a_backend_vip_is_known() {
    let dir = test_state_dir("reconcile_ingress_config_writes_and_reloads_nginx");
    let (mut reconciler, nginx) = test_reconciler_with_acme_and_nginx(&dir, FakeAcmeClient::new(), FakeDnsProvider::new());
    reconciler.service_vips.set_all(&[keel_controlplane::wire::ServiceProxyEntry {
        name: "hugo-site".to_string(), vip: "10.0.0.9".to_string(), port: 8080, replicas: vec![],
    }]);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.reconcile_ingress_config();
    let config = nginx.last_written_config("keel-ingress").unwrap();
    assert!(config.contains("server_name example.com;"));
    assert!(config.contains("proxy_pass http://10.0.0.9:8080;"));
    assert_eq!(nginx.reload_count("keel-ingress"), 1);
}

#[test]
fn reconcile_ingress_config_skips_a_backend_whose_vip_is_not_yet_known() {
    let dir = test_state_dir("reconcile_ingress_config_skips_a_backend_whose_vip_is_not_known");
    let (mut reconciler, nginx) = test_reconciler_with_acme_and_nginx(&dir, FakeAcmeClient::new(), FakeDnsProvider::new());
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    reconciler.reconcile_ingress_config();
    let config = nginx.last_written_config("keel-ingress").unwrap();
    assert!(!config.contains("example.com"));
}

#[test]
fn reconcile_ingress_config_does_not_reload_when_validation_fails() {
    let dir = test_state_dir("reconcile_ingress_config_does_not_reload_when_validation_fails");
    let (mut reconciler, nginx) = test_reconciler_with_acme_and_nginx(&dir, FakeAcmeClient::new(), FakeDnsProvider::new());
    reconciler.service_vips.set_all(&[keel_controlplane::wire::ServiceProxyEntry {
        name: "hugo-site".to_string(), vip: "10.0.0.9".to_string(), port: 8080, replicas: vec![],
    }]);
    reconciler.apply_ingress(sample_ingress_spec("blog", "example.com")).unwrap();
    nginx.set_fail_test(true);
    reconciler.reconcile_ingress_config();
    assert_eq!(nginx.reload_count("keel-ingress"), 0);
}
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `cargo test -p keel-agentd reconcile_ingress_config`
Expected: FAIL to compile - `Reconciler` has no `nginx`/`service_vips` fields or `reconcile_ingress_config` method yet.

- [ ] **Step 7: Add `nginx`/`service_vips` fields and `reconcile_ingress_config`**

Add fields to `Reconciler`:

```rust
    nginx: Box<dyn crate::nginx::NginxController + Send>,
    service_vips: crate::ServiceVipSlot,
```

Add both as new `Reconciler::new` parameters (after `dns`), threading them through every call site exactly like Task 11 did for `acme`/`dns`.

Add `reconcile_ingress_config` and call it from `reconcile` (right after `reconcile_certs`):

```rust
    fn reconcile_ingress_config(&mut self) {
        let backends: Vec<keel_ingress::IngressBackendConfig> = self
            .ingress_records
            .values()
            .filter_map(|record| {
                let (vip, port) = self.service_vips.get(&record.spec.spec.backend.service)?;
                Some(keel_ingress::IngressBackendConfig {
                    host: record.spec.spec.host.clone(),
                    vip,
                    port,
                    cert_path: format!("/usr/local/etc/nginx/certs/{}.crt", record.spec.spec.host),
                    key_path: format!("/usr/local/etc/nginx/certs/{}.key", record.spec.spec.host),
                })
            })
            .collect();
        let config = keel_ingress::render_nginx_config(&backends);
        if let Err(e) = self.nginx.write_config("keel-ingress", &config) {
            eprintln!("keel-agentd: failed to write ingress nginx config: {e}");
            return;
        }
        if let Err(e) = self.nginx.test_config("keel-ingress") {
            eprintln!("keel-agentd: ingress nginx config failed validation, leaving the previous config live: {e}");
            return;
        }
        if let Err(e) = self.nginx.reload("keel-ingress") {
            eprintln!("keel-agentd: failed to reload ingress nginx: {e}");
        }
    }
```

Update `reconcile`:

```rust
    pub fn reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)> {
        if let Err(e) = self.ensure_ingress_jail() {
            eprintln!("keel-agentd: failed to ensure the ingress jail exists: {e}");
        }
        let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.reconcile_certs(now_unix);
        self.reconcile_ingress_config();
        let names: Vec<String> = self.records.keys().cloned().collect();
        let mut failures = Vec::new();
        for name in names {
            if let Err(e) = self.reconcile_one(&name, now) {
                failures.push((name, e));
            }
        }
        failures
    }
```

`FakeNginxController` needs `NginxController` implemented for `Arc<FakeNginxController>` too (used in the test helper above so the test keeps its own handle for assertions after moving a clone into the `Reconciler`) - add to `keel-agentd/src/nginx.rs`:

```rust
impl NginxController for std::sync::Arc<FakeNginxController> {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError> {
        (**self).write_config(jail_name, config)
    }
    fn test_config(&self, jail_name: &str) -> Result<(), NginxError> {
        (**self).test_config(jail_name)
    }
    fn reload(&self, jail_name: &str) -> Result<(), NginxError> {
        (**self).reload(jail_name)
    }
}
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p keel-agentd reconcile_ingress_config`
Expected: PASS.

- [ ] **Step 9: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS (fix every remaining `Reconciler::new(...)` call site to pass a `Box::new(crate::nginx::FakeNginxController::new())` and `crate::ServiceVipSlot::new()`).

- [ ] **Step 10: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 11: Commit**

```bash
git add keel-agentd/src/nginx.rs keel-agentd/src/reconciler.rs keel-agentd/src/lib.rs
git commit -m "feat(keel-agentd): regenerate, validate, and reload ingress nginx config"
```

---

### Task 13: `keel-agentd`: real `JexecNginxController`

**Files:**
- Modify: `keel-agentd/src/nginx.rs`

**Interfaces:**
- Produces: `keel_agentd::nginx::JexecNginxController` (real `NginxController`, shells to `jexec` and writes directly to the jail's rootfs path, matching `keel-net::process::ProcessNetManager`'s existing `jexec`-shelling style).
- Consumes: `record::jail_rootfs_path` (existing).

- [ ] **Step 1: Write the failing FreeBSD-gated test**

Create `keel-agentd/tests/freebsd_nginx.rs` (gated, run manually as root on the dev VM - not part of `cargo test --workspace`'s default run since it needs a real `keel-ingress` jail already running nginx; document the prerequisite in a comment at the top of the file, mirroring `keel-jail/tests/freebsd_lifecycle.rs`'s own prerequisite comment):

```rust
#![cfg(target_os = "freebsd")]

// Run as root on the dev VM, with a `keel-ingress` jail already running
// nginx (see this milestone's Task 19 for how that jail comes to exist).
// Usage: cargo test -p keel-agentd --test freebsd_nginx -- --ignored --test-threads=1

use keel_agentd::nginx::{JexecNginxController, NginxController};

#[test]
#[ignore]
fn write_test_and_reload_round_trip_against_a_real_running_nginx_jail() {
    let controller = JexecNginxController::new("zroot".to_string());
    let config = "user www; worker_processes 1;\nevents { worker_connections 1024; }\nhttp {\n    server { listen 80; return 200 'ok'; }\n}\n";
    controller.write_config("keel-ingress", config).unwrap();
    controller.test_config("keel-ingress").unwrap();
    controller.reload("keel-ingress").unwrap();
}

#[test]
#[ignore]
fn test_config_fails_on_a_deliberately_malformed_config() {
    let controller = JexecNginxController::new("zroot".to_string());
    controller.write_config("keel-ingress", "this is not valid nginx config {{{").unwrap();
    assert!(controller.test_config("keel-ingress").is_err());
}
```

- [ ] **Step 2: Run to verify it fails to compile (not run - FreeBSD-only)**

Run: `cargo build -p keel-agentd --tests` (on any OS; `#![cfg(target_os = "freebsd")]` means it's a no-op on macOS, but it must still compile once `JexecNginxController` exists - before that, `cargo check -p keel-agentd` on a FreeBSD target or a quick `#[cfg(target_os = "freebsd")]`-stripped local copy confirms the symbol is missing).
Expected: FAIL - `JexecNginxController` doesn't exist yet.

- [ ] **Step 3: Implement `JexecNginxController`**

Append to `keel-agentd/src/nginx.rs`:

```rust
pub struct JexecNginxController {
    pool: String,
}

impl JexecNginxController {
    pub fn new(pool: String) -> Self {
        Self { pool }
    }

    fn run_jexec(&self, jail_name: &str, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
        std::process::Command::new("jexec").arg(jail_name).args(args).output()
    }
}

impl NginxController for JexecNginxController {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError> {
        let spec_name = jail_name.strip_prefix("keel-").unwrap_or(jail_name);
        let rootfs = crate::record::jail_rootfs_path(&self.pool, spec_name);
        let config_dir = rootfs.join("usr/local/etc/nginx");
        std::fs::create_dir_all(&config_dir).map_err(|e| NginxError::Write(e.to_string()))?;
        let final_path = config_dir.join("nginx.conf");
        let tmp_path = config_dir.join("nginx.conf.tmp");
        std::fs::write(&tmp_path, config).map_err(|e| NginxError::Write(e.to_string()))?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| NginxError::Write(e.to_string()))?;
        Ok(())
    }

    fn test_config(&self, jail_name: &str) -> Result<(), NginxError> {
        let output = self.run_jexec(jail_name, &["/usr/local/sbin/nginx", "-t"]).map_err(|e| NginxError::ValidationFailed(e.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NginxError::ValidationFailed(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }

    fn reload(&self, jail_name: &str) -> Result<(), NginxError> {
        let output = self.run_jexec(jail_name, &["/usr/local/sbin/nginx", "-s", "reload"]).map_err(|e| NginxError::ReloadFailed(e.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NginxError::ReloadFailed(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }
}
```

(`jail_name.strip_prefix("keel-")` recovers the spec name `"ingress"` from the jail name `"keel-ingress"` that callers pass in, since `record::jail_rootfs_path` takes a spec name, not a jail name - every other call site in this plan already calls these methods with `"keel-ingress"` as `jail_name` to match `NginxController::write_config(jail_name: &str, ...)`'s own parameter name; confirm this round-trips correctly against `record::jail_name`/`jail_rootfs_path`'s real signatures before finalizing, since getting spec-name vs. jail-name backwards here would silently write nginx's config to the wrong path.)

- [ ] **Step 4: Compile-check on a non-FreeBSD OS**

Run: `cargo build -p keel-agentd --tests`
Expected: compiles cleanly (the FreeBSD-gated test file compiles down to nothing on macOS/Linux, but `JexecNginxController` itself is not `cfg`-gated and must always compile, matching `keel-net::process::ProcessNetManager`'s own non-gated real implementation).

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/nginx.rs keel-agentd/tests/freebsd_nginx.rs
git commit -m "feat(keel-agentd): add real JexecNginxController"
```

---

### Task 14: `keel-agentd`: host-level `pf` redirect rules

**Files:**
- Create: `keel-agentd/src/pf.rs`
- Modify: `keel-agentd/src/main.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Produces: `keel_agentd::pf::PfController` trait (`ensure_redirect_rules(&self, public_iface: &str, ingress_bridge_addr: &str) -> Result<(), PfError>`), `FakePfController`, `PfctlController` (real, shells to `pfctl`).
- Consumes: nothing from other tasks - applied once at startup from `main.rs`, independent of per-`Ingress` reconciliation, exactly as the design specifies ("applied once, not per-`Ingress`").

This is a single, once-at-startup call (with its own retry loop, not folded into `Reconciler::reconcile`), matching the design's explicit statement that a `pf` failure "doesn't block per-`Ingress` cert/config reconciliation, since it's a separate concern."

- [ ] **Step 1: Write the failing test in `keel-agentd/src/pf.rs`**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PfError {
    #[error("pfctl command failed: {0}")]
    Command(String),
}

pub trait PfController {
    fn ensure_redirect_rules(&self, public_iface: &str, ingress_bridge_addr: &str) -> Result<(), PfError>;
}

#[derive(Default)]
pub struct FakePfController {
    applied: std::sync::Mutex<Vec<(String, String)>>,
    fail: std::sync::Mutex<bool>,
}

impl FakePfController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail(&self, fail: bool) {
        *self.fail.lock().unwrap() = fail;
    }

    pub fn applied_rules(&self) -> Vec<(String, String)> {
        self.applied.lock().unwrap().clone()
    }
}

impl PfController for FakePfController {
    fn ensure_redirect_rules(&self, public_iface: &str, ingress_bridge_addr: &str) -> Result<(), PfError> {
        if *self.fail.lock().unwrap() {
            return Err(PfError::Command("simulated pfctl failure".to_string()));
        }
        self.applied.lock().unwrap().push((public_iface.to_string(), ingress_bridge_addr.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_redirect_rules_records_the_applied_rule() {
        let pf = FakePfController::new();
        pf.ensure_redirect_rules("em0", "10.0.0.2").unwrap();
        assert_eq!(pf.applied_rules(), vec![("em0".to_string(), "10.0.0.2".to_string())]);
    }

    #[test]
    fn ensure_redirect_rules_can_be_made_to_fail_for_retry_tests() {
        let pf = FakePfController::new();
        pf.set_fail(true);
        assert!(pf.ensure_redirect_rules("em0", "10.0.0.2").is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd pf::tests`
Expected: FAIL - module doesn't exist yet.

- [ ] **Step 3: Register the module and add the real `PfctlController`**

Add `pub mod pf;` to `keel-agentd/src/lib.rs`.

Append to `keel-agentd/src/pf.rs`:

```rust
pub struct PfctlController;

impl PfctlController {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PfctlController {
    fn default() -> Self {
        Self::new()
    }
}

impl PfController for PfctlController {
    fn ensure_redirect_rules(&self, public_iface: &str, ingress_bridge_addr: &str) -> Result<(), PfError> {
        let rules = format!(
            "rdr pass on {public_iface} inet proto tcp from any to {public_iface} port 80 -> {ingress_bridge_addr} port 80\nrdr pass on {public_iface} inet proto tcp from any to {public_iface} port 443 -> {ingress_bridge_addr} port 443\n"
        );
        let rules_path = std::path::Path::new("/usr/local/etc/keel/pf-ingress.conf");
        std::fs::create_dir_all(rules_path.parent().unwrap()).map_err(|e| PfError::Command(e.to_string()))?;
        std::fs::write(rules_path, &rules).map_err(|e| PfError::Command(e.to_string()))?;
        let output = std::process::Command::new("pfctl")
            .args(["-a", "keel-ingress", "-f"])
            .arg(rules_path)
            .output()
            .map_err(|e| PfError::Command(e.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(PfError::Command(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }
}
```

(`pfctl -a keel-ingress -f <file>` loads the rules into a named anchor rather than the main ruleset, so this never clobbers any other `pf` rules already on the host - the host's own top-level `pf.conf` needs one `anchor "keel-ingress"` line pointing at this anchor, which is host setup, not something `keel-agentd` writes; document this as a deployment prerequisite alongside the dev VM's other manual setup steps, not something to silently assume exists.)

- [ ] **Step 4: Wire a startup call and its own retry loop into `main.rs`**

```rust
    if let Some(public_iface) = config.public_iface.clone() {
        thread::spawn(move || {
            let pf = keel_agentd::pf::PfctlController::new();
            let mut backoff = keel_agentd::backoff::BackoffState::new();
            loop {
                let now = std::time::Instant::now();
                if backoff.can_retry(now) {
                    backoff.record_attempt(now);
                    if let Err(e) = pf.ensure_redirect_rules(&public_iface, keel_agentd::record::INGRESS_JAIL_BRIDGE_ADDR) {
                        eprintln!("keel-agentd: failed to apply pf ingress redirect rules: {e}");
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
    }
```

Add `public_iface: Option<String>` to `Config` in `main.rs` (default `None`) and a `--public-iface` flag parsed the same way every other optional flag already is.

`keel_agentd::record::INGRESS_JAIL_BRIDGE_ADDR` is a new `pub const &str = "10.0.0.2"` added to `keel-agentd/src/record.rs` in Task 10 instead of a literal repeated in two files: Task 10's synthesized `JailSpec.spec.network.address` and this task's `pf` rule both read from the one constant, so a later real-VM correction to the address (flagged as likely in Task 10) only needs to change in one place. Go back and update Task 10's Step 3 to use `format!("{}/24", keel_agentd::record::INGRESS_JAIL_BRIDGE_ADDR)` for `network.address` rather than the literal `"10.0.0.2/24"` shown there, and add the constant itself near `record.rs`'s other `pub fn`s:

```rust
pub const INGRESS_JAIL_BRIDGE_ADDR: &str = "10.0.0.2";
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-agentd pf::tests`
Expected: PASS, 2 tests.

- [ ] **Step 6: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keel-agentd`
Expected: PASS.

- [ ] **Step 7: Run clippy**

Run: `cargo clippy -p keel-agentd --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add keel-agentd/src/pf.rs keel-agentd/src/main.rs keel-agentd/src/lib.rs
git commit -m "feat(keel-agentd): apply host-level pf redirect rules for ingress"
```

---

### Task 15: `keel-ingress`: real `OvhDnsProvider`

**Files:**
- Modify: `keel-ingress/Cargo.toml` (add `ureq`, `sha1`)
- Create: `keel-ingress/src/ovh.rs`
- Modify: `keel-ingress/src/lib.rs`

**Interfaces:**
- Consumes: `keel_ingress::DnsProvider`/`DnsError` (Task 2).
- Produces: `keel_ingress::OvhDnsProvider::new(app_key: String, app_secret: String, consumer_key: String, zone: String) -> Self`, implementing `DnsProvider`.

The `zone` is taken as explicit constructor config rather than derived from the host automatically (resolving this milestone's own "left to implementation-time discovery" open question conservatively): a single real domain's zone is simple daemon config, and auto-derivation would need OVH's own zone-listing API and label-boundary guessing that this milestone's Non-Goals (no wildcard/multi-domain support) don't require.

- [ ] **Step 1: Add dependencies**

In `keel-ingress/Cargo.toml`:

```toml
[dependencies]
thiserror = "1"
ureq = { version = "3", features = ["rustls"] }
sha1 = "0.10"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

(Verify `ureq = "3"`'s TLS feature name against whatever version actually resolves at `cargo add ureq --dry-run` time - this was checked as "rustls enabled by default" against docs.rs on 2026-07-23, but pin the exact feature flag name the resolved version prints if `cargo build` complains about an unknown feature.)

- [ ] **Step 2: Write the failing unit test in `keel-ingress/src/ovh.rs`**

This tests only the request-signing function in isolation (pure, no network) - the actual HTTP round-trip against OVH's real API is exercised in Task 19's real verification, not here, since this milestone's fakes-only tests must never make a real network call.

```rust
use crate::dns::{DnsError, DnsProvider};
use sha1::{Digest, Sha1};

pub struct OvhDnsProvider {
    app_key: String,
    app_secret: String,
    consumer_key: String,
    zone: String,
    endpoint: String,
}

impl OvhDnsProvider {
    pub fn new(app_key: String, app_secret: String, consumer_key: String, zone: String) -> Self {
        Self { app_key, app_secret, consumer_key, zone, endpoint: "https://eu.api.ovh.com/1.0".to_string() }
    }

    fn sign(&self, method: &str, url: &str, body: &str, timestamp: u64) -> String {
        let to_hash = format!("{}+{}+{}+{}+{}+{}", self.app_secret, self.consumer_key, method, url, body, timestamp);
        let mut hasher = Sha1::new();
        hasher.update(to_hash.as_bytes());
        format!("$1${:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_produces_the_dollar_one_dollar_prefixed_sha1_hex_digest() {
        let provider = OvhDnsProvider::new("app-key".to_string(), "app-secret".to_string(), "consumer-key".to_string(), "example.com".to_string());
        let signature = provider.sign("POST", "https://eu.api.ovh.com/1.0/domain/zone/example.com/record", "{}", 1_800_000_000);
        assert!(signature.starts_with("$1$"));
        assert_eq!(signature.len(), 3 + 40); // "$1$" + 40 hex chars of a SHA1 digest
    }

    #[test]
    fn sign_is_deterministic_for_the_same_inputs() {
        let provider = OvhDnsProvider::new("app-key".to_string(), "app-secret".to_string(), "consumer-key".to_string(), "example.com".to_string());
        let a = provider.sign("POST", "https://eu.api.ovh.com/1.0/domain/zone/example.com/record", "{}", 1_800_000_000);
        let b = provider.sign("POST", "https://eu.api.ovh.com/1.0/domain/zone/example.com/record", "{}", 1_800_000_000);
        assert_eq!(a, b);
    }

    #[test]
    fn sign_changes_when_the_timestamp_changes() {
        let provider = OvhDnsProvider::new("app-key".to_string(), "app-secret".to_string(), "consumer-key".to_string(), "example.com".to_string());
        let a = provider.sign("POST", "https://eu.api.ovh.com/1.0/domain/zone/example.com/record", "{}", 1_800_000_000);
        let b = provider.sign("POST", "https://eu.api.ovh.com/1.0/domain/zone/example.com/record", "{}", 1_800_000_001);
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p keel-ingress ovh`
Expected: PASS, 3 tests (this is implement-then-test rather than strictly test-first, since `sign`'s correctness is only checkable once it exists - still write the test file with the implementation inline as shown, run once to confirm, per this task's step ordering).

- [ ] **Step 4: Implement the real `DnsProvider` methods against OVH's REST API**

Append to `keel-ingress/src/ovh.rs`:

```rust
    fn request(&self, method: &str, path: &str, body: &str) -> Result<String, DnsError> {
        let url = format!("{}{}", self.endpoint, path);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| DnsError::Request(e.to_string()))?
            .as_secs();
        let signature = self.sign(method, &url, body, timestamp);
        let agent = ureq::Agent::new_with_defaults();
        let mut request = agent
            .request(method, &url)
            .header("X-Ovh-Application", &self.app_key)
            .header("X-Ovh-Consumer", &self.consumer_key)
            .header("X-Ovh-Timestamp", &timestamp.to_string())
            .header("X-Ovh-Signature", &signature)
            .header("Content-Type", "application/json");
        let response = if body.is_empty() {
            request.call()
        } else {
            request.send(body.as_bytes())
        }
        .map_err(|e| DnsError::Request(e.to_string()))?;
        response
            .into_body()
            .read_to_string()
            .map_err(|e| DnsError::Request(e.to_string()))
    }
```

(`ureq::Agent`'s exact builder/request API must be checked against whatever `ureq` version actually resolves in `Cargo.lock` after Step 1 - this was checked against docs.rs on 2026-07-23 for `ureq` 3.x's blocking `Agent`/`.request(method, url)` shape, but if the resolved version's method names differ, run `cargo doc --open -p ureq` and adjust `request`'s body to match rather than fighting the pinned snippet above.)

```rust
impl DnsProvider for OvhDnsProvider {
    fn create_txt_record(&self, name: &str, value: &str) -> Result<(), DnsError> {
        let sub_domain = name.trim_end_matches(&format!(".{}", self.zone)).trim_end_matches(&self.zone);
        let body = serde_json::json!({ "fieldType": "TXT", "subDomain": sub_domain, "target": value, "ttl": 60 }).to_string();
        self.request("POST", &format!("/domain/zone/{}/record", self.zone), &body)?;
        self.request("POST", &format!("/domain/zone/{}/refresh", self.zone), "")?;
        Ok(())
    }

    fn delete_txt_record(&self, name: &str) -> Result<(), DnsError> {
        let sub_domain = name.trim_end_matches(&format!(".{}", self.zone)).trim_end_matches(&self.zone);
        let list_body = self.request("GET", &format!("/domain/zone/{}/record?fieldType=TXT&subDomain={sub_domain}", self.zone), "")?;
        let ids: Vec<u64> = serde_json::from_str(&list_body).map_err(|e| DnsError::Request(e.to_string()))?;
        for id in ids {
            self.request("DELETE", &format!("/domain/zone/{}/record/{id}", self.zone), "")?;
        }
        self.request("POST", &format!("/domain/zone/{}/refresh", self.zone), "")?;
        Ok(())
    }

    fn wait_for_propagation(&self, name: &str, value: &str) -> Result<(), DnsError> {
        const MAX_ATTEMPTS: u32 = 30;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
        for _ in 0..MAX_ATTEMPTS {
            let output = std::process::Command::new("host").args(["-t", "TXT", name]).output().map_err(|e| DnsError::Request(e.to_string()))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains(value) {
                return Ok(());
            }
            std::thread::sleep(RETRY_DELAY);
        }
        Err(DnsError::PropagationTimeout(name.to_string()))
    }
}
```

- [ ] **Step 5: Register in `keel-ingress/src/lib.rs`**

```rust
pub mod acme;
pub mod config;
pub mod dns;
pub mod ovh;

pub use acme::{AcmeClient, AcmeError, Cert, FakeAcmeClient};
pub use config::{render_nginx_config, IngressBackendConfig};
pub use dns::{DnsError, DnsProvider, FakeDnsProvider};
pub use ovh::OvhDnsProvider;
```

- [ ] **Step 6: Run all fakes-only tests to confirm no regression**

Run: `cargo test -p keel-ingress`
Expected: PASS (this task adds zero new tests requiring network access - `create_txt_record`/`delete_txt_record`/`wait_for_propagation` are exercised for real only in Task 19, against the real dev VM and real OVH account).

- [ ] **Step 7: Run clippy**

Run: `cargo clippy -p keel-ingress --all-targets -- -D warnings`
Expected: no warnings (fix any lint from `ureq`'s actual resolved API shape).

- [ ] **Step 8: Commit**

```bash
git add keel-ingress/Cargo.toml keel-ingress/src/ovh.rs keel-ingress/src/lib.rs
git commit -m "feat(keel-ingress): add real OvhDnsProvider"
```

---

### Task 16: `keel-ingress`: real `AcmeClient` wrapping `instant-acme`

**Files:**
- Modify: `keel-ingress/Cargo.toml` (add `tokio`, `instant-acme`)
- Create: `keel-ingress/src/instant_acme_client.rs`
- Modify: `keel-ingress/src/lib.rs`

**Interfaces:**
- Consumes: `keel_ingress::{AcmeClient, DnsProvider, Cert, AcmeError}` (Task 2/3).
- Produces: `keel_ingress::InstantAcmeClient::new(directory_url: String, account_key_path: std::path::PathBuf) -> Self`, implementing `AcmeClient` synchronously (bridging into `instant-acme`'s async API via an internal `tokio::runtime::Runtime`).

The exact `instant-acme` 0.8 API surface (`Account`/`AccountBuilder`/`NewOrder`/`Authorizations`/`ChallengeHandle`/`OrderStatus` polling loop) is more involved than can be pinned reliably without running `cargo doc --open -p instant-acme` against the actually-resolved version - checked against docs.rs on 2026-07-23 only at the level of "these types exist and the flow is account → order → per-authorization DNS-01 challenge → poll → finalize → download," not exact method signatures. This is the one piece of this plan intentionally left to implementation-time discovery against the real crate docs, the same way Milestone 20 left bhyve's exact flag set to real-hardware discovery - do not guess at exact method names; read `cargo doc -p instant-acme --open` first.

- [ ] **Step 1: Add dependencies**

```toml
tokio = { version = "1", features = ["rt", "time"] }
instant-acme = "0.8"
```

- [ ] **Step 2: Write the shape of `InstantAcmeClient` and its test (integration-style, `#[ignore]`d, run manually against the real Let's Encrypt staging directory once Task 19 has real credentials)**

```rust
use crate::acme::{AcmeClient, AcmeError, Cert};
use crate::dns::DnsProvider;
use std::path::PathBuf;

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
```

- [ ] **Step 3: Register in `keel-ingress/src/lib.rs`**

```rust
pub mod acme;
pub mod config;
pub mod dns;
pub mod instant_acme_client;
pub mod ovh;

pub use acme::{AcmeClient, AcmeError, Cert, FakeAcmeClient};
pub use config::{render_nginx_config, IngressBackendConfig};
pub use dns::{DnsError, DnsProvider, FakeDnsProvider};
pub use instant_acme_client::InstantAcmeClient;
pub use ovh::OvhDnsProvider;
```

- [ ] **Step 4: Confirm the crate still compiles (the `todo!()` body is filled in during Task 19's real verification, against real Let's Encrypt staging, where a stub would fail loudly and immediately rather than silently)**

Run: `cargo build -p keel-ingress`
Expected: builds (the `todo!()` only panics if actually called; no test calls `request_certificate_async` yet).

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p keel-ingress --all-targets -- -D warnings`
Expected: no warnings (add `#[allow(clippy::todo)]` on `request_certificate_async` only if clippy's default lint set flags `todo!()`, which it does not by default - confirm and adjust only if needed).

- [ ] **Step 6: Commit**

```bash
git add keel-ingress/Cargo.toml keel-ingress/src/instant_acme_client.rs keel-ingress/src/lib.rs
git commit -m "feat(keel-ingress): scaffold real InstantAcmeClient (implementation deferred to real-VM verification)"
```

---

### Task 17: `keelctl`: route `Ingress` specs

**Files:**
- Modify: `keelctl/src/main.rs`

**Interfaces:**
- Consumes: `keel_spec::parse_and_validate_ingress` (Task 1).
- Produces: `run_apply` recognizes `kind: Ingress` and `PUT`s to `/ingress/{name}`; `run_get`/`run_delete` gain the same for `get ingress <name>`/`delete ingress <name>`-style dispatch, matching however `run_get`/`run_delete` already distinguish `Jail` vs. `Service` (read those functions first; mirror their exact branching shape, since this file's current `Service` support in `get`/`delete` may already be name-based without a `kind` prefix - confirm before assuming a new `keelctl get ingress <name>` subcommand syntax is needed, versus `keelctl get <name>` working generically already).

- [ ] **Step 1: Write the failing test**

Add near existing `keelctl` tests exercising `run_apply`'s `Service` branch (search for a test calling `run_apply` against a `Service` YAML fixture, and mirror its exact shape - this project's `keelctl` tests spin up a real `keel-agentd`-equivalent test double or hit a Unix socket, matching the pattern already used):

```rust
#[test]
fn run_apply_routes_an_ingress_spec_to_the_ingress_endpoint_not_the_jails_endpoint() {
    let (socket, _guard) = start_test_agent("run_apply_routes_an_ingress_spec");
    let target = Target::Socket(socket);
    let dir = std::env::temp_dir().join("keelctl-test-ingress-run-apply.yaml");
    std::fs::write(
        &dir,
        "apiVersion: keel/v1\nkind: Ingress\nmetadata:\n  name: blog\nspec:\n  host: example.com\n  backend:\n    service: hugo-site\n    port: 8080\n  tls:\n    email: admin@example.com\n",
    ).unwrap();
    // Applying against an agent with no known 'hugo-site' Service is
    // expected to fail with a 400 from the ingress-specific validation,
    // NOT with a JailSpec YAML-parse error -- proving it was routed to
    // /ingress/blog rather than falling into the Jail else-branch.
    let result = run_apply(&target, &["-f".to_string(), dir.to_string_lossy().to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("hugo-site"), "expected the Service-not-found message, not a Jail parse error");
}
```

(Adapt `start_test_agent`/`Target::Socket` to whatever helper `keelctl`'s existing test module already provides - read `keelctl/src/main.rs`'s test module first, since this project's convention is clearly to reuse existing helpers rather than duplicate them, as seen in every other task of this plan.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p keelctl run_apply_routes_an_ingress_spec`
Expected: FAIL - currently falls into the `else` branch and errors on `keel_spec::parse_and_validate` (a `JailSpec` parse failure), not the `hugo-site` message.

- [ ] **Step 3: Add the `Ingress` branch to `run_apply`**

```rust
fn run_apply(target: &Target, args: &[String]) -> Result<String, String> {
    let index = args.iter().position(|a| a == "-f").ok_or("apply requires -f FILE")?;
    let file = args.get(index + 1).ok_or("apply requires -f FILE")?;
    let yaml = std::fs::read_to_string(file).map_err(|e| format!("failed to read {file}: {e}"))?;
    let kind = keel_spec::sniff_kind(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
    if kind == "Service" {
        let spec = keel_spec::parse_and_validate_service(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
        let path = jails_path(target, &format!("/services/{}", spec.metadata.name));
        success_body(dispatch(target, "PUT", &path, &yaml)).map(|_| String::new())
    } else if kind == "Ingress" {
        let spec = keel_spec::parse_and_validate_ingress(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
        let path = format!("/ingress/{}", spec.metadata.name);
        success_body(dispatch(target, "PUT", &path, &yaml)).map(|_| String::new())
    } else {
        let spec = keel_spec::parse_and_validate(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
        let path = jails_path(target, &format!("/jails/{}", spec.metadata.name));
        success_body(dispatch(target, "PUT", &path, &yaml)).map(|_| String::new())
    }
}
```

(`Ingress` uses a bare `/ingress/{name}` path, not `jails_path(target, ...)` - per this milestone's Non-Goals, `Ingress` is never routed through a control plane, so the `--node`-aware path-prefixing `jails_path` exists for is simply not applicable here.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p keelctl run_apply_routes_an_ingress_spec`
Expected: PASS.

- [ ] **Step 5: Run the full existing test suite to confirm no regression**

Run: `cargo test -p keelctl`
Expected: PASS.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p keelctl --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add keelctl/src/main.rs
git commit -m "feat(keelctl): route kind: Ingress specs to /ingress instead of /jails"
```

---

### Task 18: `keel-agentd`: daemon config for ACME directory URL and OVH credentials, wire the real implementations into `main.rs`

**Files:**
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_ingress::{OvhDnsProvider, InstantAcmeClient}` (Task 15/16).
- Produces: `main.rs` reads `/usr/local/etc/keel/dns-ovh.toml` (OVH app key/secret/consumer key, zone) and a `--acme-directory-url`/`--acme-account-key-file` flag pair at startup, constructing the real `Reconciler` with `Box::new(OvhDnsProvider::new(...))`/`Box::new(InstantAcmeClient::new(...)?)` instead of the `Fake*` placeholders Task 11 left in `main.rs`.

- [ ] **Step 1: Write the failing test for the TOML config parser**

Add a small parsing function and test it directly (no existing config-file parsing precedent in this project to mirror beyond plain CLI flags, so this introduces the project's first config-file reader; keep it minimal - a hand-rolled key=value or a `toml` crate dependency, whichever is less code: prefer adding `toml = "0.8"` + `serde` derive, since hand-rolling TOML parsing is exactly the kind of unnecessary reinvention this project's own conventions elsewhere avoid, e.g. reusing `serde_yaml` everywhere rather than a hand-rolled YAML reader):

```rust
#[derive(serde::Deserialize)]
struct OvhConfig {
    app_key: String,
    app_secret: String,
    consumer_key: String,
    zone: String,
}

fn load_ovh_config(path: &std::path::Path) -> OvhConfig {
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read OVH config at {}: {e}", path.display()));
    toml::from_str(&content).unwrap_or_else(|e| panic!("failed to parse OVH config at {}: {e}", path.display()))
}

#[cfg(test)]
mod ovh_config_tests {
    use super::*;

    #[test]
    fn load_ovh_config_parses_a_well_formed_file() {
        let dir = std::env::temp_dir().join("keel-agentd-ovh-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dns-ovh.toml");
        std::fs::write(&path, "app_key = \"ak\"\napp_secret = \"as\"\nconsumer_key = \"ck\"\nzone = \"example.com\"\n").unwrap();
        let config = load_ovh_config(&path);
        assert_eq!(config.app_key, "ak");
        assert_eq!(config.zone, "example.com");
    }
}
```

Add `toml = "0.8"` to `keel-agentd/Cargo.toml`'s `[dependencies]`.

- [ ] **Step 2: Run test to verify it fails, then passes once implemented**

Run: `cargo test -p keel-agentd load_ovh_config`
Expected: FAIL to compile first (no `toml` dependency / function), then PASS once Step 1's code and `Cargo.toml` edit both land.

- [ ] **Step 3: Add `--dns-ovh-config`, `--acme-directory-url`, `--acme-account-key-file` flags and wire the real implementations**

Add to `Config`: `dns_ovh_config: Option<PathBuf>, acme_directory_url: Option<String>, acme_account_key_file: Option<PathBuf>`, parsed the same way every other optional flag already is in `parse_args_from`.

Where `Reconciler::new(...)` is called in `main()`, replace the `Fake*` placeholders:

```rust
    let (acme, dns): (Box<dyn keel_ingress::AcmeClient + Send>, Box<dyn keel_ingress::DnsProvider + Send>) =
        match (&config.dns_ovh_config, &config.acme_directory_url, &config.acme_account_key_file) {
            (Some(ovh_config_path), Some(directory_url), Some(account_key_file)) => {
                let ovh_config = load_ovh_config(ovh_config_path);
                let dns = keel_ingress::OvhDnsProvider::new(ovh_config.app_key, ovh_config.app_secret, ovh_config.consumer_key, ovh_config.zone);
                let acme = keel_ingress::InstantAcmeClient::new(directory_url.clone(), account_key_file.clone())
                    .expect("failed to initialize the real ACME client");
                (Box::new(acme), Box::new(dns))
            }
            (None, None, None) => (Box::new(keel_ingress::FakeAcmeClient::new()), Box::new(keel_ingress::FakeDnsProvider::new())),
            _ => panic!("--dns-ovh-config, --acme-directory-url, and --acme-account-key-file must all be set together, or none of them"),
        };
```

Pass `acme, dns` into `Reconciler::new(...)`'s new parameter positions from Task 11, alongside the `nginx`/`service_vips` arguments from Task 12 (`Box::new(keel_agentd::nginx::FakeNginxController::new())` swapped for `Box::new(keel_agentd::nginx::JexecNginxController::new(config.pool.clone()))`, matching this same "fake by default, real once configured" shape - or, simpler and consistent with how this project has no other "fake in prod by default" precedent, always construct the real `JexecNginxController` in `main.rs` unconditionally, since unlike ACME/OVH it needs no external account and is always correct to use in a running daemon).

- [ ] **Step 4: Run the full existing test suite to confirm no regression**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Run clippy across the whole workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/main.rs keel-agentd/Cargo.toml
git commit -m "feat(keel-agentd): wire real OvhDnsProvider/InstantAcmeClient/JexecNginxController into main.rs"
```

---

### Task 19: Real FreeBSD VM verification

**Files:** none (verification only - fixes discovered along the way land as amendments to earlier tasks' files, committed separately with a message noting which real-VM finding they fix, matching how prior milestones' READMEs describe real-VM bugs found and fixed).

**Interfaces:** none new.

This task cannot start until the user has provided: a real domain name delegated to OVH DNS, and OVH API credentials (application key, application secret, consumer key). Ask for these explicitly before proceeding past Step 1 - do not fabricate placeholder values.

- [ ] **Step 1: Confirm prerequisites are in hand**

Ask the user (if not already provided) for: the domain name to use, and the three OVH API credential values (or a path to where they're already stored). Do not proceed to Step 2 without them.

- [ ] **Step 2: Prepare the `base/keel-ingress` base image on the dev VM**

As root on `root@192.168.64.2`, following this project's existing base-image convention (e.g. `zroot/keel/base/test` per `keel-jail/tests/freebsd_lifecycle.rs`'s own documented prerequisite): create a `zroot/keel/base/keel-ingress` dataset, install `nginx` into it (`pkg -c <mountpoint> install nginx` or equivalent), and snapshot it `@keel` so `clone_from_base` can clone it, exactly like every other base image in this project.

- [ ] **Step 3: Fill in `InstantAcmeClient::request_certificate_async`'s real body**

Run `cargo doc -p keel-ingress --open` (with `instant-acme` now resolved in `Cargo.lock`) and replace Task 16 Step 2's `todo!()` with the real account/order/DNS-01/finalize flow described in that task's comments, using `instant-acme`'s actual API. Parse the real certificate's `notAfter` to replace Task 11's `90 * 24 * 60 * 60` placeholder expiry with the actual expiry.

- [ ] **Step 4: Write `/usr/local/etc/keel/dns-ovh.toml` on the dev VM with the real credentials**

```toml
app_key = "<real value>"
app_secret = "<real value>"
consumer_key = "<real value>"
zone = "<the real domain's zone>"
```

- [ ] **Step 5: Start `keel-agentd` on the dev VM pointed at Let's Encrypt's staging directory**

```bash
keel-agentd --pool zroot --dns-ovh-config /usr/local/etc/keel/dns-ovh.toml \
  --acme-directory-url https://acme-staging-v02.api.letsencrypt.org/directory \
  --acme-account-key-file /var/db/keel/acme-account.key \
  --control-plane-addr <colocated control-plane addr> --node-id vps-1 \
  --advertise-addr <addr> --replicate-addr <addr> \
  --tls-ca-file ... --tls-cert-file ... --tls-key-file ... --tls-crl-file ...
```

(Run `keel-controlplane` alongside it too, per this design's "Deployment topology" note - a single-node `Ingress` deployment still needs a colocated control plane, since `Service` requires one.)

- [ ] **Step 6: Apply a minimal test backend `Service` and an `Ingress` pointing at it**

Using a trivial `Service` (any HTTP response distinguishable enough to confirm routing - per this milestone's Non-Goals, not the real Hugo/Umami images) and an `Ingress` naming the real domain and a real contact email, confirm: the ACME order completes, the DNS-01 challenge is served via the real OVH account, a real Let's Encrypt **staging** certificate is issued, and `curl -k --resolve <domain>:443:<ingress jail's bridge address> https://<domain>/` (run from the dev VM itself, since there's no public IP yet) gets a response through nginx.

- [ ] **Step 7: Exercise the renewal path with an artificially shortened threshold**

Temporarily lower `RENEWAL_THRESHOLD_SECS` in `keel-agentd/src/reconciler.rs` (or add a `--cert-renewal-threshold-secs` override flag if that's cleaner - prefer the flag, since editing a `const` for a manual test and reverting it is more error-prone than a flag this plan can leave in permanently) to something like 89 days' worth of seconds relative to a freshly-issued 90-day cert, restart, and confirm a real renewal issuance happens on the next reconcile pass without any manual intervention.

- [ ] **Step 8: Run once against Let's Encrypt's production directory**

Repeat Step 5 with `--acme-directory-url https://acme-v02.api.letsencrypt.org/directory` (mind Let's Encrypt's real rate limits - this is a single confirming run, not iteration) and confirm a real production certificate issues cleanly.

- [ ] **Step 9: Update the design doc and README**

Update `docs/superpowers/specs/2026-07-22-keel-agent-milestone21-ingress-https-design.md`'s Status line to "Approved, implemented" with a dated note on what was verified (mirroring every prior milestone's README entry style: test count, clippy status, what real-VM verification covered). Add a "Milestone 21" entry to `README.md`'s Roadmap section under a new "Sub-project 9: ingress and automatic HTTPS" heading, following the exact prose style of the Milestone 15-19 entries immediately above it.

- [ ] **Step 10: Commit**

```bash
git add docs/superpowers/specs/2026-07-22-keel-agent-milestone21-ingress-https-design.md README.md
git commit -m "docs: mark Milestone 21 (ingress and automatic HTTPS) implemented, real-VM verification passed"
```
