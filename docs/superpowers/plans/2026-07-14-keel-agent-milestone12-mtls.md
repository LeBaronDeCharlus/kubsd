# Milestone 12: Mutual TLS for the Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Milestone 11's shared bearer token with mutual TLS: every connection between `keelctl`, `keel-controlplane`, and `keel-agentd` is encrypted and every party proves its identity with a certificate signed by a private CA, verified during the TLS handshake itself, before any HTTP request is parsed.

**Architecture:** A new `scripts/gen-certs.sh` shells out to `openssl` to generate a CA once and issue named leaf certificates (dual server+client use for nodes and the control plane, client-only for operators). A `tls` module (near-identical shape in `keel-controlplane` and `keel-agentd`; a smaller client-only version in `keelctl`) loads PEM certs/keys into `rustls::ServerConfig`/`rustls::ClientConfig` and installs the `ring` crypto provider once per process. Every accepted TCP connection is wrapped in a `rustls::StreamOwned<ServerConnection, TcpStream>` before any HTTP parsing happens; every outbound connection is wrapped in a `rustls::StreamOwned<ClientConnection, TcpStream>` before any HTTP request is written. `keel-agentd`'s Unix socket keeps using plain `UnixStream`, untouched. Milestone 11's `auth` modules, `--auth-token-file`, and `keel-agentd`'s `route`/`route_authenticated` split are deleted outright.

**Tech Stack:** Rust, `rustls` (`default-features = false`, `features = ["ring"]`), `rustls-pemfile`, `httparse`, `serde_yaml`, `openssl` (CLI, for cert issuance only, never linked into any binary).

## Global Constraints

- `rustls` is added to `keel-controlplane`, `keel-agentd`, and `keelctl`'s `Cargo.toml` with `default-features = false, features = ["ring"]` — never the default `aws-lc-rs` backend, which is C/assembly, not Rust, and would add a build toolchain requirement this project has never had. `rustls-pemfile` is added alongside it with default features.
- Every process that builds a `rustls::ServerConfig` or `rustls::ClientConfig` must call `tls::ensure_crypto_provider()` first (a `std::sync::Once`-guarded call to `rustls::crypto::ring::default_provider().install_default()`); `rustls` panics if a config is built with no provider installed.
- Every certificate's SAN, and every `ServerName` built for a handshake, is an IP address (`rustls::pki_types::ServerName::IpAddress`), never a DNS name — every address in this project's config is a literal IP.
- `gen-certs.sh` issues certificates with a 10-year validity by default, overridable with a trailing `--days N`; this milestone's own test fixtures are issued at `--days 36500` (~100 years) so they never need regenerating.
- No revocation (CRL/OCSP), no rotation/renewal automation, no differentiated authorization by identity: a valid certificate from any node or operator gets exactly the same access as any other.
- `keel-agentd`'s Unix socket (`run`/`handle_connection`/`route`) ends this plan **byte-for-byte unchanged** in logic — verified by an explicit task step that runs its existing tests without modification. (`route()` itself changes for everyone, since Milestone 11's auth check is removed from it entirely — but that change is identical on both the Unix and TCP paths, since both call the same `route()`; nothing Unix-socket-*specific* changes.)
- Milestone 11's `keel-controlplane::auth`/`keel-agentd::auth` modules, `--auth-token-file` on all three binaries, and `keel-agentd`'s `route_authenticated` wrapper are deleted outright, not deprecated. No compatibility shim.
- Spec reference: `docs/superpowers/specs/2026-07-14-keel-agent-milestone12-mtls-design.md`.

---

### Task 1: `scripts/gen-certs.sh`

**Files:**
- Create: `scripts/gen-certs.sh`
- Modify: `.gitignore` (add `/certs`, the default output directory for operator-generated real certificates — distinct from the committed `testdata/tls/` test fixtures Task 2 creates)

**Interfaces:**
- Produces: a shell script with three subcommands (`init`, `node <name> <addr> [--days N]`, `client <name> [--days N]`), writing to `$GEN_CERTS_OUT_DIR` (default `certs`) relative to the current working directory.

This task is pure shell, no Rust, no automated test suite entry — it's verified by actually running it and inspecting the output with `openssl x509 -text`.

- [ ] **Step 1: Write the script**

Create `scripts/gen-certs.sh`:

```sh
#!/bin/sh
#
# Generates the private CA and per-identity leaf certificates Milestone
# 12's mutual TLS uses. Every leaf certificate is signed by one CA and
# gets both serverAuth and clientAuth extended key usage, since a node's
# (and the control plane's own) single identity certificate is used both
# ways: server when accepting an inbound connection, client when dialing
# out. Validity defaults to 10 years, overridable with a trailing
# --days N; this project's own test fixtures use --days 36500 (~100
# years) so testdata/tls/ never needs regenerating.
#
# Usage:
#   ./scripts/gen-certs.sh init
#   ./scripts/gen-certs.sh node <name> <ip-address> [--days N]
#   ./scripts/gen-certs.sh client <name> [--days N]
#
# Output goes to $GEN_CERTS_OUT_DIR (default: ./certs).

set -eu

OUT_DIR="${GEN_CERTS_OUT_DIR:-certs}"
DEFAULT_DAYS=3650

mkdir -p "$OUT_DIR"

fail() {
    echo "gen-certs: $*" >&2
    exit 1
}

cmd="${1:-}"
[ -n "$cmd" ] || fail "usage: $0 <init|node|client> ..."
shift

issue_leaf() {
    name="$1"
    days="$2"
    san_line="$3"

    openssl genrsa -out "$OUT_DIR/$name.key" 4096
    openssl req -new -key "$OUT_DIR/$name.key" -subj "/CN=$name" -out "$OUT_DIR/$name.csr"

    ext_file=$(mktemp)
    trap 'rm -f "$ext_file"' EXIT
    if [ -n "$san_line" ]; then
        printf 'subjectAltName = %s\nextendedKeyUsage = serverAuth, clientAuth\n' "$san_line" > "$ext_file"
    else
        printf 'extendedKeyUsage = serverAuth, clientAuth\n' > "$ext_file"
    fi

    openssl x509 -req -in "$OUT_DIR/$name.csr" -CA "$OUT_DIR/ca.crt" -CAkey "$OUT_DIR/ca.key" \
        -CAcreateserial -out "$OUT_DIR/$name.crt" -days "$days" -sha256 -extfile "$ext_file"
    rm -f "$OUT_DIR/$name.csr" "$ext_file"
    trap - EXIT
}

case "$cmd" in
    init)
        [ -f "$OUT_DIR/ca.key" ] && fail "$OUT_DIR/ca.key already exists, refusing to overwrite"
        openssl genrsa -out "$OUT_DIR/ca.key" 4096
        openssl req -x509 -new -nodes -key "$OUT_DIR/ca.key" -sha256 -days "$DEFAULT_DAYS" \
            -subj "/CN=keel-cluster-ca" -out "$OUT_DIR/ca.crt"
        echo "gen-certs: wrote $OUT_DIR/ca.crt and $OUT_DIR/ca.key"
        ;;
    node)
        name="${1:-}"
        addr="${2:-}"
        [ -n "$name" ] && [ -n "$addr" ] || fail "usage: $0 node <name> <ip-address> [--days N]"
        shift 2
        days="$DEFAULT_DAYS"
        if [ "${1:-}" = "--days" ]; then
            days="${2:-}"
            [ -n "$days" ] || fail "--days requires a value"
        fi
        [ -f "$OUT_DIR/ca.key" ] || fail "run '$0 init' first"
        issue_leaf "$name" "$days" "IP:$addr"
        echo "gen-certs: wrote $OUT_DIR/$name.crt and $OUT_DIR/$name.key (SAN=IP:$addr)"
        ;;
    client)
        name="${1:-}"
        [ -n "$name" ] || fail "usage: $0 client <name> [--days N]"
        shift
        days="$DEFAULT_DAYS"
        if [ "${1:-}" = "--days" ]; then
            days="${2:-}"
            [ -n "$days" ] || fail "--days requires a value"
        fi
        [ -f "$OUT_DIR/ca.key" ] || fail "run '$0 init' first"
        issue_leaf "$name" "$days" ""
        echo "gen-certs: wrote $OUT_DIR/$name.crt and $OUT_DIR/$name.key"
        ;;
    *)
        fail "unknown subcommand: $cmd (expected init|node|client)"
        ;;
esac
```

Make it executable: `chmod +x scripts/gen-certs.sh`.

- [ ] **Step 2: Add `/certs` to `.gitignore`**

`.gitignore` currently reads:

```
/target
/.superpowers
/Cargo.lock
```

Add a line so it reads:

```
/target
/.superpowers
/Cargo.lock
/certs
```

(`/certs` is the script's default output directory for real, operator-generated material; it must never be committed. `testdata/tls/`, created by Task 2, is a different, deliberately-committed directory and is unaffected by this ignore rule.)

- [ ] **Step 3: Run it and inspect the output**

```bash
rm -rf /tmp/gen-certs-smoke && mkdir -p /tmp/gen-certs-smoke && cd /tmp/gen-certs-smoke
GEN_CERTS_OUT_DIR=. /path/to/repo/scripts/gen-certs.sh init
GEN_CERTS_OUT_DIR=. /path/to/repo/scripts/gen-certs.sh node smoke-node 127.0.0.1
GEN_CERTS_OUT_DIR=. /path/to/repo/scripts/gen-certs.sh client smoke-client
openssl x509 -in smoke-node.crt -noout -text | grep -A1 "Subject Alternative Name"
openssl x509 -in smoke-node.crt -noout -text | grep "TLS Web"
openssl x509 -in smoke-client.crt -noout -text | grep "TLS Web"
cd - && rm -rf /tmp/gen-certs-smoke
```

Expected: `smoke-node.crt`'s SAN line shows `IP Address:127.0.0.1`; both certs' Extended Key Usage line shows `TLS Web Server Authentication, TLS Web Client Authentication`.

- [ ] **Step 4: Commit**

```bash
git add scripts/gen-certs.sh .gitignore
git commit -m "Add scripts/gen-certs.sh for CA and per-identity certificate issuance"
```

---

### Task 2: Test fixtures (`testdata/tls/`)

**Files:**
- Create: `testdata/tls/ca.crt`, `testdata/tls/fixture-node.crt`, `testdata/tls/fixture-node.key`, `testdata/tls/fixture-client.crt`, `testdata/tls/fixture-client.key`, `testdata/tls/wrong-ca-node.crt`, `testdata/tls/wrong-ca-node.key`

**Interfaces:**
- Produces: committed PEM files every later task's tests read directly by relative path (e.g. `concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls/ca.crt")` from any crate, since `testdata/` lives at the workspace root, one level up from every crate directory).

The design spec's Testing Strategy describes the cross-CA rejection test as using a certificate "generated ad hoc in the test." This plan deviates slightly, for simplicity and to avoid requiring `openssl` at `cargo test` time: `wrong-ca-node` is generated once now, by an *entirely separate, throwaway* CA whose own certificate/key are never committed and never given to anything as a trust root, and committed as a static fixture alongside the real ones. The security property under test, a certificate signed by a CA nothing trusts is rejected, is identical either way; only the mechanics of *how* the wrong-CA material comes to exist differ (build time vs. test time).

- [ ] **Step 1: Generate the real fixture CA and its two leaf identities**

```bash
mkdir -p /tmp/keel-fixture-ca
GEN_CERTS_OUT_DIR=/tmp/keel-fixture-ca ./scripts/gen-certs.sh init
GEN_CERTS_OUT_DIR=/tmp/keel-fixture-ca ./scripts/gen-certs.sh node fixture-node 127.0.0.1 --days 36500
GEN_CERTS_OUT_DIR=/tmp/keel-fixture-ca ./scripts/gen-certs.sh client fixture-client --days 36500
```

- [ ] **Step 2: Generate the throwaway, never-committed second CA and its one leaf identity**

```bash
mkdir -p /tmp/keel-wrong-ca
GEN_CERTS_OUT_DIR=/tmp/keel-wrong-ca ./scripts/gen-certs.sh init
GEN_CERTS_OUT_DIR=/tmp/keel-wrong-ca ./scripts/gen-certs.sh node wrong-ca-node 127.0.0.1 --days 36500
```

- [ ] **Step 3: Copy the committed set into `testdata/tls/`**

```bash
mkdir -p testdata/tls
cp /tmp/keel-fixture-ca/ca.crt testdata/tls/ca.crt
cp /tmp/keel-fixture-ca/fixture-node.crt testdata/tls/fixture-node.crt
cp /tmp/keel-fixture-ca/fixture-node.key testdata/tls/fixture-node.key
cp /tmp/keel-fixture-ca/fixture-client.crt testdata/tls/fixture-client.crt
cp /tmp/keel-fixture-ca/fixture-client.key testdata/tls/fixture-client.key
cp /tmp/keel-wrong-ca/wrong-ca-node.crt testdata/tls/wrong-ca-node.crt
cp /tmp/keel-wrong-ca/wrong-ca-node.key testdata/tls/wrong-ca-node.key
rm -rf /tmp/keel-fixture-ca /tmp/keel-wrong-ca
```

Note: `testdata/tls/ca.crt` is the trust root every later task's tests build a `RootCertStore` from. `testdata/tls/wrong-ca-node.crt`/`.key` are signed by a CA that is never present anywhere in `testdata/`, so any test that presents them against a verifier trusting only `ca.crt` must see the handshake fail.

- [ ] **Step 4: Verify the fixture set is self-consistent**

```bash
openssl verify -CAfile testdata/tls/ca.crt testdata/tls/fixture-node.crt
openssl verify -CAfile testdata/tls/ca.crt testdata/tls/fixture-client.crt
openssl verify -CAfile testdata/tls/ca.crt testdata/tls/wrong-ca-node.crt
```

Expected: the first two print `...: OK`; the third fails with `unable to get local issuer certificate` (proving `wrong-ca-node` really is untrusted by `ca.crt`, the actual property every cross-CA test in this plan relies on).

- [ ] **Step 5: Commit**

```bash
git add testdata/tls/
git commit -m "Add committed TLS test fixtures (CA, node, client, and cross-CA certificates)"
```

---

### Task 3: `keel-controlplane::tls` module

**Files:**
- Create: `keel-controlplane/src/tls.rs`
- Modify: `keel-controlplane/src/lib.rs` (add `pub mod tls;`)
- Modify: `keel-controlplane/Cargo.toml` (add `rustls`, `rustls-pemfile`)

**Interfaces:**
- Produces: `keel_controlplane::tls::ensure_crypto_provider() -> ()`, `keel_controlplane::tls::load_server_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ServerConfig, String>`, `keel_controlplane::tls::load_client_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ClientConfig, String>`, `keel_controlplane::tls::server_name_from_addr(addr: &str) -> Result<rustls::pki_types::ServerName<'static>, String>`.

- [ ] **Step 1: Add dependencies**

`keel-controlplane/Cargo.toml` currently reads:

```toml
[package]
name = "keel-controlplane"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
thiserror = "1"
httparse = "1"
```

Add two lines to `[dependencies]`:

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
thiserror = "1"
httparse = "1"
rustls = { version = "0.23", default-features = false, features = ["ring"] }
rustls-pemfile = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `keel-controlplane/src/tls.rs` with the test module only:

```rust
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
        load_client_config(&fixture("fixture-client.crt"), &fixture("fixture-client.key"), &fixture("ca.crt"))
            .expect("expected a valid client config");
    }

    #[test]
    fn load_client_config_fails_on_a_malformed_ca_file() {
        let bad_ca = std::env::temp_dir().join(format!("keel-controlplane-tls-test-bad-ca-{}", std::process::id()));
        std::fs::write(&bad_ca, "not a certificate").unwrap();
        let err = load_client_config(&fixture("fixture-client.crt"), &fixture("fixture-client.key"), &bad_ca)
            .unwrap_err();
        assert!(err.contains("failed to"), "got: {err}");
    }

    #[test]
    fn server_name_from_addr_parses_the_host_and_drops_the_port() {
        let name = server_name_from_addr("192.168.64.4:7621").unwrap();
        assert_eq!(name, rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::new(192, 168, 64, 4).into()));
    }

    #[test]
    fn server_name_from_addr_rejects_a_non_ip_host() {
        assert!(server_name_from_addr("not-an-ip:7620").is_err());
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane tls::`
Expected: compile error — `load_server_config`/`load_client_config`/`server_name_from_addr`/`ensure_crypto_provider` not found in this scope.

- [ ] **Step 4: Write the implementation**

Add above the test module in `keel-controlplane/src/tls.rs`:

```rust
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
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("failed to install rustls ring crypto provider");
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
    rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse certificate file {}: {e}", path.display()))
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
```

Add `pub mod tls;` to `keel-controlplane/src/lib.rs:1` (alongside the existing `pub mod auth;`; Task 6 removes `auth` later, this task only adds `tls`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane tls::`
Expected: 6 tests pass.

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/Cargo.toml keel-controlplane/src/tls.rs keel-controlplane/src/lib.rs
git commit -m "Add TLS config loading to keel-controlplane"
```

---

### Task 4: `keel-agentd::tls` module

**Files:**
- Create: `keel-agentd/src/tls.rs`
- Modify: `keel-agentd/src/lib.rs` (add `pub mod tls;`)
- Modify: `keel-agentd/Cargo.toml` (add `rustls`, `rustls-pemfile`)

**Interfaces:**
- Produces: `keel_agentd::tls::ensure_crypto_provider() -> ()`, `keel_agentd::tls::load_server_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ServerConfig, String>`, `keel_agentd::tls::load_client_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ClientConfig, String>`, `keel_agentd::tls::server_name_from_addr(addr: &str) -> Result<rustls::pki_types::ServerName<'static>, String>`.

Structurally identical to Task 3, duplicated per-crate rather than shared (matching this project's established preference for small parallel implementations, the same choice already made for Milestone 11's `auth` modules).

- [ ] **Step 1: Add dependencies**

Add to `keel-agentd/Cargo.toml`'s `[dependencies]` (after `httparse = "1"`):

```toml
rustls = { version = "0.23", default-features = false, features = ["ring"] }
rustls-pemfile = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `keel-agentd/src/tls.rs` with the test module only (identical to Task 3's, differing only in the crate-name-derived temp file prefix):

```rust
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
```

Note: `keel-agentd` dials the control plane using its own node identity as the client certificate, so this crate's `load_client_config` test reuses `fixture-node.crt`/`.key` (a node's dual-use identity), not `fixture-client.crt` (which is reserved for `keelctl`, an operator identity).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-agentd tls::`
Expected: compile error — functions not found.

- [ ] **Step 4: Write the implementation**

Add above the test module in `keel-agentd/src/tls.rs` (byte-for-byte identical to Task 3's implementation):

```rust
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
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("failed to install rustls ring crypto provider");
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
    rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse certificate file {}: {e}", path.display()))
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
```

Add `pub mod tls;` to `keel-agentd/src/lib.rs:1` (alongside the existing `pub mod auth;`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-agentd tls::`
Expected: 6 tests pass.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/Cargo.toml keel-agentd/src/tls.rs keel-agentd/src/lib.rs
git commit -m "Add TLS config loading to keel-agentd"
```

---

### Task 5: `keelctl::tls` module (client-only)

**Files:**
- Create: `keelctl/src/tls.rs`
- Modify: `keelctl/src/main.rs` (add `mod tls;`)
- Modify: `keelctl/Cargo.toml` (add `rustls`, `rustls-pemfile`)

**Interfaces:**
- Produces: `keelctl`'s crate-private `tls::ensure_crypto_provider() -> ()`, `tls::load_client_config(cert_path: &Path, key_path: &Path, ca_path: &Path) -> Result<rustls::ClientConfig, String>`, `tls::server_name_from_addr(addr: &str) -> Result<rustls::pki_types::ServerName<'static>, String>`. `keelctl` never accepts an inbound connection, so it has no `load_server_config`.

`keelctl` is a binary-only crate (no `src/lib.rs`); `mod tls;` in `main.rs` makes `keelctl/src/tls.rs` a private submodule, the same way any binary crate gains a submodule.

- [ ] **Step 1: Add dependencies**

Add to `keelctl/Cargo.toml`'s `[dependencies]` (after `httparse = "1"`):

```toml
rustls = { version = "0.23", default-features = false, features = ["ring"] }
rustls-pemfile = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `keelctl/src/tls.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
    }

    #[test]
    fn load_client_config_succeeds_with_valid_fixtures() {
        load_client_config(&fixture("fixture-client.crt"), &fixture("fixture-client.key"), &fixture("ca.crt"))
            .expect("expected a valid client config");
    }

    #[test]
    fn load_client_config_fails_on_a_missing_key_file() {
        let err =
            load_client_config(&fixture("fixture-client.crt"), &fixture("does-not-exist.key"), &fixture("ca.crt"))
                .unwrap_err();
        assert!(err.contains("does-not-exist.key"), "got: {err}");
    }

    #[test]
    fn server_name_from_addr_parses_the_host_and_drops_the_port() {
        let name = server_name_from_addr("10.0.0.1:7620").unwrap();
        assert_eq!(name, rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::new(10, 0, 0, 1).into()));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keelctl tls::`
Expected: compile error — `tls` module doesn't exist / `mod tls;` not yet declared in `main.rs`.

- [ ] **Step 4: Write the implementation**

Add above the test module in `keelctl/src/tls.rs`:

```rust
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::RootCertStore;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Once;

static CRYPTO_PROVIDER_INIT: Once = Once::new();

pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("failed to install rustls ring crypto provider");
    });
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
    rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse certificate file {}: {e}", path.display()))
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
```

Add `mod tls;` near the top of `keelctl/src/main.rs` (after the existing `use` lines, e.g. line 8, before `const DEFAULT_SOCKET`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keelctl tls::`
Expected: 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add keelctl/Cargo.toml keelctl/src/tls.rs keelctl/src/main.rs
git commit -m "Add TLS client-config loading to keelctl"
```

---

### Task 6: `keel-controlplane::http` — inbound TLS, auth check removed

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `keel_controlplane::tls` (Task 3).
- Produces: `run(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<rustls::ServerConfig>)`, `route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>)` (loses the `token` parameter and its `auth::check` call). `forward`/`handle_forward`/`handle_scheduled_*` keep their Milestone-11-era `token: &str` signatures for now — Task 7 converts them to TLS client connections; this task only changes the *inbound* (accept) side.

- [ ] **Step 1: Write the failing tests**

In `keel-controlplane/src/http.rs`'s test module, replace the `TEST_TOKEN`-based `start_test_server`/`send_request` with TLS-backed equivalents, and replace the five `_without_auth_header_`/`_wrong_auth_token_` 401 tests with TLS-handshake-failure tests:

```rust
use crate::tls;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
}

fn start_test_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
    let tls_config = Arc::new(
        tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || run(listener, commands, tls_config));
    addr
}

fn client_tls_config() -> Arc<rustls::ClientConfig> {
    Arc::new(
        tls::load_client_config(&fixture("fixture-client.crt"), &fixture("fixture-client.key"), &fixture("ca.crt"))
            .unwrap(),
    )
}

fn wrong_ca_tls_config() -> Arc<rustls::ClientConfig> {
    Arc::new(
        tls::load_client_config(&fixture("wrong-ca-node.crt"), &fixture("wrong-ca-node.key"), &fixture("ca.crt"))
            .unwrap(),
    )
}

fn send_request(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    send_request_with(addr, method, path, body, &client_tls_config())
}

fn send_request_with(addr: &str, method: &str, path: &str, body: &str, client_config: &Arc<rustls::ClientConfig>) -> (u16, String) {
    let server_name = tls::server_name_from_addr(addr).unwrap();
    let tcp_stream = TcpStream::connect(addr).unwrap();
    let conn = rustls::ClientConnection::new(Arc::clone(client_config), server_name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).unwrap();
    stream.sock.shutdown(std::net::Shutdown::Write).ok();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(&response).unwrap() {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => panic!("incomplete response: {response:?}"),
    };
    let status = parsed.code.unwrap();
    let body = String::from_utf8(response[header_len..].to_vec()).unwrap();
    (status, body)
}

#[test]
fn a_client_with_no_certificate_cannot_complete_the_handshake() {
    let addr = start_test_server();
    let tcp_stream = TcpStream::connect(&addr).unwrap();
    // A ClientConfig built with no client cert at all: connects, but the
    // server requires one, so the handshake itself must fail.
    let roots = {
        let mut roots = rustls::RootCertStore::empty();
        let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(fixture("ca.crt")).unwrap()))
            .next().unwrap().unwrap();
        roots.add(cert).unwrap();
        roots
    };
    let bare_config = Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let server_name = tls::server_name_from_addr(&addr).unwrap();
    let conn = rustls::ClientConnection::new(bare_config, server_name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
    let result = stream.write_all(b"GET /nodes HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    assert!(result.is_err(), "expected the handshake to fail with no client certificate presented");
}

#[test]
fn a_client_with_a_wrong_ca_certificate_cannot_complete_the_handshake() {
    let addr = start_test_server();
    let result = std::panic::catch_unwind(|| send_request_with(&addr, "GET", "/nodes", "", &wrong_ca_tls_config()));
    assert!(result.is_err() || result.unwrap().0 != 200, "expected the handshake to fail for a wrong-CA client certificate");
}
```

Remove the five now-obsolete tests that asserted `401` for missing/wrong tokens (`register_without_auth_header_returns_401`, `heartbeat_with_wrong_auth_token_returns_401`, `get_nodes_without_auth_header_returns_401`, `named_node_forward_without_auth_header_returns_401_even_for_an_unknown_node`, `scheduled_apply_without_auth_header_returns_401`) — there is no more application-layer `401` for this; rejection now happens at the TLS layer, which the two new tests above cover. Also remove `send_request_raw` (no longer used by anything). **Leave `const TEST_TOKEN` in place for now** — it is still used by the two outbound forwarding-token-capture tests (`named_node_forward_attaches_the_control_planes_auth_token_to_the_outbound_request`, `scheduled_apply_attaches_the_control_planes_auth_token_to_the_outbound_request`), which this task does not touch; `forward()` itself isn't converted to TLS until Task 7, which is also where those two tests (and `TEST_TOKEN`) finally get removed.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --lib`
Expected: compile errors — `run` still takes a `token: Arc<String>` (not yet a `tls_config`), `route()` still takes a token, `send_request`'s callers throughout the rest of the test module (all the pre-Task-6 tests) still expect the old plain-TCP, bearer-token-based helpers.

- [ ] **Step 3: Write the implementation**

In `keel-controlplane/src/http.rs`:

1. Imports: replace `use crate::auth;` with `use crate::tls;`; add `use rustls::{ServerConfig, ServerConnection, StreamOwned};`.
2. A type alias for readability, added near the top:

```rust
type TlsStream = StreamOwned<ServerConnection, TcpStream>;
```

3. `run`/`handle_connection` (currently around lines 13-38) lose the token, gain a TLS config, and operate on `TlsStream` instead of `TcpStream`:

```rust
pub fn run(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<ServerConfig>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = Arc::clone(&tls_config);
        thread::spawn(move || {
            let Ok(conn) = ServerConnection::new(tls_config) else { return };
            let mut tls_stream = TlsStream::new(conn, stream);
            if handle_connection(&mut tls_stream, &commands).is_err() {
                eprintln!("keel-controlplane: TLS handshake or request handling failed for a connection");
            }
        });
    }
}

fn handle_connection(stream: &mut TlsStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response(stream, status, &body)
}
```

4. `ParsedRequest` drops `auth_header` entirely (nothing checks it anymore):

```rust
struct ParsedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}
```

5. `read_request` (currently ~lines 40-93) stops capturing the `Authorization` header, operates on `&mut TlsStream`, and its break tuple drops back to four elements:

```rust
fn read_request(stream: &mut TlsStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length) = loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let content_length = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_MESSAGE_BYTES {
                    return Ok(None);
                }
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Ok(None),
        }
    };

    let total_len = header_len + content_length;
    if total_len > MAX_MESSAGE_BYTES {
        return Ok(None);
    }
    while buf.len() < total_len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_len..total_len].to_vec();
    Ok(Some(ParsedRequest { method, path, body }))
}
```

6. `write_response` retypes its stream parameter to `&mut TlsStream`, body unchanged.
7. `route` (currently ~line 119) drops the token parameter and the `auth::check` call entirely, and its calls to `handle_forward`/`handle_scheduled_*` keep passing `token` for now (Task 7's job to remove) — **but since `route` itself no longer receives a token, this task must thread a *placeholder* removal carefully**: to keep this task's diff isolated to inbound concerns only, `route` keeps a `token: &str` parameter for **exactly one release**, sourced not from any auth mechanism but as an intentionally unused placeholder is NOT acceptable (no placeholders/dead parameters per this project's conventions). Instead: since Task 7 (immediately next) is what converts `forward()`'s signature away from `token` anyway, do both removals together in this task's `route` signature — drop `token` from `route` now, and temporarily hardcode an empty `""` token literal at `route`'s three call-sites-into forwarding functions (`handle_forward`, `handle_scheduled_apply`, `handle_scheduled_read`, `handle_scheduled_delete`), clearly commented as a stopgap Task 7 removes:

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, &request.body, commands),
        ("GET", ["nodes"]) => handle_list(commands),
        ("PUT", ["nodes", id, "jails", name]) => {
            // stopgap "" token, Task 7 replaces forward()'s auth entirely with TLS
            let (status, body) =
                handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands, "");
            if (200..300).contains(&status) {
                send_record_placement(name, id, commands);
            }
            (status, body)
        }
        ("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands, ""),
        ("GET", ["nodes", id, "jails", name]) => {
            handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands, "")
        }
        ("DELETE", ["nodes", id, "jails", name]) => {
            let (status, body) = handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands, "");
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, body)
        }
        ("PUT", ["jails", name]) => handle_scheduled_apply(name, &request.body, commands, ""),
        ("GET", ["jails", name]) => handle_scheduled_read(name, commands, ""),
        ("DELETE", ["jails", name]) => handle_scheduled_delete(name, commands, ""),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}
```

This mirrors the exact "stopgap, clearly commented, next task removes it" pattern Milestone 10's own plan used at an equivalent cross-task boundary (registry.rs/worker.rs literal placeholders while resource fields were threaded through in stages) — it is not a silent default, it is visibly non-functional (an empty bearer token satisfies nothing) and Task 7 deletes every trace of it in the very next task.

8. `reason_phrase`'s `401 => "Unauthorized"` line stays (harmless, no longer reachable from this crate's own code, but not worth removing in this task; Task 7/8 will naturally leave it as dead code the final workspace build's `#[warn(dead_code)]` would catch only if truly unused — since `error_response(401, ...)` still exists as a general-purpose helper other future error paths could use, `401` staying mapped is not a defect).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --lib`
Expected: all tests pass except the two forwarding-token-capture tests (`named_node_forward_attaches_the_control_planes_auth_token_to_the_outbound_request`, `scheduled_apply_attaches_the_control_planes_auth_token_to_the_outbound_request`), which now fail because `forward()` sends an empty `Authorization: Bearer ` header instead of a real token — **this is expected**, Task 7 removes those two tests (they test a mechanism that no longer exists) and replaces them with TLS-outbound equivalents. Run the narrower `cargo test -p keel-controlplane --lib -- --skip attaches_the_control_planes_auth_token` to confirm everything else is green.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Wrap keel-controlplane's inbound TCP connections in TLS, remove the bearer-token check from route()"
```

---

### Task 7: `keel-controlplane::http` — outbound TLS (`forward()`)

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `keel_controlplane::tls::load_client_config`/`server_name_from_addr` (Task 3).
- Produces: `forward(addr: &str, method: &str, path: &str, body: &[u8], client_config: &Arc<rustls::ClientConfig>) -> Result<(u16, Vec<u8>), String>`, `handle_forward(id: &str, method: &str, path: &str, body: &[u8], commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>)`, `handle_scheduled_apply(name: &str, body: &[u8], commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>)`, `handle_scheduled_read(name: &str, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>)`, `handle_scheduled_delete(name: &str, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>)`, `route(request: &ParsedRequest, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>)` (regains a parameter, this time for the outbound leg only — `route` never checks anything inbound with it, it only threads it through to the forwarding functions), and `run`/`handle_connection` (Task 6) each gain one more parameter, `client_config: Arc<rustls::ClientConfig>`, threaded straight through to `route` — `run`'s final signature is `run(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<ServerConfig>, client_config: Arc<rustls::ClientConfig>)`, the signature every later task (8, 10, 13) calls.

- [ ] **Step 1: Write the failing tests**

Replace the two token-capture tests removed in Task 6's expected-failure list with TLS equivalents, and update `start_fake_remote_agentd`/`start_fake_remote_agentd_capturing`/`register_node`/`start_test_server` to thread a `client_config` through:

```rust
fn start_fake_remote_tls_agentd(status: u16, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server_config = Arc::new(
        tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&server_config)) else { continue };
            let mut tls_stream = rustls::StreamOwned::new(conn, stream);
            let mut buf = [0u8; 4096];
            loop {
                match tls_stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tls_stream.write_all(response.as_bytes());
            let _ = tls_stream.flush();
        }
    });
    addr
}

#[test]
fn forward_over_tls_relays_status_and_body_from_the_target_node() {
    let cp_addr = start_test_server();
    let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
    register_node(&cp_addr, "node-1", &node_addr);

    let (status, body) = send_request(&cp_addr, "PUT", "/nodes/node-1/jails/web-1", "apiVersion: keel/v1\n");
    assert_eq!(status, 200);
    assert!(body.contains("running: true"), "expected relayed body, got: {body}");
}

#[test]
fn forward_to_a_node_presenting_a_wrong_ca_certificate_fails() {
    let cp_addr = start_test_server();
    // A "node" whose server certificate is signed by a CA the control
    // plane's own client config does not trust.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let node_addr = listener.local_addr().unwrap().to_string();
    let wrong_server_config = Arc::new(
        tls::load_server_config(&fixture("wrong-ca-node.crt"), &fixture("wrong-ca-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&wrong_server_config)) else { continue };
            let mut tls_stream = rustls::StreamOwned::new(conn, stream);
            let mut buf = [0u8; 4096];
            let _ = tls_stream.read(&mut buf);
        }
    });
    register_node(&cp_addr, "node-1", &node_addr);

    let (status, body) = send_request(&cp_addr, "GET", "/nodes/node-1/jails", "");
    assert_eq!(status, 500);
    assert!(body.contains("failed to reach node"), "expected a forwarding failure, got: {body}");
}
```

Remove `start_fake_remote_agentd`, `start_fake_remote_agentd_capturing`, and the now-superseded `forward_put_relays_status_and_body_from_the_target_node`/`forward_get_relays_status_and_body_from_the_target_node`/`forward_delete_relays_status_from_the_target_node`/`forward_to_a_node_with_nothing_listening_returns_500`/`scheduled_put_lands_on_the_lower_id_node_when_headroom_is_equal`/`scheduled_put_is_sticky_across_repeated_apply`/`scheduled_delete_removes_the_placement_so_a_later_get_returns_404`/`named_route_apply_and_scheduled_route_share_the_same_placement_table` tests **only if** they use `start_fake_remote_agentd` (plain TCP, no TLS) directly — since the target is now always TLS, every one of these needs its fake-remote helper swapped for `start_fake_remote_tls_agentd`, not deleted. Re-point every call site from `start_fake_remote_agentd(...)` to `start_fake_remote_tls_agentd(...)`, keep the tests' own logic and assertions completely unchanged.

Also remove the two outbound forwarding-token-capture tests Task 6 deliberately left in place (`named_node_forward_attaches_the_control_planes_auth_token_to_the_outbound_request`, `scheduled_apply_attaches_the_control_planes_auth_token_to_the_outbound_request`) — the two new tests added in Step 1 above (`forward_over_tls_relays_status_and_body_from_the_target_node`, `forward_to_a_node_presenting_a_wrong_ca_certificate_fails`) supersede them. Once those two are gone, `const TEST_TOKEN` has no remaining reference anywhere in this file — remove that line too.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --lib forward`
Expected: compile errors (`forward`/`handle_forward`/`handle_scheduled_*` still take `token: &str`, not `client_config`), and the two new tests don't pass as behavior yet.

- [ ] **Step 3: Write the implementation**

In `keel-controlplane/src/http.rs`:

1. `forward` (currently ~line 310) gains `client_config: &Arc<rustls::ClientConfig>`, drops `token`, and dials over TLS:

```rust
fn forward(addr: &str, method: &str, path: &str, body: &[u8], client_config: &Arc<rustls::ClientConfig>) -> Result<(u16, Vec<u8>), String> {
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "could not resolve address".to_string())?;
    let tcp_stream =
        TcpStream::connect_timeout(&socket_addr, FORWARD_CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    tcp_stream.set_read_timeout(Some(FORWARD_READ_TIMEOUT)).ok();
    let server_name = tls::server_name_from_addr(addr)?;
    let conn = rustls::ClientConnection::new(Arc::clone(client_config), server_name).map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);

    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    stream.sock.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(&response).map_err(|e| e.to_string())? {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Err("incomplete response".to_string()),
    };
    let status = parsed.code.ok_or_else(|| "missing status code".to_string())?;
    Ok((status, response[header_len..].to_vec()))
}
```

(`rustls::StreamOwned`'s public field is `sock` for the underlying transport; `stream.sock.shutdown(...)` reaches the wrapped `TcpStream` directly, the same half-close `forward()` already relied on before this task.)

2. `handle_forward` and the three scheduled handlers take `client_config: &Arc<rustls::ClientConfig>` instead of `token: &str`, passing it straight to `forward`:

```rust
fn handle_forward(id: &str, method: &str, path: &str, body: &[u8], commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Resolve(id.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let addr = match reply_rx.recv() {
        Ok(Ok(addr)) => addr,
        Ok(Err(e)) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, method, path, body, client_config) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_apply(name: &str, body: &[u8], commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ResolveOrSchedule(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let (node_id, addr) = match reply_rx.recv() {
        Ok(Ok(pair)) => pair,
        Ok(Err(ScheduleOrResolveError::Schedule(e))) => return error_response(503, e.to_string()),
        Ok(Err(ScheduleOrResolveError::Resolve(e))) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, "PUT", &format!("/jails/{name}"), body, client_config) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_record_placement(name, &node_id, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_read(name: &str, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "GET", &format!("/jails/{name}"), &[], client_config) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_delete(name: &str, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "DELETE", &format!("/jails/{name}"), &[], client_config) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}
```

3. `route` (Task 6 left it token-free with `""` stopgaps) gains `client_config: &Arc<rustls::ClientConfig>` and passes it in place of every `""` stopgap:

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, &request.body, commands),
        ("GET", ["nodes"]) => handle_list(commands),
        ("PUT", ["nodes", id, "jails", name]) => {
            let (status, body) =
                handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands, client_config);
            if (200..300).contains(&status) {
                send_record_placement(name, id, commands);
            }
            (status, body)
        }
        ("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands, client_config),
        ("GET", ["nodes", id, "jails", name]) => {
            handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands, client_config)
        }
        ("DELETE", ["nodes", id, "jails", name]) => {
            let (status, body) = handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands, client_config);
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, body)
        }
        ("PUT", ["jails", name]) => handle_scheduled_apply(name, &request.body, commands, client_config),
        ("GET", ["jails", name]) => handle_scheduled_read(name, commands, client_config),
        ("DELETE", ["jails", name]) => handle_scheduled_delete(name, commands, client_config),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}
```

4. `handle_connection` (from Task 6) gains `client_config` and passes it to `route`:

```rust
fn handle_connection(stream: &mut TlsStream, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> io::Result<()> {
    let request = match read_request(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands, client_config);
    write_response(stream, status, &body)
}
```

5. `run` gains a second `Arc`, this crate's own client config (used only for the *outbound* forwarding leg, entirely separate from the *inbound* `tls_config: Arc<ServerConfig>` Task 6 added):

```rust
pub fn run(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<ServerConfig>, client_config: Arc<rustls::ClientConfig>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = Arc::clone(&tls_config);
        let client_config = Arc::clone(&client_config);
        thread::spawn(move || {
            let Ok(conn) = ServerConnection::new(tls_config) else { return };
            let mut tls_stream = TlsStream::new(conn, stream);
            if handle_connection(&mut tls_stream, &commands, &client_config).is_err() {
                eprintln!("keel-controlplane: TLS handshake or request handling failed for a connection");
            }
        });
    }
}
```

6. Update `start_test_server` (test helper) to build and pass a `client_config` too, using the same `fixture-client.crt`/`.key` this control plane presents when dialing nodes:

```rust
fn start_test_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
    let tls_config = Arc::new(
        tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    let client_config = client_tls_config();
    thread::spawn(move || run(listener, commands, tls_config, client_config));
    addr
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --lib`
Expected: all tests pass, including the two new TLS-forwarding tests and every re-pointed pre-existing forwarding/scheduling test.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Wrap keel-controlplane's outbound forwarding connections in TLS"
```

---

### Task 8: Delete `keel-controlplane::auth`; `main.rs` flag/wiring replacement

**Files:**
- Delete: `keel-controlplane/src/auth.rs`
- Modify: `keel-controlplane/src/lib.rs` (remove `pub mod auth;`)
- Modify: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `keel_controlplane::tls::{ensure_crypto_provider, load_server_config, load_client_config}` (Task 3), `keel_controlplane::http::run(listener, commands, tls_config: Arc<ServerConfig>, client_config: Arc<ClientConfig>)` (Task 7).

- [ ] **Step 1: Delete the module**

```bash
git rm keel-controlplane/src/auth.rs
```

Remove `pub mod auth;` from `keel-controlplane/src/lib.rs:1`.

- [ ] **Step 2: Write the failing tests**

Replace `keel-controlplane/src/main.rs`'s existing test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_the_tls_flags() {
        let config = parse_args_from(args(&[
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
        ]));
        assert_eq!(config.tls_ca_file, Some(PathBuf::from("/etc/keel/ca.crt")));
        assert_eq!(config.tls_cert_file, Some(PathBuf::from("/etc/keel/controlplane.crt")));
        assert_eq!(config.tls_key_file, Some(PathBuf::from("/etc/keel/controlplane.key")));
    }

    #[test]
    #[should_panic(expected = "--tls-ca-file, --tls-cert-file, and --tls-key-file are all required")]
    fn missing_any_tls_flag_panics() {
        parse_args_from(args(&["--tls-ca-file", "/etc/keel/ca.crt"]));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --bin keel-controlplane`
Expected: compile error — no `tls_ca_file`/`tls_cert_file`/`tls_key_file` fields, no such panic message yet.

- [ ] **Step 4: Write the implementation**

Replace the whole of `keel-controlplane/src/main.rs` above the test module:

```rust
use keel_controlplane::placements::Placements;
use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

struct Config {
    addr: String,
    tls_ca_file: Option<PathBuf>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:7620".to_string(),
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
        }
    }
}

fn parse_args() -> Config {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Config {
    let mut config = Config::default();
    let mut args = args;
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--addr" => config.addr = value,
            "--tls-ca-file" => config.tls_ca_file = Some(PathBuf::from(value)),
            "--tls-cert-file" => config.tls_cert_file = Some(PathBuf::from(value)),
            "--tls-key-file" => config.tls_key_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.tls_ca_file.is_none() || config.tls_cert_file.is_none() || config.tls_key_file.is_none() {
        panic!("--tls-ca-file, --tls-cert-file, and --tls-key-file are all required");
    }
    config
}

fn main() {
    let config = parse_args();
    let ca_file = config.tls_ca_file.expect("validated as required in parse_args_from");
    let cert_file = config.tls_cert_file.expect("validated as required in parse_args_from");
    let key_file = config.tls_key_file.expect("validated as required in parse_args_from");

    let tls_config = keel_controlplane::tls::load_server_config(&cert_file, &key_file, &ca_file)
        .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
    let client_config = keel_controlplane::tls::load_client_config(&cert_file, &key_file, &ca_file)
        .unwrap_or_else(|e| panic!("failed to load TLS client config: {e}"));

    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, Arc::new(tls_config), Arc::new(client_config));
}
```

(`load_server_config`/`load_client_config` both call `tls::ensure_crypto_provider()` internally per Task 3's implementation, so `main` doesn't need to call it separately.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --bin keel-controlplane`
Expected: 2 tests pass.

- [ ] **Step 6: Commit**

```bash
git add -A keel-controlplane/
git commit -m "Delete keel-controlplane's auth module; require TLS cert/key/CA flags at startup"
```

---

### Task 9: `keel-agentd::http` — TLS on the TCP path, `route`/`route_authenticated` collapse

**Files:**
- Modify: `keel-agentd/src/http.rs`

**Interfaces:**
- Consumes: `keel_agentd::tls` (Task 4).
- Produces: `run(listener: UnixListener, commands: Sender<Command>)` (**unchanged**), `run_tcp(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<rustls::ServerConfig>)`, `route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>)` (used by **both** the Unix and TCP paths now — `route_authenticated` is deleted).

This is the safety-critical task, exactly as it was in Milestone 11: `run`/`handle_connection` (the Unix-socket path) must show zero logic changes.

- [ ] **Step 1: Write the failing tests**

Update `keel-agentd/src/http.rs`'s test module: replace `TEST_TOKEN`/`start_tcp_test_server`/`send_request_tcp`/`send_request_tcp_raw` with TLS-backed equivalents, and replace the two `_returns_401` tests with handshake-failure tests. Leave every Unix-socket test and helper (`start_test_server`, `send_request`) completely untouched.

```rust
use crate::tls;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
}

fn start_tcp_test_server(name: &str) -> String {
    let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-tcp-test-state-{name}"));
    let _ = std::fs::remove_dir_all(&state_dir);
    let zfs = FakeZfsManager::new();
    zfs.seed_dataset("zroot/keel/base/14.2-web");
    let reconciler = Reconciler::new(
        FakeJailRuntime::new(),
        zfs,
        FakeNetManager::new(),
        "zroot".to_string(),
        state_dir,
    )
    .unwrap();
    let (_worker_handle, commands) = worker::spawn(reconciler);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let tls_config = Arc::new(
        tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || run_tcp(listener, commands, tls_config));
    addr
}

fn client_tls_config() -> Arc<rustls::ClientConfig> {
    Arc::new(
        tls::load_client_config(&fixture("fixture-client.crt"), &fixture("fixture-client.key"), &fixture("ca.crt"))
            .unwrap(),
    )
}

fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let server_name = tls::server_name_from_addr(addr).unwrap();
    let tcp_stream = TcpStream::connect(addr).unwrap();
    let conn = rustls::ClientConnection::new(client_tls_config(), server_name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).unwrap();
    stream.sock.shutdown(std::net::Shutdown::Write).ok();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(&response).unwrap() {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => panic!("incomplete response: {response:?}"),
    };
    let status = parsed.code.unwrap();
    let body = String::from_utf8(response[header_len..].to_vec()).unwrap();
    (status, body)
}

#[test]
fn a_client_with_no_certificate_cannot_complete_the_tcp_handshake() {
    let addr = start_tcp_test_server("a_client_with_no_certificate_cannot_complete_the_tcp_handshake");
    let roots = {
        let mut roots = rustls::RootCertStore::empty();
        let cert = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(fixture("ca.crt")).unwrap()))
            .next().unwrap().unwrap();
        roots.add(cert).unwrap();
        roots
    };
    let bare_config = Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let server_name = tls::server_name_from_addr(&addr).unwrap();
    let tcp_stream = TcpStream::connect(&addr).unwrap();
    let conn = rustls::ClientConnection::new(bare_config, server_name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
    let result = stream.write_all(b"GET /jails HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    assert!(result.is_err(), "expected the handshake to fail with no client certificate presented");
}

#[test]
fn a_client_with_a_wrong_ca_certificate_cannot_complete_the_tcp_handshake() {
    let addr = start_tcp_test_server("a_client_with_a_wrong_ca_certificate_cannot_complete_the_tcp_handshake");
    let wrong_config = Arc::new(
        tls::load_client_config(&fixture("wrong-ca-node.crt"), &fixture("wrong-ca-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    let server_name = tls::server_name_from_addr(&addr).unwrap();
    let tcp_stream = TcpStream::connect(&addr).unwrap();
    let conn = rustls::ClientConnection::new(wrong_config, server_name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
    let result = stream.write_all(b"GET /jails HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    assert!(result.is_err(), "expected the handshake to fail for a wrong-CA client certificate");
}
```

Remove `const TEST_TOKEN`, `put_over_tcp_without_auth_header_returns_401`, `get_jails_over_tcp_with_wrong_auth_token_returns_401`, and `send_request_tcp_raw`. Re-point the three pre-existing TCP tests (`put_valid_spec_over_tcp_returns_200_and_provisions_the_jail`, `get_jails_over_tcp_lists_all_applied_jails`, `delete_over_tcp_removes_a_provisioned_jail`) to the new `start_tcp_test_server`/`send_request_tcp` (same names, new TLS-backed bodies above) — their own assertions are unchanged.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd --lib http::`
Expected: compile errors (`run_tcp` still takes `token: Arc<String>`, `route_authenticated` still exists and is what the TCP tests need removed).

- [ ] **Step 3: Write the implementation**

In `keel-agentd/src/http.rs`:

1. Imports: replace `use crate::auth;` with `use crate::tls;`; add `use rustls::{ServerConfig, ServerConnection, StreamOwned};`.
2. Type alias: `type TlsStream = StreamOwned<ServerConnection, TcpStream>;`
3. `ParsedRequest` drops `auth_header`:

```rust
struct ParsedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}
```

4. `read_request` (Unix, currently ~lines 61-114) **loses only the `auth_header` capture and the fifth tuple element** — otherwise completely unchanged:

```rust
fn read_request(stream: &mut UnixStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length) = loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let content_length = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_MESSAGE_BYTES {
                    return Ok(None);
                }
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Ok(None),
        }
    };

    let total_len = header_len + content_length;
    if total_len > MAX_MESSAGE_BYTES {
        return Ok(None);
    }
    while buf.len() < total_len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_len..total_len].to_vec();
    Ok(Some(ParsedRequest { method, path, body }))
}
```

5. `read_request_tcp` becomes `read_request_tls`, operating on `&mut TlsStream` instead of `&mut TcpStream`, with the identical body-shape change:

```rust
fn read_request_tls(stream: &mut TlsStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length) = loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let content_length = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_MESSAGE_BYTES {
                    return Ok(None);
                }
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Ok(None),
        }
    };

    let total_len = header_len + content_length;
    if total_len > MAX_MESSAGE_BYTES {
        return Ok(None);
    }
    while buf.len() < total_len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_len..total_len].to_vec();
    Ok(Some(ParsedRequest { method, path, body }))
}
```

6. `run`/`handle_connection` (Unix) are **completely untouched** (same function bodies as before this whole milestone, since `ParsedRequest`'s shrink and `read_request`'s trimmed tuple are the only changes and both apply identically regardless of which listener uses them). `write_response` (Unix) is also untouched.
7. `run_tcp`/`handle_connection_tcp`/`write_response_tcp` are renamed `run_tls`/`handle_connection_tls`/`write_response_tls`, retyped to `TlsStream`, and `handle_connection_tls` calls the single shared `route()` (no more `route_authenticated`):

```rust
pub fn run_tls(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<ServerConfig>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = Arc::clone(&tls_config);
        thread::spawn(move || {
            let Ok(conn) = ServerConnection::new(tls_config) else { return };
            let mut tls_stream = TlsStream::new(conn, stream);
            if handle_connection_tls(&mut tls_stream, &commands).is_err() {
                eprintln!("keel-agentd: TLS handshake or request handling failed for a connection");
            }
        });
    }
}

fn handle_connection_tls(stream: &mut TlsStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request_tls(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response_tls(stream, status, &body)
}

fn write_response_tls(stream: &mut TlsStream, status: u16, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
```

8. Delete `route_authenticated` entirely. `route()` (currently ~line 205) is otherwise **completely unchanged** — it never had the auth check in the first place (that lived only in `route_authenticated`), so nothing about its own body needs editing.

Note the plan renames `run_tcp`→`run_tls` and its siblings, a naming call this task makes since "tcp" no longer describes what makes this path distinct (it's now "the TLS path" vs. "the Unix path", not "the TCP path" vs. "the Unix path" — the transport is still TCP underneath, but TLS is the meaningful distinction from here on). This is a rename, not a behavior change, and Task 11 updates `main.rs`'s one call site accordingly.

- [ ] **Step 4: Run tests to verify they pass, and prove the Unix socket path is untouched**

Run: `cargo test -p keel-agentd --lib http::`
Expected: all tests pass, including the existing Unix-socket tests with **zero changes to their source** — confirming the Non-Goal that the Unix socket is unaffected.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/http.rs
git commit -m "Wrap keel-agentd's TCP listener in TLS, collapse route/route_authenticated back to one route()"
```

---

### Task 10: Delete `keel-agentd::auth`; `registration.rs` outbound TLS

**Files:**
- Delete: `keel-agentd/src/auth.rs`
- Modify: `keel-agentd/src/lib.rs` (remove `pub mod auth;`)
- Modify: `keel-agentd/src/registration.rs`

**Interfaces:**
- Consumes: `keel_agentd::tls::{load_client_config, server_name_from_addr}` (Task 4), `keel_controlplane::http::run(listener, commands, tls_config, client_config)` (Task 7, used by this file's own tests).
- Produces: `spawn(node_id: String, advertise_addr: String, control_plane_addr: String, heartbeat_interval: Duration, capacity_cpu: f64, capacity_memory: u64, client_config: Arc<rustls::ClientConfig>, commands: Sender<crate::worker::Command>) -> JoinHandle<()>` (`token: String` replaced by `client_config: Arc<rustls::ClientConfig>` as the 7th parameter).

- [ ] **Step 1: Delete the module**

```bash
git rm keel-agentd/src/auth.rs
```

Remove `pub mod auth;` from `keel-agentd/src/lib.rs:1`.

- [ ] **Step 2: Write the failing test**

Update `keel-agentd/src/registration.rs`'s test module: replace `start_test_control_plane(token)` with a TLS-backed version, update the three existing tests' `spawn(...)` calls, and replace `registration_with_a_mismatched_token_never_registers` with a wrong-CA-certificate equivalent:

```rust
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
}

fn start_test_control_plane() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
    let tls_config = std::sync::Arc::new(
        keel_agentd::tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    let client_config = std::sync::Arc::new(
        keel_agentd::tls::load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || keel_controlplane::http::run(listener, commands, tls_config, client_config));
    addr
}

fn node_client_config() -> std::sync::Arc<rustls::ClientConfig> {
    std::sync::Arc::new(
        crate::tls::load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    )
}

fn wrong_ca_client_config() -> std::sync::Arc<rustls::ClientConfig> {
    std::sync::Arc::new(
        crate::tls::load_client_config(&fixture("wrong-ca-node.crt"), &fixture("wrong-ca-node.key"), &fixture("ca.crt"))
            .unwrap(),
    )
}

fn get_nodes(control_plane_addr: &str) -> String {
    let server_name = crate::tls::server_name_from_addr(control_plane_addr).unwrap();
    let tcp_stream = TcpStream::connect(control_plane_addr).unwrap();
    let conn = rustls::ClientConnection::new(node_client_config(), server_name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
    stream
        .write_all(b"GET /nodes HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
        .unwrap();
    stream.sock.shutdown(std::net::Shutdown::Write).ok();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).to_string()
}

#[test]
fn registers_and_then_keeps_heartbeating() {
    let control_plane_addr = start_test_control_plane();
    let (_worker_handle, commands) = crate::worker::spawn(
        crate::Reconciler::new(
            keel_jail::FakeJailRuntime::new(),
            keel_zfs::FakeZfsManager::new(),
            keel_net::FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join("keel-agentd-registration-test-registers_and_then_keeps_heartbeating"),
        )
        .unwrap(),
    );
    let _handle = spawn(
        "node-1".to_string(),
        "10.0.0.1".to_string(),
        control_plane_addr.clone(),
        Duration::from_millis(50),
        4.0,
        8 * 1024 * 1024 * 1024,
        node_client_config(),
        commands,
    );

    thread::sleep(Duration::from_millis(200));
    let body = get_nodes(&control_plane_addr);
    assert!(body.contains("node-1"), "expected node-1 to have registered, got: {body}");
    assert!(body.contains("Alive"), "expected node-1 to be Alive, got: {body}");
    assert!(body.contains("capacity_cpu: 4"), "expected reported capacity in body: {body}");
}

#[test]
fn heartbeats_report_the_reconcilers_committed_resources() {
    let control_plane_addr = start_test_control_plane();
    let zfs = keel_zfs::FakeZfsManager::new();
    zfs.seed_dataset("zroot/keel/base/14.2-web");
    let reconciler = crate::Reconciler::new(
        keel_jail::FakeJailRuntime::new(),
        zfs,
        keel_net::FakeNetManager::new(),
        "zroot".to_string(),
        std::env::temp_dir().join("keel-agentd-registration-test-heartbeats_report_the_reconcilers_committed_resources"),
    )
    .unwrap();
    let (_worker_handle, commands) = crate::worker::spawn(reconciler);

    let (apply_tx, apply_rx) = mpsc::channel();
    commands
        .send(crate::worker::Command::Apply(
            keel_spec::JailSpec {
                api_version: "keel/v1".to_string(),
                kind: "Jail".to_string(),
                metadata: keel_spec::Metadata { name: "web-1".to_string() },
                spec: keel_spec::Spec {
                    image: "base/14.2-web".to_string(),
                    command: vec!["/usr/local/bin/myapp".to_string()],
                    network: keel_spec::NetworkSpec {
                        vnet: true,
                        bridge: "keel0".to_string(),
                        address: "10.0.0.5/24".to_string(),
                    },
                    resources: keel_spec::ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                    restart_policy: keel_spec::RestartPolicy::Always,
                },
            },
            apply_tx,
        ))
        .unwrap();
    apply_rx.recv().unwrap().unwrap();

    let control_plane_addr_clone = control_plane_addr.clone();
    let _handle = spawn(
        "node-1".to_string(),
        "10.0.0.1".to_string(),
        control_plane_addr_clone,
        Duration::from_millis(50),
        4.0,
        8 * 1024 * 1024 * 1024,
        node_client_config(),
        commands,
    );

    thread::sleep(Duration::from_millis(200));
    let body = get_nodes(&control_plane_addr);
    assert!(body.contains("committed_cpu: 2"), "expected committed_cpu from the applied jail, got: {body}");
    assert!(body.contains("committed_memory: 536870912"), "expected committed_memory from the applied jail, got: {body}");
}

#[test]
fn registration_with_a_wrong_ca_certificate_never_registers() {
    let control_plane_addr = start_test_control_plane();
    let (_worker_handle, commands) = crate::worker::spawn(
        crate::Reconciler::new(
            keel_jail::FakeJailRuntime::new(),
            keel_zfs::FakeZfsManager::new(),
            keel_net::FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join("keel-agentd-registration-test-registration_with_a_wrong_ca_certificate_never_registers"),
        )
        .unwrap(),
    );
    let _handle = spawn(
        "node-1".to_string(),
        "10.0.0.1".to_string(),
        control_plane_addr.clone(),
        Duration::from_millis(50),
        4.0,
        8 * 1024 * 1024 * 1024,
        wrong_ca_client_config(),
        commands,
    );

    thread::sleep(Duration::from_millis(200));
    let body = get_nodes(&control_plane_addr);
    assert!(!body.contains("node-1"), "expected node-1 to never register with a wrong-CA certificate, got: {body}");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-agentd --lib registration::`
Expected: compile error (`spawn` still takes `token: String`, not `client_config`).

- [ ] **Step 4: Write the implementation**

In `keel-agentd/src/registration.rs`:

```rust
use crate::tls;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

// each parameter is independently needed by the registration loop; bundling into a struct would be over-engineering for this single call site
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    client_config: Arc<rustls::ClientConfig>,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr, capacity_cpu, capacity_memory, &client_config) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id, &commands, &client_config) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("keel-agentd: heartbeat failed: {e}");
                        registered = false;
                    }
                }
            }
            thread::sleep(heartbeat_interval);
        }
    })
}

fn register_once(
    control_plane_addr: &str,
    node_id: &str,
    advertise_addr: &str,
    capacity_cpu: f64,
    capacity_memory: u64,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let body = format!(
        "id: {node_id}\naddr: {advertise_addr}\ncapacity_cpu: {capacity_cpu}\ncapacity_memory: {capacity_memory}\n"
    );
    send_request(control_plane_addr, "POST", "/nodes/register", &body, client_config)
}

fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, client_config)
}

fn send_request(addr: &str, method: &str, path: &str, body: &str, client_config: &Arc<rustls::ClientConfig>) -> Result<(), String> {
    let server_name = tls::server_name_from_addr(addr)?;
    let tcp_stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let conn = rustls::ClientConnection::new(Arc::clone(client_config), server_name).map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);

    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.sock.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&response).map_err(|e| format!("malformed response: {e}"))? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err("incomplete response from control plane".to_string()),
    };
    let status = parsed.code.unwrap_or(0);
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("control plane returned status {status}"))
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-agentd --lib registration::`
Expected: all tests pass, including the new wrong-CA test.

- [ ] **Step 6: Commit**

```bash
git add -A keel-agentd/
git commit -m "Delete keel-agentd's auth module; wrap registration/heartbeat calls in TLS"
```

---

### Task 11: `keel-agentd/main.rs` — flag/wiring replacement

**Files:**
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_agentd::tls::{load_server_config, load_client_config}` (Task 4), `keel_agentd::registration::spawn(..., client_config: Arc<ClientConfig>, commands)` (Task 10), `keel_agentd::http::{run, run_tls}` (Task 9's `run` unchanged, `run_tls` new name).

- [ ] **Step 1: Write the failing tests**

Replace `keel-agentd/src/main.rs`'s test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn defaults_have_no_control_plane_configuration() {
        let config = parse_args_from(args(&["--pool", "zroot"]));
        assert_eq!(config.node_id, None);
        assert_eq!(config.control_plane_addr, None);
        assert_eq!(config.advertise_addr, None);
        assert_eq!(config.tls_ca_file, None);
        assert_eq!(config.tls_cert_file, None);
        assert_eq!(config.tls_key_file, None);
    }

    #[test]
    fn parses_all_six_control_plane_flags() {
        let config = parse_args_from(args(&[
            "--node-id", "node-2",
            "--control-plane-addr", "192.168.64.2:7620",
            "--advertise-addr", "192.168.64.2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
            "--tls-key-file", "/etc/keel/node-2.key",
        ]));
        assert_eq!(config.node_id, Some("node-2".to_string()));
        assert_eq!(config.control_plane_addr, Some("192.168.64.2:7620".to_string()));
        assert_eq!(config.advertise_addr, Some("192.168.64.2".to_string()));
        assert_eq!(config.tls_ca_file, Some(PathBuf::from("/etc/keel/ca.crt")));
        assert_eq!(config.tls_cert_file, Some(PathBuf::from("/etc/keel/node-2.crt")));
        assert_eq!(config.tls_key_file, Some(PathBuf::from("/etc/keel/node-2.key")));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, and --tls-key-file are all required when --control-plane-addr is set")]
    fn control_plane_addr_without_node_id_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--advertise-addr", "192.168.64.2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
            "--tls-key-file", "/etc/keel/node-2.key",
        ]));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, and --tls-key-file are all required when --control-plane-addr is set")]
    fn control_plane_addr_without_tls_key_file_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--node-id", "node-2",
            "--advertise-addr", "192.168.64.2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
        ]));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd --bin keel-agentd`
Expected: compile error — no `tls_ca_file`/`tls_cert_file`/`tls_key_file` fields yet.

- [ ] **Step 3: Write the implementation**

In `keel-agentd/src/main.rs`:

1. `Config` and its `Default` replace `auth_token_file: Option<PathBuf>` with three fields: `tls_ca_file`, `tls_cert_file`, `tls_key_file` (all `Option<PathBuf>`, default `None`).
2. `parse_args_from`'s match replaces `"--auth-token-file" => ...` with three arms (`"--tls-ca-file"`, `"--tls-cert-file"`, `"--tls-key-file"`), and the trailing pairing check becomes:

```rust
if config.control_plane_addr.is_some()
    && (config.node_id.is_none()
        || config.advertise_addr.is_none()
        || config.tls_ca_file.is_none()
        || config.tls_cert_file.is_none()
        || config.tls_key_file.is_none())
{
    panic!(
        "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, and --tls-key-file are all required when --control-plane-addr is set"
    );
}
```

3. `main()`'s control-plane block replaces token loading with TLS config loading, and calls `http::run_tls` (renamed from `run_tcp` in Task 9) instead of `run_tcp`:

```rust
if let (Some(node_id), Some(control_plane_addr), Some(advertise_addr), Some(ca_file), Some(cert_file), Some(key_file)) = (
    config.node_id.clone(),
    config.control_plane_addr.clone(),
    config.advertise_addr.clone(),
    config.tls_ca_file.clone(),
    config.tls_cert_file.clone(),
    config.tls_key_file.clone(),
) {
    let (capacity_cpu, capacity_memory) = keel_agentd::capacity::detect()
        .unwrap_or_else(|e| panic!("failed to detect node capacity via sysctl: {e}"));
    let tls_server_config = keel_agentd::tls::load_server_config(&cert_file, &key_file, &ca_file)
        .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
    let tls_client_config = keel_agentd::tls::load_client_config(&cert_file, &key_file, &ca_file)
        .unwrap_or_else(|e| panic!("failed to load TLS client config: {e}"));
    eprintln!(
        "keel-agentd: registering with control plane at {control_plane_addr} as node '{node_id}' ({advertise_addr}), capacity {capacity_cpu} cores / {capacity_memory} bytes"
    );
    keel_agentd::registration::spawn(
        node_id,
        advertise_addr.clone(),
        control_plane_addr,
        Duration::from_secs(5),
        capacity_cpu,
        capacity_memory,
        std::sync::Arc::new(tls_client_config),
        commands.clone(),
    );

    eprintln!("keel-agentd: serving jails API over TLS on {advertise_addr}");
    let tcp_listener = TcpListener::bind(&advertise_addr)
        .unwrap_or_else(|e| panic!("failed to bind jails-API TCP listener on {advertise_addr}: {e}"));
    let tcp_commands = commands.clone();
    thread::spawn(move || keel_agentd::http::run_tls(tcp_listener, tcp_commands, std::sync::Arc::new(tls_server_config)));
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd --bin keel-agentd`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/main.rs
git commit -m "Require TLS cert/key/CA flags on keel-agentd's control-plane path"
```

---

### Task 12: `keelctl` — flag/wiring replacement

**Files:**
- Modify: `keelctl/src/main.rs`

**Interfaces:**
- Produces: `resolve_target(socket: PathBuf, control_plane_addr: Option<String>, node: Option<String>, tls_ca_file: Option<String>, tls_cert_file: Option<String>, tls_key_file: Option<String>) -> Result<Target, String>`, `Target::ControlPlane { addr: String, node: Option<String>, tls_ca_file: PathBuf, tls_cert_file: PathBuf, tls_key_file: PathBuf }` (holds the three file **paths**, not a built `rustls::ClientConfig` — `ClientConfig` has no `PartialEq` impl, and `Target` needs one for its existing unit tests; the config is built lazily, once, at the point of use in `send_request_tcp`).

- [ ] **Step 1: Write the failing tests**

Replace `keelctl/src/main.rs`'s `Target` enum and test module:

```rust
#[derive(Debug, PartialEq)]
enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: Option<String>, tls_ca_file: PathBuf, tls_cert_file: PathBuf, tls_key_file: PathBuf },
}
```

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_control_plane_flags_yields_socket_target() {
        let target = resolve_target(PathBuf::from("/var/run/keel-agentd.sock"), None, None, None, None, None).unwrap();
        assert_eq!(target, Target::Socket(PathBuf::from("/var/run/keel-agentd.sock")));
    }

    #[test]
    fn node_without_control_plane_addr_is_an_error() {
        let err = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            None,
            Some("node-1".to_string()),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, "--node requires --control-plane-addr");
    }

    #[test]
    fn control_plane_addr_without_tls_flags_is_an_error() {
        let err = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            Some("10.0.0.1:7620".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, "--tls-ca-file, --tls-cert-file, and --tls-key-file are all required with --control-plane-addr");
    }

    #[test]
    fn control_plane_addr_with_all_tls_flags_builds_a_control_plane_target() {
        let target = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            Some("10.0.0.1:7620".to_string()),
            Some("node-1".to_string()),
            Some("/etc/keel/ca.crt".to_string()),
            Some("/etc/keel/alice.crt".to_string()),
            Some("/etc/keel/alice.key".to_string()),
        )
        .unwrap();
        assert_eq!(
            target,
            Target::ControlPlane {
                addr: "10.0.0.1:7620".to_string(),
                node: Some("node-1".to_string()),
                tls_ca_file: PathBuf::from("/etc/keel/ca.crt"),
                tls_cert_file: PathBuf::from("/etc/keel/alice.crt"),
                tls_key_file: PathBuf::from("/etc/keel/alice.key"),
            }
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keelctl`
Expected: compile error — `resolve_target` still has the old 4-parameter signature, `Target::ControlPlane` still has a `token` field.

- [ ] **Step 3: Write the implementation**

In `keelctl/src/main.rs`:

1. New `resolve_target`:

```rust
fn resolve_target(
    socket: PathBuf,
    control_plane_addr: Option<String>,
    node: Option<String>,
    tls_ca_file: Option<String>,
    tls_cert_file: Option<String>,
    tls_key_file: Option<String>,
) -> Result<Target, String> {
    match (control_plane_addr, node, tls_ca_file, tls_cert_file, tls_key_file) {
        (Some(addr), node, Some(ca), Some(cert), Some(key)) => Ok(Target::ControlPlane {
            addr,
            node,
            tls_ca_file: PathBuf::from(ca),
            tls_cert_file: PathBuf::from(cert),
            tls_key_file: PathBuf::from(key),
        }),
        (Some(_), _, _, _, _) => {
            Err("--tls-ca-file, --tls-cert-file, and --tls-key-file are all required with --control-plane-addr".to_string())
        }
        (None, Some(_), _, _, _) => Err("--node requires --control-plane-addr".to_string()),
        (None, None, _, _, _) => Ok(Target::Socket(socket)),
    }
}
```

2. `main()`'s target-resolution block gains the three new flags and passes them through:

```rust
fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let control_plane_addr = extract_flag(&mut args, "--control-plane-addr");
    let node = extract_flag(&mut args, "--node");
    let tls_ca_file = extract_flag(&mut args, "--tls-ca-file");
    let tls_cert_file = extract_flag(&mut args, "--tls-cert-file");
    let tls_key_file = extract_flag(&mut args, "--tls-key-file");

    let target = match resolve_target(socket, control_plane_addr, node, tls_ca_file, tls_cert_file, tls_key_file) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::FAILURE;
        }
    };

    // ... existing `match args.split_first() { ... }` dispatch, unchanged ...
}
```

3. `jails_path` needs **no change** — its `Target::ControlPlane { node: Some(node), .. }`/`{ node: None, .. }` patterns already absorb the three renamed fields via `..`.
4. `dispatch` and `send_request_tcp` build the TLS client config lazily, once per call, from the paths in `Target`:

```rust
fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, tls_ca_file, tls_cert_file, tls_key_file, .. } => {
            send_request_tcp(addr, method, path, body, tls_ca_file, tls_cert_file, tls_key_file)
        }
    }
}

fn send_request_tcp(
    addr: &str,
    method: &str,
    path: &str,
    body: &str,
    tls_ca_file: &PathBuf,
    tls_cert_file: &PathBuf,
    tls_key_file: &PathBuf,
) -> Result<String, String> {
    let client_config = std::sync::Arc::new(
        tls::load_client_config(tls_cert_file, tls_key_file, tls_ca_file)
            .map_err(|e| format!("failed to load TLS client config: {e}"))?,
    );
    let server_name = tls::server_name_from_addr(addr).map_err(|e| e.to_string())?;
    let tcp_stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let conn = rustls::ClientConnection::new(client_config, server_name).map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);

    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.sock.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;
    parse_response(&response)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keelctl`
Expected: the 4 unit tests pass. (`cargo test -p keelctl --test cli` will not yet fully pass — Task 13 updates that file.)

- [ ] **Step 5: Commit**

```bash
git add keelctl/src/main.rs
git commit -m "Require TLS cert/key/CA flags on keelctl's control-plane-routed mode"
```

---

### Task 13: `keelctl/tests/cli.rs` — TLS-backed integration helpers

**Files:**
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `keel_agentd::http::run` (unchanged), `keel_agentd::http::run_tls` (Task 9's rename), `keel_controlplane::http::run(listener, commands, tls_config, client_config)` (Task 7).

- [ ] **Step 1: Update the TLS-affected helpers**

Replace `TEST_TOKEN`/`test_token_file` with fixture paths, and update `start_test_agentd_tcp`, `start_test_control_plane_with_node`, `run_keelctl_routed`, `run_keelctl_scheduled`:

```rust
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
}

fn start_test_agentd_tcp(name: &str) -> String {
    let state_dir = std::env::temp_dir().join(format!("keelctl-routed-test-state-{name}"));
    let _ = std::fs::remove_dir_all(&state_dir);
    let zfs = FakeZfsManager::new();
    zfs.seed_dataset("zroot/keel/base/14.2-web");
    let reconciler =
        Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), "zroot".to_string(), state_dir)
            .unwrap();
    let (_worker_handle, commands) = worker::spawn(reconciler);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let tls_config = std::sync::Arc::new(
        keel_agentd::tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || keel_agentd::http::run_tls(listener, commands, tls_config));
    addr
}

fn start_test_control_plane_with_node(node_id: &str, node_addr: &str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) =
        keel_controlplane::worker::spawn(keel_controlplane::Registry::new(), keel_controlplane::Placements::new());

    let (reg_tx, reg_rx) = std::sync::mpsc::channel();
    commands
        .send(keel_controlplane::worker::Command::Register(
            node_id.to_string(),
            node_addr.to_string(),
            4.0,
            8 * 1024 * 1024 * 1024,
            reg_tx,
        ))
        .unwrap();
    reg_rx.recv().unwrap();

    let tls_config = std::sync::Arc::new(
        keel_controlplane::tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    let client_config = std::sync::Arc::new(
        keel_controlplane::tls::load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
            .unwrap(),
    );
    thread::spawn(move || keel_controlplane::http::run(listener, commands, tls_config, client_config));
    addr
}

fn run_keelctl_routed(control_plane_addr: &str, node: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(args)
        .arg("--control-plane-addr")
        .arg(control_plane_addr)
        .arg("--node")
        .arg(node)
        .arg("--tls-ca-file")
        .arg(fixture("ca.crt"))
        .arg("--tls-cert-file")
        .arg(fixture("fixture-client.crt"))
        .arg("--tls-key-file")
        .arg(fixture("fixture-client.key"))
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn run_keelctl_scheduled(control_plane_addr: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(args)
        .arg("--control-plane-addr")
        .arg(control_plane_addr)
        .arg("--tls-ca-file")
        .arg(fixture("ca.crt"))
        .arg("--tls-cert-file")
        .arg(fixture("fixture-client.crt"))
        .arg("--tls-key-file")
        .arg(fixture("fixture-client.key"))
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}
```

`start_test_server` (Unix-socket helper, calls `keel_agentd::http::run` unchanged) and `run_keelctl` (Unix-socket CLI helper) are untouched. `node_without_control_plane_addr_is_a_usage_error`'s bare `Command::new(...).args(["get"]).arg("--node").arg("node-1")` invocation is untouched too — it never sets `--control-plane-addr`, so `resolve_target`'s `(None, Some(_), _, _, _)` arm still fires first and the assertion on `"--node requires --control-plane-addr"` still holds.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p keelctl`
Expected: all unit tests (Task 12) plus all integration tests in `tests/cli.rs` pass — this is the first point every part of `keelctl` is checked together.

- [ ] **Step 3: Commit**

```bash
git add keelctl/tests/cli.rs
git commit -m "Update keelctl's integration tests for TLS cert/key/CA flags"
```

---

### Task 14: Full workspace test run + VM verification + README/website

**Files:** README.md, site/journey.html (verification and docs only; no source changes).

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: every test across all crates passes — this is the first point all thirteen prior tasks' changes are checked together.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: 0 errors; only the pre-existing, unrelated warnings already present before this milestone (28x `assert_eq!` literal bool in `keel-zfs/src/fake.rs`, `or_insert_with` in `keel-agentd/src/reconciler.rs`, large-enum-variant in `keel-agentd/src/worker.rs`). No new warnings attributable to this milestone's code.

- [ ] **Step 3: Generate the real cluster CA and identities**

On the operator's machine (or one of the VMs):

```bash
mkdir -p /tmp/keel-cluster-certs
GEN_CERTS_OUT_DIR=/tmp/keel-cluster-certs ./scripts/gen-certs.sh init
GEN_CERTS_OUT_DIR=/tmp/keel-cluster-certs ./scripts/gen-certs.sh node controlplane 192.168.64.2
GEN_CERTS_OUT_DIR=/tmp/keel-cluster-certs ./scripts/gen-certs.sh node node-2 192.168.64.2
GEN_CERTS_OUT_DIR=/tmp/keel-cluster-certs ./scripts/gen-certs.sh node node-4 192.168.64.4
GEN_CERTS_OUT_DIR=/tmp/keel-cluster-certs ./scripts/gen-certs.sh node node-5 192.168.64.5
GEN_CERTS_OUT_DIR=/tmp/keel-cluster-certs ./scripts/gen-certs.sh client operator
```

(`node-2` and `controlplane` share the same host, `.2`, but need distinct identities: `keel-controlplane` and `node-2`'s `keel-agentd` are two different processes on that box, each with their own certificate.)

- [ ] **Step 4: Distribute and restart the cluster with TLS**

Copy `ca.crt` to all three VMs and the client machine. Copy each node's own `<name>.crt`/`<name>.key` to its host only (`controlplane.crt`/`.key` and `node-2.crt`/`.key` both go to `.2`; `node-4.crt`/`.key` to `.4`; `node-5.crt`/`.key` to `.5`); copy `operator.crt`/`.key` to the `keelctl` client machine.

On `.2`: `keel-controlplane --addr 0.0.0.0:7620 --tls-ca-file /tmp/ca.crt --tls-cert-file /tmp/controlplane.crt --tls-key-file /tmp/controlplane.key` and `keel-agentd --node-id node-2 --advertise-addr 192.168.64.2:7621 --control-plane-addr 192.168.64.2:7620 --tls-ca-file /tmp/ca.crt --tls-cert-file /tmp/node-2.crt --tls-key-file /tmp/node-2.key` (plus its existing pool/state-dir/socket flags). On `.4`/`.5`: `keel-agentd` with the matching `--node-id`/`--advertise-addr` and that node's own cert/key.

- [ ] **Step 5: Confirm normal operation is unaffected**

From the client: `keelctl apply -f web-1.yaml --control-plane-addr 192.168.64.2:7620 --tls-ca-file /tmp/ca.crt --tls-cert-file /tmp/operator.crt --tls-key-file /tmp/operator.key` (no `--node`, exercising the scheduler), then `keelctl get web-1 ...` and `keelctl delete web-1 ...` with the same flags.
Expected: identical behavior to Milestone 11's verification — apply lands on a node, get/delete succeed, now over mTLS.

- [ ] **Step 6: Confirm a client with no certificate or a wrong-CA certificate is rejected**

Attempt `keelctl get web-1 --control-plane-addr 192.168.64.2:7620` with no `--tls-*` flags at all (should fail locally as a usage error, never reaching the network). Generate a second, throwaway CA and a cert signed by it (`GEN_CERTS_OUT_DIR=/tmp/keel-wrong ./scripts/gen-certs.sh init && ... client rogue`), then attempt `keelctl get web-1 --control-plane-addr 192.168.64.2:7620 --tls-ca-file /tmp/ca.crt --tls-cert-file /tmp/keel-wrong/rogue.crt --tls-key-file /tmp/keel-wrong/rogue.key`.
Expected: the first fails with the `--tls-ca-file, --tls-cert-file, and --tls-key-file are all required` usage error; the second connects but the TLS handshake fails, surfacing as a connection error from `keelctl`, never a normal HTTP response.

- [ ] **Step 7: Confirm a node with a SAN-mismatched certificate fails to register**

On `.4`, replace `node-4.crt`/`.key` with `node-5`'s certificate (SAN=192.168.64.5, not `.4`) and restart `keel-agentd`.
Expected: `.4`'s registration attempt fails at the TLS layer (the control plane, dialing back or validating on connect, rejects a certificate whose SAN doesn't match the connecting/dialed address, depending on which leg surfaces it first); `.4` never appears `Alive` in an authenticated `GET /nodes` request from the operator's own valid certificate.

- [ ] **Step 8: Clean teardown**

Stop all `keel-controlplane`/`keel-agentd` processes on all three VMs; confirm no leftover jails (`jls`) and no lingering processes, matching every prior milestone's teardown discipline.

- [ ] **Step 9: Update the README and website**

Add Milestone 12 to the README's "The journey so far" and Roadmap sections (mark roadmap item 12 done, following Milestones 7-11's per-milestone write-up style), and add the matching entry to `site/journey.html`.

- [ ] **Step 10: Commit**

```bash
git add README.md site/journey.html
git commit -m "Document Milestone 12 completion: mutual TLS for the control plane"
```
