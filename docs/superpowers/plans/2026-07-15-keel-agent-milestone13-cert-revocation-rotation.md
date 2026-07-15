# Milestone 13: Certificate Revocation and Rotation Automation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add CRL-based certificate revocation (checked at the TLS handshake, in both directions) and a background reload thread in `keel-controlplane`/`keel-agentd` so a rotated certificate or a refreshed CRL takes effect with no restart, closing the two gaps Milestone 12 explicitly deferred.

**Architecture:** `scripts/gen-certs.sh` grows a real `openssl ca` certificate database so it can revoke a serial and regenerate a CRL; `rustls` 0.23's already-vendored `WebPkiClientVerifier`/`WebPkiServerVerifier` `with_crls(...)` support is wired into every TLS config builder in `keel-controlplane`, `keel-agentd`, and `keelctl`, symmetrically (listeners checking the caller's certificate, outbound callers checking the peer's server certificate); a new `ReloadingTls` type in the two long-running daemons' `tls.rs` polls the same four files on a background timer and hot-swaps the active configuration.

**Tech Stack:** Rust, `rustls` 0.23 (`ring` backend, already a dependency), `rustls-pemfile` 2 (already a dependency), `openssl` CLI (via `scripts/gen-certs.sh`), no new Rust dependency.

## Global Constraints

- No new Rust dependency. `rustls`'s `WebPkiClientVerifier`/`WebPkiServerVerifier::with_crls(...)` and `rustls_pemfile::crls()` are already vendored at the pinned versions (`rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }`, `rustls-pemfile = "2"`); do not add `rcgen`, `x509-parser`, or any CRL/cert-generation crate.
- Certificate and CRL *generation* stays entirely in `scripts/gen-certs.sh`, shelling out to `openssl`. No Rust code parses a CSR, issues a certificate, or builds a CRL from scratch.
- `keel-agentd`'s Unix socket path is completely unaffected: no TLS, no certificates, no CRL, `route()` keeps its existing unwrapped call from that path.
- Certificate validity stays at the existing 10-year default (100-year for test fixtures via `--days 36500`). Do not change `DEFAULT_DAYS`.
- The background reload interval is a fixed constant in each `main.rs` (30 seconds), not a CLI flag.
- Every existing test must keep passing; call sites of `load_server_config`/`load_client_config` across `keel-controlplane`, `keel-agentd`, and `keelctl` (including `keelctl/tests/cli.rs`) all gain a 4th `crl_path` argument in this plan — grep for `load_server_config\|load_client_config` after each task to confirm no stale call site was missed.
- Follow this project's existing conventions throughout: `eprintln!` for operational visibility (no logging framework), fail-fast `panic!`/`ExitCode::FAILURE` on startup config errors, hand-rolled HTTP parsing untouched, one worker thread per daemon untouched.

---

## File Structure

**Modify:**
- `scripts/gen-certs.sh` — CA database, safe rotation-on-reissue, new `revoke`/`crl` subcommands.
- `testdata/tls/` — add `ca-db/`, `crl.pem`, and a `revoked-node.crt`/`.key` fixture pair (existing fixtures untouched).
- `keel-controlplane/src/tls.rs` — CRL-aware `load_server_config`/`load_client_config`, new `ReloadingTls` type.
- `keel-controlplane/src/http.rs` — `run()` takes `Arc<ReloadingTls>`; new revoked-certificate tests (both directions); reload tests.
- `keel-controlplane/src/main.rs` — new `--tls-crl-file` flag; construct `ReloadingTls` instead of one-shot configs.
- `keel-agentd/src/tls.rs` — same shape as `keel-controlplane/src/tls.rs`.
- `keel-agentd/src/http.rs` — `run_tls()` takes `Arc<ReloadingTls>`; new revoked-certificate test.
- `keel-agentd/src/registration.rs` — `spawn()` takes `Arc<ReloadingTls>`, fetches a fresh client config every tick; new revoked-certificate test.
- `keel-agentd/src/main.rs` — new `--tls-crl-file` flag; construct `ReloadingTls`.
- `keelctl/src/tls.rs` — CRL-aware `load_client_config`.
- `keelctl/src/main.rs` — new `--tls-crl-file` flag threaded through `Target`/`resolve_target`/`dispatch`/`send_request_tcp`.
- `keelctl/tests/cli.rs` — call-site updates for the new `crl_path` argument and `--tls-crl-file` CLI flag.

No new files. `ReloadingTls` lives inside each crate's existing `tls.rs`, matching this project's established choice to keep `tls.rs` crate-local rather than extract a shared library crate.

---

## Task 1: `scripts/gen-certs.sh` — CA database, rotation, revoke, crl

**Files:**
- Modify: `scripts/gen-certs.sh`

**Interfaces:**
- Produces: `./scripts/gen-certs.sh init` (idempotent: generates the CA keypair only if missing, always ensures `$OUT_DIR/ca-db/` and an initial `$OUT_DIR/crl.pem` exist); `./scripts/gen-certs.sh node <name> <ip> [--days N]` / `client <name> [--days N]` (now rotate-safe: reissuing an existing name auto-revokes the old serial and refreshes `crl.pem`); `./scripts/gen-certs.sh revoke <name>`; `./scripts/gen-certs.sh crl`.

This task has been fully hand-tested against the real `openssl` binary on this machine (`openssl 3.6.3`) before being written into this plan: CA init, idempotent re-init, first issuance, rotation (reissue under an existing name), standalone `revoke`, and `crl` were all run end-to-end, and `openssl verify -crl_check` confirmed a rotated-away certificate is rejected as revoked while the current one verifies clean.

- [ ] **Step 1: Replace `scripts/gen-certs.sh` with the CA-database-backed version**

Replace the entire file with:

```sh
#!/bin/sh
#
# Generates the private CA and per-identity leaf certificates this
# project's mutual TLS uses, and manages revocation and rotation on top
# of a real openssl ca(1) certificate database (needed because openssl
# ca -gencrl is the only tool that can produce a CRL). Every leaf
# certificate is signed by one CA and gets both serverAuth and
# clientAuth extended key usage. Validity defaults to 10 years,
# overridable with a trailing --days N.
#
# Usage:
#   ./scripts/gen-certs.sh init
#   ./scripts/gen-certs.sh node <name> <ip-address> [--days N]
#   ./scripts/gen-certs.sh client <name> [--days N]
#   ./scripts/gen-certs.sh revoke <name>
#   ./scripts/gen-certs.sh crl
#
# Reissuing an existing node/client name rotates it: the new certificate
# is issued first, and only once that succeeds is the previous
# certificate under that name revoked and crl.pem regenerated, so a
# failed reissue never strands an identity with zero valid certificates.
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
[ -n "$cmd" ] || fail "usage: $0 <init|node|client|revoke|crl> ..."
shift

ca_config() {
    echo "$OUT_DIR/ca-db/openssl.cnf"
}

init_ca_db() {
    if [ -f "$OUT_DIR/ca-db/index.txt" ]; then
        return
    fi
    mkdir -p "$OUT_DIR/ca-db/newcerts"
    : > "$OUT_DIR/ca-db/index.txt"
    echo "unique_subject = no" > "$OUT_DIR/ca-db/index.txt.attr"
    echo 1000 > "$OUT_DIR/ca-db/serial"
    echo 1000 > "$OUT_DIR/ca-db/crlnumber"
    cat > "$(ca_config)" <<CNFEOF
[ ca ]
default_ca = CA_default

[ CA_default ]
database         = $OUT_DIR/ca-db/index.txt
serial           = $OUT_DIR/ca-db/serial
crlnumber        = $OUT_DIR/ca-db/crlnumber
new_certs_dir    = $OUT_DIR/ca-db/newcerts
certificate      = $OUT_DIR/ca.crt
private_key      = $OUT_DIR/ca.key
default_md       = sha256
default_days     = $DEFAULT_DAYS
default_crl_days = $DEFAULT_DAYS
policy           = policy_anything
email_in_dn      = no
unique_subject   = no
copy_extensions  = none

[ policy_anything ]
countryName            = optional
stateOrProvinceName    = optional
organizationName       = optional
organizationalUnitName = optional
commonName             = supplied
emailAddress           = optional
CNFEOF
    openssl ca -config "$(ca_config)" -gencrl -out "$OUT_DIR/crl.pem"
    echo "gen-certs: initialized CA database at $OUT_DIR/ca-db and wrote empty $OUT_DIR/crl.pem"
}

issue_leaf() {
    name="$1"
    days="$2"
    san_line="$3"

    tmp_key="$OUT_DIR/.$name.key.tmp.$$"
    tmp_crt="$OUT_DIR/.$name.crt.tmp.$$"
    tmp_csr=$(mktemp)
    ext_file=$(mktemp)
    trap 'rm -f "$tmp_key" "$tmp_crt" "$tmp_csr" "$ext_file"' EXIT

    openssl genrsa -out "$tmp_key" 4096
    openssl req -new -key "$tmp_key" -subj "/CN=$name" -out "$tmp_csr"

    if [ -n "$san_line" ]; then
        printf 'subjectAltName = %s\nextendedKeyUsage = serverAuth, clientAuth\n' "$san_line" > "$ext_file"
    else
        printf 'extendedKeyUsage = serverAuth, clientAuth\n' > "$ext_file"
    fi

    openssl ca -config "$(ca_config)" -in "$tmp_csr" -out "$tmp_crt" \
        -days "$days" -batch -extfile "$ext_file" -notext
    rm -f "$tmp_csr" "$ext_file"

    if [ -f "$OUT_DIR/$name.crt" ]; then
        mv "$OUT_DIR/$name.crt" "$OUT_DIR/$name.crt.previous"
        mv "$tmp_key" "$OUT_DIR/$name.key"
        mv "$tmp_crt" "$OUT_DIR/$name.crt"
        trap - EXIT
        openssl ca -config "$(ca_config)" -revoke "$OUT_DIR/$name.crt.previous"
        openssl ca -config "$(ca_config)" -gencrl -out "$OUT_DIR/crl.pem"
        rm -f "$OUT_DIR/$name.crt.previous"
        echo "gen-certs: rotated $name (previous certificate revoked, crl.pem refreshed)"
    else
        mv "$tmp_key" "$OUT_DIR/$name.key"
        mv "$tmp_crt" "$OUT_DIR/$name.crt"
        trap - EXIT
    fi
}

case "$cmd" in
    init)
        days="$DEFAULT_DAYS"
        if [ "${1:-}" = "--days" ]; then
            days="${2:-}"
            [ -n "$days" ] || fail "--days requires a value"
        fi
        if [ -f "$OUT_DIR/ca.key" ]; then
            echo "gen-certs: $OUT_DIR/ca.key already exists, reusing it"
        else
            openssl genrsa -out "$OUT_DIR/ca.key" 4096
            openssl req -x509 -new -nodes -key "$OUT_DIR/ca.key" -sha256 -days "$days" \
                -subj "/CN=keel-cluster-ca" -out "$OUT_DIR/ca.crt"
            echo "gen-certs: wrote $OUT_DIR/ca.crt and $OUT_DIR/ca.key"
        fi
        init_ca_db
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
    revoke)
        name="${1:-}"
        [ -n "$name" ] || fail "usage: $0 revoke <name>"
        [ -f "$OUT_DIR/$name.crt" ] || fail "no certificate found for '$name' at $OUT_DIR/$name.crt"
        openssl ca -config "$(ca_config)" -revoke "$OUT_DIR/$name.crt"
        openssl ca -config "$(ca_config)" -gencrl -out "$OUT_DIR/crl.pem"
        echo "gen-certs: revoked $name and refreshed $OUT_DIR/crl.pem"
        ;;
    crl)
        openssl ca -config "$(ca_config)" -gencrl -out "$OUT_DIR/crl.pem"
        echo "gen-certs: refreshed $OUT_DIR/crl.pem"
        ;;
    *)
        fail "unknown subcommand: $cmd (expected init|node|client|revoke|crl)"
        ;;
esac
```

- [ ] **Step 2: Verify the script end-to-end in a scratch directory**

Run:

```bash
chmod +x scripts/gen-certs.sh
export GEN_CERTS_OUT_DIR=/tmp/gen-certs-verify-$$
scripts/gen-certs.sh init
scripts/gen-certs.sh init          # must print "already exists, reusing it" and not fail
scripts/gen-certs.sh node node-4 192.168.64.4 --days 36500
scripts/gen-certs.sh client alice --days 36500
old_serial=$(openssl x509 -in "$GEN_CERTS_OUT_DIR/node-4.crt" -noout -serial)
scripts/gen-certs.sh node node-4 192.168.64.4 --days 36500   # rotate
new_serial=$(openssl x509 -in "$GEN_CERTS_OUT_DIR/node-4.crt" -noout -serial)
scripts/gen-certs.sh revoke alice
scripts/gen-certs.sh crl
cat "$GEN_CERTS_OUT_DIR/ca.crt" "$GEN_CERTS_OUT_DIR/crl.pem" > /tmp/chain-verify-$$.pem
openssl verify -crl_check -CAfile /tmp/chain-verify-$$.pem "$GEN_CERTS_OUT_DIR/node-4.crt"
echo "old_serial=$old_serial new_serial=$new_serial (must differ)"
rm -rf "$GEN_CERTS_OUT_DIR" /tmp/chain-verify-$$.pem
```

Expected: every command exits 0 except nothing should fail; `openssl verify` prints `...node-4.crt: OK`; `old_serial` and `new_serial` differ.

- [ ] **Step 3: Commit**

```bash
git add scripts/gen-certs.sh
git commit -m "gen-certs.sh: add CA database, safe rotation, revoke/crl subcommands"
```

---

## Task 2: Regenerate `testdata/tls/` fixtures for CRL testing

**Files:**
- Modify: `testdata/tls/` (adds `ca-db/`, `crl.pem`, `revoked-node.crt`, `revoked-node.key`; existing `ca.crt`, `ca.key`, `fixture-node.*`, `fixture-client.*`, `wrong-ca-node.*` are untouched)

**Interfaces:**
- Produces: `testdata/tls/crl.pem` (a CRL, signed by `testdata/tls/ca.crt`'s key, listing one revoked serial), `testdata/tls/revoked-node.crt`/`.key` (a certificate matching that revoked serial, dual-use server+client, ~100-year validity, dummy SAN `192.0.2.99`, an RFC 5737 TEST-NET-1 address reserved for documentation/example use).

- [ ] **Step 1: Bootstrap the CA database around the existing testdata CA, issue and revoke a fixture identity**

Run from the repo root, using Task 1's now-idempotent `init` to grow a `ca-db/` around the CA keypair that's already committed at `testdata/tls/ca.crt`/`ca.key` (this reuses that CA rather than regenerating it, so `fixture-node.*`, `fixture-client.*`, and `wrong-ca-node.*` remain valid and untouched):

```bash
export GEN_CERTS_OUT_DIR=testdata/tls
scripts/gen-certs.sh init
scripts/gen-certs.sh node revoked-node 192.0.2.99 --days 36500
scripts/gen-certs.sh revoke revoked-node
unset GEN_CERTS_OUT_DIR
```

Expected: `init` prints `testdata/tls/ca.key already exists, reusing it`; `node revoked-node ...` writes `testdata/tls/revoked-node.crt`/`.key`; `revoke revoked-node` writes a populated `testdata/tls/crl.pem`.

- [ ] **Step 2: Verify the new fixtures are internally consistent**

Run:

```bash
openssl verify -crl_check -CAfile <(cat testdata/tls/ca.crt testdata/tls/crl.pem) testdata/tls/revoked-node.crt
openssl verify -crl_check -CAfile <(cat testdata/tls/ca.crt testdata/tls/crl.pem) testdata/tls/fixture-node.crt
```

Expected: the first command prints `error 23 at 0 depth lookup: certificate revoked` and `verification failed` for `revoked-node.crt`; the second prints `testdata/tls/fixture-node.crt: OK`.

- [ ] **Step 3: Commit**

```bash
git add testdata/tls/
git commit -m "testdata/tls: add ca-db, crl.pem, and a revoked-node fixture identity"
```

---

## Task 3: `keel-controlplane` — CRL enforcement, both directions

**Files:**
- Modify: `keel-controlplane/src/tls.rs`
- Modify: `keel-controlplane/src/http.rs`
- Modify: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `testdata/tls/crl.pem`, `testdata/tls/revoked-node.crt`/`.key` (Task 2), `testdata/tls/wrong-ca-node.crt`/`.key` (existing).
- Produces: `load_server_config(cert_path: &Path, key_path: &Path, ca_path: &Path, crl_path: &Path) -> Result<rustls::ServerConfig, String>`, `load_client_config(cert_path: &Path, key_path: &Path, ca_path: &Path, crl_path: &Path) -> Result<rustls::ClientConfig, String>` — both now 4-argument, CRL-aware. `keel-controlplane`'s `--tls-crl-file` flag.

- [ ] **Step 1: Rewrite `keel-controlplane/src/tls.rs` with CRL support**

Replace the entire file with:

```rust
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, ServerName};
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

pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: &Path,
    crl_path: &Path,
) -> Result<rustls::ServerConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    let crls = load_crls(crl_path)?;
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .build()
        .map_err(|e| format!("failed to build client certificate verifier: {e}"))?;
    rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("failed to build TLS server config: {e}"))
}

pub fn load_client_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: &Path,
    crl_path: &Path,
) -> Result<rustls::ClientConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    let crls = load_crls(crl_path)?;
    let server_verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .build()
        .map_err(|e| format!("failed to build server certificate verifier: {e}"))?;
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(server_verifier)
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

fn load_crls(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, String> {
    let file = File::open(path).map_err(|e| format!("failed to open CRL file {}: {e}", path.display()))?;
    let crls: Vec<_> = rustls_pemfile::crls(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse CRL file {}: {e}", path.display()))?;
    if crls.is_empty() {
        return Err(format!("failed to find a PEM-encoded CRL in {}", path.display()));
    }
    Ok(crls)
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
        load_server_config(
            &fixture("fixture-node.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .expect("expected a valid server config");
    }

    #[test]
    fn load_server_config_fails_on_a_missing_cert_file() {
        let err = load_server_config(
            &fixture("does-not-exist.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .unwrap_err();
        assert!(err.contains("does-not-exist.crt"), "got: {err}");
    }

    #[test]
    fn load_server_config_fails_on_a_missing_crl_file() {
        let err = load_server_config(
            &fixture("fixture-node.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("does-not-exist-crl.pem"),
        )
        .unwrap_err();
        assert!(err.contains("does-not-exist-crl.pem"), "got: {err}");
    }

    #[test]
    fn load_server_config_fails_on_a_malformed_crl_file() {
        let bad_crl = std::env::temp_dir().join(format!("keel-controlplane-tls-test-bad-crl-{}", std::process::id()));
        std::fs::write(&bad_crl, "not a crl").unwrap();
        let err = load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &bad_crl)
            .unwrap_err();
        assert!(err.contains("failed to"), "got: {err}");
    }

    #[test]
    fn load_client_config_succeeds_with_valid_fixtures() {
        load_client_config(
            &fixture("fixture-client.crt"),
            &fixture("fixture-client.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .expect("expected a valid client config");
    }

    #[test]
    fn load_client_config_fails_on_a_malformed_ca_file() {
        let bad_ca = std::env::temp_dir().join(format!("keel-controlplane-tls-test-bad-ca-{}", std::process::id()));
        std::fs::write(&bad_ca, "not a certificate").unwrap();
        let err =
            load_client_config(&fixture("fixture-client.crt"), &fixture("fixture-client.key"), &bad_ca, &fixture("crl.pem"))
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

- [ ] **Step 2: Update `keel-controlplane/src/http.rs`'s call sites and add revoked-certificate tests**

In the `#[cfg(test)] mod tests` block, update every existing `tls::load_server_config(...)` / `tls::load_client_config(...)` call to pass `&fixture("crl.pem")` as the 4th argument. There are 6 call sites (lines 426, 436-441, 447, 647, 690, 748 in the pre-Task-3 file). For example:

```rust
// before
tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
// after
tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &fixture("crl.pem"))
```

Apply the same `, &fixture("crl.pem")` addition to the `tls::load_client_config(...)` calls at (former) lines 436-441 and 447.

Then add two new tests at the end of the `mod tests` block, mirroring the existing wrong-CA tests:

```rust
    #[test]
    fn a_client_with_a_revoked_certificate_cannot_complete_the_handshake() {
        let addr = start_test_server();
        let revoked_config = Arc::new(
            tls::load_client_config(
                &fixture("revoked-node.crt"),
                &fixture("revoked-node.key"),
                &fixture("ca.crt"),
                &fixture("crl.pem"),
            )
            .unwrap(),
        );
        let result = std::panic::catch_unwind(|| send_request_with(&addr, "GET", "/nodes", "", &revoked_config));
        assert!(
            result.is_err() || result.unwrap().0 != 200,
            "expected the handshake to fail for a revoked client certificate"
        );
    }

    #[test]
    fn forward_to_a_node_presenting_a_revoked_certificate_fails() {
        let cp_addr = start_test_server();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let node_addr = listener.local_addr().unwrap().to_string();
        let revoked_server_config = Arc::new(
            tls::load_server_config(
                &fixture("revoked-node.crt"),
                &fixture("revoked-node.key"),
                &fixture("ca.crt"),
                &fixture("crl.pem"),
            )
            .unwrap(),
        );
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&revoked_server_config)) else { continue };
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

- [ ] **Step 3: Update `keel-controlplane/src/main.rs`'s flag and call sites**

Replace:

```rust
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
```

with:

```rust
struct Config {
    addr: String,
    tls_ca_file: Option<PathBuf>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
    tls_crl_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:7620".to_string(),
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
            tls_crl_file: None,
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
            "--tls-crl-file" => config.tls_crl_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.tls_ca_file.is_none()
        || config.tls_cert_file.is_none()
        || config.tls_key_file.is_none()
        || config.tls_crl_file.is_none()
    {
        panic!("--tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required");
    }
    config
}
```

Replace:

```rust
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

with (this still uses one-shot loading for now; Task 6 replaces it with `ReloadingTls`):

```rust
fn main() {
    let config = parse_args();
    let ca_file = config.tls_ca_file.expect("validated as required in parse_args_from");
    let cert_file = config.tls_cert_file.expect("validated as required in parse_args_from");
    let key_file = config.tls_key_file.expect("validated as required in parse_args_from");
    let crl_file = config.tls_crl_file.expect("validated as required in parse_args_from");

    let tls_config = keel_controlplane::tls::load_server_config(&cert_file, &key_file, &ca_file, &crl_file)
        .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
    let client_config = keel_controlplane::tls::load_client_config(&cert_file, &key_file, &ca_file, &crl_file)
        .unwrap_or_else(|e| panic!("failed to load TLS client config: {e}"));

    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, Arc::new(tls_config), Arc::new(client_config));
}
```

Update the two existing tests in `main.rs`'s `mod tests` block:

```rust
    #[test]
    fn parses_the_tls_flags() {
        let config = parse_args_from(args(&[
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
        assert_eq!(config.tls_ca_file, Some(PathBuf::from("/etc/keel/ca.crt")));
        assert_eq!(config.tls_cert_file, Some(PathBuf::from("/etc/keel/controlplane.crt")));
        assert_eq!(config.tls_key_file, Some(PathBuf::from("/etc/keel/controlplane.key")));
        assert_eq!(config.tls_crl_file, Some(PathBuf::from("/etc/keel/crl.pem")));
    }

    #[test]
    #[should_panic(expected = "--tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required")]
    fn missing_any_tls_flag_panics() {
        parse_args_from(args(&["--tls-ca-file", "/etc/keel/ca.crt"]));
    }
```

- [ ] **Step 4: Run the crate's tests**

Run: `cargo test -p keel-controlplane`
Expected: all tests pass, including the two new revoked-certificate tests.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/tls.rs keel-controlplane/src/http.rs keel-controlplane/src/main.rs
git commit -m "keel-controlplane: enforce CRL revocation on every TLS connection, both directions"
```

---

## Task 4: `keel-agentd` — CRL enforcement, both directions

**Files:**
- Modify: `keel-agentd/src/tls.rs`
- Modify: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/registration.rs`
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: same fixtures as Task 3.
- Produces: `keel_agentd::tls::load_server_config`/`load_client_config`, now 4-argument, identical shape to Task 3. `keel-agentd`'s `--tls-crl-file` flag.

- [ ] **Step 1: Rewrite `keel-agentd/src/tls.rs` with CRL support**

Apply the exact same rewrite as Task 3 Step 1, with two differences: the test module's `bad_ca`/`bad_crl` temp filenames should say `keel-agentd` instead of `keel-controlplane` (matching the existing file's naming), and the `server_name_from_addr` test fixture keeps using `"192.168.64.2:7620"` (this crate's existing test value, unchanged). Concretely:

```rust
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Once};

static CRYPTO_PROVIDER_INIT: Once = Once::new();

pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: &Path,
    crl_path: &Path,
) -> Result<rustls::ServerConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    let crls = load_crls(crl_path)?;
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .build()
        .map_err(|e| format!("failed to build client certificate verifier: {e}"))?;
    rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("failed to build TLS server config: {e}"))
}

pub fn load_client_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: &Path,
    crl_path: &Path,
) -> Result<rustls::ClientConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    let crls = load_crls(crl_path)?;
    let server_verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .build()
        .map_err(|e| format!("failed to build server certificate verifier: {e}"))?;
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(server_verifier)
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

fn load_crls(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, String> {
    let file = File::open(path).map_err(|e| format!("failed to open CRL file {}: {e}", path.display()))?;
    let crls: Vec<_> = rustls_pemfile::crls(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse CRL file {}: {e}", path.display()))?;
    if crls.is_empty() {
        return Err(format!("failed to find a PEM-encoded CRL in {}", path.display()));
    }
    Ok(crls)
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
        load_server_config(
            &fixture("fixture-node.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .expect("expected a valid server config");
    }

    #[test]
    fn load_server_config_fails_on_a_missing_cert_file() {
        let err = load_server_config(
            &fixture("does-not-exist.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .unwrap_err();
        assert!(err.contains("does-not-exist.crt"), "got: {err}");
    }

    #[test]
    fn load_server_config_fails_on_a_missing_crl_file() {
        let err = load_server_config(
            &fixture("fixture-node.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("does-not-exist-crl.pem"),
        )
        .unwrap_err();
        assert!(err.contains("does-not-exist-crl.pem"), "got: {err}");
    }

    #[test]
    fn load_client_config_succeeds_with_valid_fixtures() {
        load_client_config(
            &fixture("fixture-node.crt"),
            &fixture("fixture-node.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .expect("expected a valid client config");
    }

    #[test]
    fn load_client_config_fails_on_a_malformed_ca_file() {
        let bad_ca = std::env::temp_dir().join(format!("keel-agentd-tls-test-bad-ca-{}", std::process::id()));
        std::fs::write(&bad_ca, "not a certificate").unwrap();
        let err =
            load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &bad_ca, &fixture("crl.pem"))
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

- [ ] **Step 2: Update `keel-agentd/src/http.rs`'s call sites and add a revoked-certificate test**

Add `&fixture("crl.pem")` as the 4th argument to the two existing `tls::load_server_config`/`tls::load_client_config` calls (former lines 468 and 477). Then add, at the end of `mod tests`:

```rust
    #[test]
    fn a_client_with_a_revoked_certificate_cannot_complete_the_tcp_handshake() {
        let addr = start_tcp_test_server("a_client_with_a_revoked_certificate_cannot_complete_the_tcp_handshake");
        let revoked_config = Arc::new(
            tls::load_client_config(
                &fixture("revoked-node.crt"),
                &fixture("revoked-node.key"),
                &fixture("ca.crt"),
                &fixture("crl.pem"),
            )
            .unwrap(),
        );
        let server_name = tls::server_name_from_addr(&addr).unwrap();
        let tcp_stream = TcpStream::connect(&addr).unwrap();
        let conn = rustls::ClientConnection::new(revoked_config, server_name).unwrap();
        let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
        let write_result = stream.write_all(b"GET /jails HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
        let mut response = Vec::new();
        let read_result = stream.read_to_end(&mut response);
        assert!(
            write_result.is_err() || read_result.is_err(),
            "expected the handshake to fail for a revoked client certificate"
        );
    }
```

- [ ] **Step 3: Update `keel-agentd/src/registration.rs`'s call sites and add a revoked-certificate test**

Add `&fixture("crl.pem")` as the 4th argument to all 5 existing `crate::tls::load_server_config`/`crate::tls::load_client_config` calls in `mod tests` (former lines 143, 147, 156, 163, 217). Then add, at the end of `mod tests`:

```rust
    fn start_fake_control_plane_with_revoked_cert() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server_config = std::sync::Arc::new(
            crate::tls::load_server_config(
                &fixture("revoked-node.crt"),
                &fixture("revoked-node.key"),
                &fixture("ca.crt"),
                &fixture("crl.pem"),
            )
            .unwrap(),
        );
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let Ok(conn) = rustls::ServerConnection::new(std::sync::Arc::clone(&server_config)) else { continue };
                let mut tls_stream = rustls::StreamOwned::new(conn, stream);
                let mut buf = [0u8; 4096];
                let _ = tls_stream.read(&mut buf);
            }
        });
        addr
    }

    #[test]
    fn send_request_to_a_peer_presenting_a_revoked_certificate_fails() {
        let addr = start_fake_control_plane_with_revoked_cert();
        let result = send_request(&addr, "GET", "/nodes", "", &node_client_config());
        assert!(result.is_err(), "expected a revoked peer certificate to fail the connection, got: {result:?}");
    }
```

- [ ] **Step 4: Update `keel-agentd/src/main.rs`'s flag and call sites**

Replace:

```rust
struct Config {
    pool: String,
    state_dir: PathBuf,
    socket: PathBuf,
    node_id: Option<String>,
    control_plane_addr: Option<String>,
    advertise_addr: Option<String>,
    tls_ca_file: Option<PathBuf>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pool: "zroot".to_string(),
            state_dir: PathBuf::from("/var/db/keel"),
            socket: PathBuf::from("/var/run/keel-agentd.sock"),
            node_id: None,
            control_plane_addr: None,
            advertise_addr: None,
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
            "--pool" => config.pool = value,
            "--state-dir" => config.state_dir = PathBuf::from(value),
            "--socket" => config.socket = PathBuf::from(value),
            "--node-id" => config.node_id = Some(value),
            "--control-plane-addr" => config.control_plane_addr = Some(value),
            "--advertise-addr" => config.advertise_addr = Some(value),
            "--tls-ca-file" => config.tls_ca_file = Some(PathBuf::from(value)),
            "--tls-cert-file" => config.tls_cert_file = Some(PathBuf::from(value)),
            "--tls-key-file" => config.tls_key_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
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
    config
}
```

with:

```rust
struct Config {
    pool: String,
    state_dir: PathBuf,
    socket: PathBuf,
    node_id: Option<String>,
    control_plane_addr: Option<String>,
    advertise_addr: Option<String>,
    tls_ca_file: Option<PathBuf>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
    tls_crl_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pool: "zroot".to_string(),
            state_dir: PathBuf::from("/var/db/keel"),
            socket: PathBuf::from("/var/run/keel-agentd.sock"),
            node_id: None,
            control_plane_addr: None,
            advertise_addr: None,
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
            tls_crl_file: None,
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
            "--pool" => config.pool = value,
            "--state-dir" => config.state_dir = PathBuf::from(value),
            "--socket" => config.socket = PathBuf::from(value),
            "--node-id" => config.node_id = Some(value),
            "--control-plane-addr" => config.control_plane_addr = Some(value),
            "--advertise-addr" => config.advertise_addr = Some(value),
            "--tls-ca-file" => config.tls_ca_file = Some(PathBuf::from(value)),
            "--tls-cert-file" => config.tls_cert_file = Some(PathBuf::from(value)),
            "--tls-key-file" => config.tls_key_file = Some(PathBuf::from(value)),
            "--tls-crl-file" => config.tls_crl_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.control_plane_addr.is_some()
        && (config.node_id.is_none()
            || config.advertise_addr.is_none()
            || config.tls_ca_file.is_none()
            || config.tls_cert_file.is_none()
            || config.tls_key_file.is_none()
            || config.tls_crl_file.is_none())
    {
        panic!(
            "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required when --control-plane-addr is set"
        );
    }
    config
}
```

In `main()`, replace:

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
```

with:

```rust
    if let (
        Some(node_id),
        Some(control_plane_addr),
        Some(advertise_addr),
        Some(ca_file),
        Some(cert_file),
        Some(key_file),
        Some(crl_file),
    ) = (
        config.node_id.clone(),
        config.control_plane_addr.clone(),
        config.advertise_addr.clone(),
        config.tls_ca_file.clone(),
        config.tls_cert_file.clone(),
        config.tls_key_file.clone(),
        config.tls_crl_file.clone(),
    ) {
        let (capacity_cpu, capacity_memory) = keel_agentd::capacity::detect()
            .unwrap_or_else(|e| panic!("failed to detect node capacity via sysctl: {e}"));
        let tls_server_config = keel_agentd::tls::load_server_config(&cert_file, &key_file, &ca_file, &crl_file)
            .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
        let tls_client_config = keel_agentd::tls::load_client_config(&cert_file, &key_file, &ca_file, &crl_file)
            .unwrap_or_else(|e| panic!("failed to load TLS client config: {e}"));
```

Update the existing tests in `main.rs`'s `mod tests`: add `config.tls_crl_file, None` to `defaults_have_no_control_plane_configuration`'s assertions; add `"--tls-crl-file", "/etc/keel/node-2.crl",` and its assertion to `parses_all_six_control_plane_flags` (rename it `parses_all_seven_control_plane_flags`); add `"--tls-crl-file", "/etc/keel/node-2.crl",` to the three `#[should_panic]` tests' args where the panic should still fire because a *different* required flag is missing, and update all four `#[should_panic(expected = "...")]` strings to the new 6-flag message. For example:

```rust
    #[test]
    fn parses_all_seven_control_plane_flags() {
        let config = parse_args_from(args(&[
            "--node-id", "node-2",
            "--control-plane-addr", "192.168.64.2:7620",
            "--advertise-addr", "192.168.64.2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
            "--tls-key-file", "/etc/keel/node-2.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
        assert_eq!(config.node_id, Some("node-2".to_string()));
        assert_eq!(config.control_plane_addr, Some("192.168.64.2:7620".to_string()));
        assert_eq!(config.advertise_addr, Some("192.168.64.2".to_string()));
        assert_eq!(config.tls_ca_file, Some(PathBuf::from("/etc/keel/ca.crt")));
        assert_eq!(config.tls_cert_file, Some(PathBuf::from("/etc/keel/node-2.crt")));
        assert_eq!(config.tls_key_file, Some(PathBuf::from("/etc/keel/node-2.key")));
        assert_eq!(config.tls_crl_file, Some(PathBuf::from("/etc/keel/crl.pem")));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required when --control-plane-addr is set")]
    fn control_plane_addr_without_node_id_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--advertise-addr", "192.168.64.2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
            "--tls-key-file", "/etc/keel/node-2.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required when --control-plane-addr is set")]
    fn control_plane_addr_without_advertise_addr_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--node-id", "node-2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
            "--tls-key-file", "/etc/keel/node-2.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required when --control-plane-addr is set")]
    fn control_plane_addr_without_tls_crl_file_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--node-id", "node-2",
            "--advertise-addr", "192.168.64.2",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/node-2.crt",
            "--tls-key-file", "/etc/keel/node-2.key",
        ]));
    }
```

(This replaces the old `control_plane_addr_without_tls_key_file_panics` test with `control_plane_addr_without_tls_crl_file_panics`, which now omits `--tls-crl-file` instead of `--tls-key-file` to exercise the newly-added flag specifically; both flags are already covered by the shared panic path.)

- [ ] **Step 5: Run the crate's tests**

Run: `cargo test -p keel-agentd`
Expected: all tests pass, including the two new revoked-certificate tests.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/tls.rs keel-agentd/src/http.rs keel-agentd/src/registration.rs keel-agentd/src/main.rs
git commit -m "keel-agentd: enforce CRL revocation on every TLS connection, both directions"
```

---

## Task 5: `keelctl` — CRL enforcement

**Files:**
- Modify: `keelctl/src/tls.rs`
- Modify: `keelctl/src/main.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: same fixtures as Tasks 3-4.
- Produces: `keelctl::tls::load_client_config`, now 4-argument. `keelctl`'s `--tls-crl-file` flag, required alongside its existing three `--tls-*` flags.

- [ ] **Step 1: Rewrite `keelctl/src/tls.rs` with CRL support**

Replace the entire file with:

```rust
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, ServerName};
use rustls::RootCertStore;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Once};

static CRYPTO_PROVIDER_INIT: Once = Once::new();

pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn load_client_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: &Path,
    crl_path: &Path,
) -> Result<rustls::ClientConfig, String> {
    ensure_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let roots = load_root_store(ca_path)?;
    let crls = load_crls(crl_path)?;
    let server_verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .build()
        .map_err(|e| format!("failed to build server certificate verifier: {e}"))?;
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(server_verifier)
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
        return Err(format!("no certificates found in {}", path.display()));
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

fn load_crls(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, String> {
    let file = File::open(path).map_err(|e| format!("failed to open CRL file {}: {e}", path.display()))?;
    let crls: Vec<_> = rustls_pemfile::crls(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse CRL file {}: {e}", path.display()))?;
    if crls.is_empty() {
        return Err(format!("no CRL found in {}", path.display()));
    }
    Ok(crls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
    }

    #[test]
    fn load_client_config_succeeds_with_valid_fixtures() {
        load_client_config(
            &fixture("fixture-client.crt"),
            &fixture("fixture-client.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .expect("expected a valid client config");
    }

    #[test]
    fn load_client_config_fails_on_a_missing_key_file() {
        let err = load_client_config(
            &fixture("fixture-client.crt"),
            &fixture("does-not-exist.key"),
            &fixture("ca.crt"),
            &fixture("crl.pem"),
        )
        .unwrap_err();
        assert!(err.contains("does-not-exist.key"), "got: {err}");
    }

    #[test]
    fn load_client_config_fails_on_a_missing_crl_file() {
        let err = load_client_config(
            &fixture("fixture-client.crt"),
            &fixture("fixture-client.key"),
            &fixture("ca.crt"),
            &fixture("does-not-exist-crl.pem"),
        )
        .unwrap_err();
        assert!(err.contains("does-not-exist-crl.pem"), "got: {err}");
    }

    #[test]
    fn server_name_from_addr_parses_the_host_and_drops_the_port() {
        let name = server_name_from_addr("10.0.0.1:7620").unwrap();
        assert_eq!(name, rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::new(10, 0, 0, 1).into()));
    }
}
```

- [ ] **Step 2: Thread `--tls-crl-file` through `keelctl/src/main.rs`**

Replace:

```rust
#[derive(Debug, PartialEq)]
enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: Option<String>, tls_ca_file: PathBuf, tls_cert_file: PathBuf, tls_key_file: PathBuf },
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let control_plane_addr = extract_flag(&mut args, "--control-plane-addr");
    let node = extract_flag(&mut args, "--node");
    let tls_ca_file = extract_flag(&mut args, "--tls-ca-file");
    let tls_cert_file = extract_flag(&mut args, "--tls-cert-file");
    let tls_key_file = extract_flag(&mut args, "--tls-key-file");

    let target = match resolve_target(socket, control_plane_addr, node, tls_ca_file, tls_cert_file, tls_key_file) {
```

with:

```rust
#[derive(Debug, PartialEq)]
enum Target {
    Socket(PathBuf),
    ControlPlane {
        addr: String,
        node: Option<String>,
        tls_ca_file: PathBuf,
        tls_cert_file: PathBuf,
        tls_key_file: PathBuf,
        tls_crl_file: PathBuf,
    },
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let control_plane_addr = extract_flag(&mut args, "--control-plane-addr");
    let node = extract_flag(&mut args, "--node");
    let tls_ca_file = extract_flag(&mut args, "--tls-ca-file");
    let tls_cert_file = extract_flag(&mut args, "--tls-cert-file");
    let tls_key_file = extract_flag(&mut args, "--tls-key-file");
    let tls_crl_file = extract_flag(&mut args, "--tls-crl-file");

    let target = match resolve_target(
        socket,
        control_plane_addr,
        node,
        tls_ca_file,
        tls_cert_file,
        tls_key_file,
        tls_crl_file,
    ) {
```

Replace `resolve_target`:

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

with:

```rust
fn resolve_target(
    socket: PathBuf,
    control_plane_addr: Option<String>,
    node: Option<String>,
    tls_ca_file: Option<String>,
    tls_cert_file: Option<String>,
    tls_key_file: Option<String>,
    tls_crl_file: Option<String>,
) -> Result<Target, String> {
    match (control_plane_addr, node, tls_ca_file, tls_cert_file, tls_key_file, tls_crl_file) {
        (Some(addr), node, Some(ca), Some(cert), Some(key), Some(crl)) => Ok(Target::ControlPlane {
            addr,
            node,
            tls_ca_file: PathBuf::from(ca),
            tls_cert_file: PathBuf::from(cert),
            tls_key_file: PathBuf::from(key),
            tls_crl_file: PathBuf::from(crl),
        }),
        (Some(_), _, _, _, _, _) => Err(
            "--tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required with --control-plane-addr"
                .to_string(),
        ),
        (None, Some(_), _, _, _, _) => Err("--node requires --control-plane-addr".to_string()),
        (None, None, _, _, _, _) => Ok(Target::Socket(socket)),
    }
}
```

Replace `dispatch` and `send_request_tcp`'s signature:

```rust
fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, tls_ca_file, tls_cert_file, tls_key_file, .. } => {
            send_request_tcp(addr, method, path, body, tls_ca_file, tls_cert_file, tls_key_file)
        }
    }
}
```

with:

```rust
fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, tls_ca_file, tls_cert_file, tls_key_file, tls_crl_file, .. } => {
            send_request_tcp(addr, method, path, body, tls_ca_file, tls_cert_file, tls_key_file, tls_crl_file)
        }
    }
}
```

Replace `send_request_tcp`'s signature and its `load_client_config` call:

```rust
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
```

with:

```rust
fn send_request_tcp(
    addr: &str,
    method: &str,
    path: &str,
    body: &str,
    tls_ca_file: &PathBuf,
    tls_cert_file: &PathBuf,
    tls_key_file: &PathBuf,
    tls_crl_file: &PathBuf,
) -> Result<String, String> {
    let client_config = std::sync::Arc::new(
        tls::load_client_config(tls_cert_file, tls_key_file, tls_ca_file, tls_crl_file)
            .map_err(|e| format!("failed to load TLS client config: {e}"))?,
    );
```

Update `main.rs`'s existing tests: every `resolve_target(...)` call gains a trailing `tls_crl_file` argument, and the error-message assertions and the `Target::ControlPlane` struct literal gain the new field:

```rust
    #[test]
    fn no_control_plane_flags_yields_socket_target() {
        let target = resolve_target(PathBuf::from("/var/run/keel-agentd.sock"), None, None, None, None, None, None).unwrap();
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
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "--tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required with --control-plane-addr"
        );
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
            Some("/etc/keel/crl.pem".to_string()),
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
                tls_crl_file: PathBuf::from("/etc/keel/crl.pem"),
            }
        );
    }
```

- [ ] **Step 3: Update `keelctl/tests/cli.rs`'s call sites**

Add `&fixture("crl.pem")` as the 4th argument to the 4 `load_server_config`/`load_client_config` calls (former lines 78, 104, 108, 149 — note line 149 is inside `start_fake_control_plane_with_truncated_body`). Add a `.arg("--tls-crl-file").arg(fixture("crl.pem"))` pair to both `run_keelctl_routed` and `run_keelctl_scheduled`'s `Command::new(...)` builders, right after the existing `.arg("--tls-key-file").arg(...)` pair:

```rust
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
        .arg("--tls-crl-file")
        .arg(fixture("crl.pem"))
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}
```

```rust
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
        .arg("--tls-crl-file")
        .arg(fixture("crl.pem"))
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}
```

- [ ] **Step 4: Run the workspace's tests**

Run: `cargo test -p keelctl`
Expected: all tests pass, including `keelctl/tests/cli.rs`'s existing end-to-end tests, now running over CRL-aware TLS.

- [ ] **Step 5: Commit**

```bash
git add keelctl/src/tls.rs keelctl/src/main.rs keelctl/tests/cli.rs
git commit -m "keelctl: require and enforce CRL revocation on control-plane-routed calls"
```

---

## Task 6: `keel-controlplane` — live TLS reload

**Files:**
- Modify: `keel-controlplane/src/tls.rs`
- Modify: `keel-controlplane/src/http.rs`
- Modify: `keel-controlplane/src/main.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `load_server_config`/`load_client_config` (Task 3).
- Produces: `pub struct ReloadingTls`, `ReloadingTls::spawn(cert_path: PathBuf, key_path: PathBuf, ca_path: PathBuf, crl_path: PathBuf, reload_interval: Duration) -> Result<Arc<ReloadingTls>, String>`, `.server_config(&self) -> Arc<rustls::ServerConfig>`, `.client_config(&self) -> Arc<rustls::ClientConfig>`. `http::run(listener: TcpListener, commands: Sender<Command>, reloading_tls: Arc<tls::ReloadingTls>)` (was: `tls_config: Arc<ServerConfig>, client_config: Arc<rustls::ClientConfig>`).

- [ ] **Step 1: Add `ReloadingTls` to `keel-controlplane/src/tls.rs`**

Add these imports to the top of the file (alongside the existing `use` lines):

```rust
use std::path::PathBuf;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;
```

Add this type after `load_client_config` and before `server_name_from_addr`:

```rust
pub struct ReloadingTls {
    cert_path: PathBuf,
    key_path: PathBuf,
    ca_path: PathBuf,
    crl_path: PathBuf,
    server: RwLock<Arc<rustls::ServerConfig>>,
    client: RwLock<Arc<rustls::ClientConfig>>,
}

impl ReloadingTls {
    pub fn spawn(
        cert_path: PathBuf,
        key_path: PathBuf,
        ca_path: PathBuf,
        crl_path: PathBuf,
        reload_interval: Duration,
    ) -> Result<Arc<Self>, String> {
        let server = load_server_config(&cert_path, &key_path, &ca_path, &crl_path)?;
        let client = load_client_config(&cert_path, &key_path, &ca_path, &crl_path)?;
        let this = Arc::new(Self {
            cert_path,
            key_path,
            ca_path,
            crl_path,
            server: RwLock::new(Arc::new(server)),
            client: RwLock::new(Arc::new(client)),
        });
        let reload_target = Arc::clone(&this);
        thread::spawn(move || loop {
            thread::sleep(reload_interval);
            reload_target.reload_once();
        });
        Ok(this)
    }

    fn reload_once(&self) {
        match load_server_config(&self.cert_path, &self.key_path, &self.ca_path, &self.crl_path) {
            Ok(cfg) => *self.server.write().unwrap() = Arc::new(cfg),
            Err(e) => eprintln!("keel-controlplane: TLS reload failed (server config): {e}"),
        }
        match load_client_config(&self.cert_path, &self.key_path, &self.ca_path, &self.crl_path) {
            Ok(cfg) => *self.client.write().unwrap() = Arc::new(cfg),
            Err(e) => eprintln!("keel-controlplane: TLS reload failed (client config): {e}"),
        }
    }

    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.server.read().unwrap())
    }

    pub fn client_config(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.client.read().unwrap())
    }
}
```

- [ ] **Step 2: Change `keel-controlplane/src/http.rs`'s `run()` to take a `ReloadingTls`**

Replace:

```rust
pub fn run(
    listener: TcpListener,
    commands: Sender<Command>,
    tls_config: Arc<ServerConfig>,
    client_config: Arc<rustls::ClientConfig>,
) {
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

with:

```rust
pub fn run(listener: TcpListener, commands: Sender<Command>, reloading_tls: Arc<tls::ReloadingTls>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = reloading_tls.server_config();
        let client_config = reloading_tls.client_config();
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

No other function in `http.rs` changes: `handle_connection`, `route`, `handle_forward`, `forward`, and every handler keep their existing `&Arc<rustls::ClientConfig>` signatures unchanged, since `run()` now hands each connection a config snapshot obtained from `reloading_tls` instead of one captured at process startup.

In `mod tests`, replace `start_test_server`'s and every other test-server helper's construction. Replace:

```rust
    fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let tls_config = Arc::new(
            tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &fixture("crl.pem"))
                .unwrap(),
        );
        let client_config = client_tls_config();
        thread::spawn(move || run(listener, commands, tls_config, client_config));
        addr
    }
```

with:

```rust
    fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let reloading_tls = tls::ReloadingTls::spawn(
            fixture("fixture-node.crt"),
            fixture("fixture-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap();
        thread::spawn(move || run(listener, commands, reloading_tls));
        addr
    }
```

(`Duration::from_secs(3600)` here is deliberately long: this helper is shared by every non-reload-focused test in the file, and those tests don't need the background reload thread to ever actually fire.)

- [ ] **Step 3: Wire `ReloadingTls` into `keel-controlplane/src/main.rs`**

Replace:

```rust
    let tls_config = keel_controlplane::tls::load_server_config(&cert_file, &key_file, &ca_file, &crl_file)
        .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
    let client_config = keel_controlplane::tls::load_client_config(&cert_file, &key_file, &ca_file, &crl_file)
        .unwrap_or_else(|e| panic!("failed to load TLS client config: {e}"));

    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, Arc::new(tls_config), Arc::new(client_config));
```

with:

```rust
    let reloading_tls = keel_controlplane::tls::ReloadingTls::spawn(
        cert_file,
        key_file,
        ca_file,
        crl_file,
        std::time::Duration::from_secs(30),
    )
    .unwrap_or_else(|e| panic!("failed to load TLS configuration: {e}"));

    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, reloading_tls);
```

- [ ] **Step 4: Add reload-behavior tests to `keel-controlplane/src/http.rs`**

Add, at the end of `mod tests`:

```rust
    #[test]
    fn reloading_tls_server_config_picks_up_a_replaced_certificate_without_restart() {
        let cert_dir = std::env::temp_dir()
            .join(format!("keel-controlplane-reload-test-{}", std::process::id()));
        std::fs::create_dir_all(&cert_dir).unwrap();
        let cert_path = cert_dir.join("node.crt");
        let key_path = cert_dir.join("node.key");
        std::fs::copy(fixture("fixture-node.crt"), &cert_path).unwrap();
        std::fs::copy(fixture("fixture-node.key"), &key_path).unwrap();

        let reloading = tls::ReloadingTls::spawn(
            cert_path.clone(),
            key_path.clone(),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_millis(50),
        )
        .unwrap();

        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || run(listener, commands, reloading));

        let (status, _) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200, "expected the initial fixture-node certificate to be served");

        // wrong-ca-node.crt is signed by a different, untrusted CA, so once
        // the server starts presenting it, any client trusting only the real
        // ca.crt must fail the handshake -- this is the observable proof
        // that the reload thread actually swapped in the replacement file.
        std::fs::copy(fixture("wrong-ca-node.crt"), &cert_path).unwrap();
        std::fs::copy(fixture("wrong-ca-node.key"), &key_path).unwrap();
        thread::sleep(Duration::from_millis(200));

        let result = std::panic::catch_unwind(|| send_request(&addr, "GET", "/nodes", ""));
        assert!(
            result.is_err() || result.unwrap().0 != 200,
            "expected the server's replaced certificate to be rejected by the client after reload"
        );
    }

    #[test]
    fn reloading_tls_keeps_serving_the_last_good_config_if_the_replacement_is_malformed() {
        let cert_dir = std::env::temp_dir()
            .join(format!("keel-controlplane-reload-bad-test-{}", std::process::id()));
        std::fs::create_dir_all(&cert_dir).unwrap();
        let cert_path = cert_dir.join("node.crt");
        let key_path = cert_dir.join("node.key");
        std::fs::copy(fixture("fixture-node.crt"), &cert_path).unwrap();
        std::fs::copy(fixture("fixture-node.key"), &key_path).unwrap();

        let reloading = tls::ReloadingTls::spawn(
            cert_path.clone(),
            key_path.clone(),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_millis(50),
        )
        .unwrap();

        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || run(listener, commands, reloading));

        std::fs::write(&cert_path, "not a certificate").unwrap();
        thread::sleep(Duration::from_millis(200));

        let (status, _) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200, "expected the last-known-good certificate to keep being served after a malformed reload");
    }
```

Add `use std::time::Duration;` to `http.rs`'s top-level imports if not already present (it is not, in the pre-Task-6 file).

- [ ] **Step 5: Update `keelctl/tests/cli.rs`'s direct call to `keel_controlplane::http::run`**

Replace, in `start_test_control_plane_with_node`:

```rust
    let tls_config = std::sync::Arc::new(
        keel_controlplane::tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &fixture("crl.pem"))
            .unwrap(),
    );
    let client_config = std::sync::Arc::new(
        keel_controlplane::tls::load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &fixture("crl.pem"))
            .unwrap(),
    );
    thread::spawn(move || keel_controlplane::http::run(listener, commands, tls_config, client_config));
```

with:

```rust
    let reloading_tls = keel_controlplane::tls::ReloadingTls::spawn(
        fixture("fixture-node.crt"),
        fixture("fixture-node.key"),
        fixture("ca.crt"),
        fixture("crl.pem"),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    thread::spawn(move || keel_controlplane::http::run(listener, commands, reloading_tls));
```

`keelctl/tests/cli.rs`'s other `keel_controlplane::tls::load_server_config` call site, inside `start_fake_control_plane_with_truncated_body`, needs no change in this task: Task 5 Step 3 already added its 4th `&fixture("crl.pem")` argument, and it constructs its own bare `rustls::ServerConnection` loop rather than calling `http::run`, so it never needs a `ReloadingTls`.

- [ ] **Step 6: Run the crate's and workspace's tests**

Run: `cargo test -p keel-controlplane && cargo test -p keelctl`
Expected: all tests pass, including the two new reload tests.

- [ ] **Step 7: Commit**

```bash
git add keel-controlplane/src/tls.rs keel-controlplane/src/http.rs keel-controlplane/src/main.rs keelctl/tests/cli.rs
git commit -m "keel-controlplane: hot-reload TLS cert/key/ca/crl on a background timer, no restart needed"
```

---

## Task 7: `keel-agentd` — live TLS reload

**Files:**
- Modify: `keel-agentd/src/tls.rs`
- Modify: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/registration.rs`
- Modify: `keel-agentd/src/main.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `load_server_config`/`load_client_config` (Task 4).
- Produces: `pub struct ReloadingTls` (identical shape to Task 6). `http::run_tls(listener: TcpListener, commands: Sender<Command>, reloading_tls: Arc<ReloadingTls>)` (was: `tls_config: Arc<ServerConfig>`). `registration::spawn(..., reloading_tls: Arc<ReloadingTls>, ...)` (was: `client_config: Arc<rustls::ClientConfig>`).

- [ ] **Step 1: Add `ReloadingTls` to `keel-agentd/src/tls.rs`**

Add these imports to the top of the file (alongside the existing `use` lines):

```rust
use std::path::PathBuf;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;
```

Add this type after `load_client_config` and before `server_name_from_addr`:

```rust
pub struct ReloadingTls {
    cert_path: PathBuf,
    key_path: PathBuf,
    ca_path: PathBuf,
    crl_path: PathBuf,
    server: RwLock<Arc<rustls::ServerConfig>>,
    client: RwLock<Arc<rustls::ClientConfig>>,
}

impl ReloadingTls {
    pub fn spawn(
        cert_path: PathBuf,
        key_path: PathBuf,
        ca_path: PathBuf,
        crl_path: PathBuf,
        reload_interval: Duration,
    ) -> Result<Arc<Self>, String> {
        let server = load_server_config(&cert_path, &key_path, &ca_path, &crl_path)?;
        let client = load_client_config(&cert_path, &key_path, &ca_path, &crl_path)?;
        let this = Arc::new(Self {
            cert_path,
            key_path,
            ca_path,
            crl_path,
            server: RwLock::new(Arc::new(server)),
            client: RwLock::new(Arc::new(client)),
        });
        let reload_target = Arc::clone(&this);
        thread::spawn(move || loop {
            thread::sleep(reload_interval);
            reload_target.reload_once();
        });
        Ok(this)
    }

    fn reload_once(&self) {
        match load_server_config(&self.cert_path, &self.key_path, &self.ca_path, &self.crl_path) {
            Ok(cfg) => *self.server.write().unwrap() = Arc::new(cfg),
            Err(e) => eprintln!("keel-agentd: TLS reload failed (server config): {e}"),
        }
        match load_client_config(&self.cert_path, &self.key_path, &self.ca_path, &self.crl_path) {
            Ok(cfg) => *self.client.write().unwrap() = Arc::new(cfg),
            Err(e) => eprintln!("keel-agentd: TLS reload failed (client config): {e}"),
        }
    }

    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.server.read().unwrap())
    }

    pub fn client_config(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.client.read().unwrap())
    }
}
```

- [ ] **Step 2: Change `keel-agentd/src/http.rs`'s `run_tls()` to take a `ReloadingTls`**

Replace:

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
```

with:

```rust
pub fn run_tls(listener: TcpListener, commands: Sender<Command>, reloading_tls: Arc<crate::tls::ReloadingTls>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = reloading_tls.server_config();
        thread::spawn(move || {
            let Ok(conn) = ServerConnection::new(tls_config) else { return };
            let mut tls_stream = TlsStream::new(conn, stream);
            if handle_connection_tls(&mut tls_stream, &commands).is_err() {
                eprintln!("keel-agentd: TLS handshake or request handling failed for a connection");
            }
        });
    }
}
```

In `mod tests`, replace `start_tcp_test_server`. Replace:

```rust
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
            tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &fixture("crl.pem"))
                .unwrap(),
        );
        thread::spawn(move || run_tls(listener, commands, tls_config));
        addr
    }
```

with:

```rust
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
        let reloading_tls = tls::ReloadingTls::spawn(
            fixture("fixture-node.crt"),
            fixture("fixture-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap();
        thread::spawn(move || run_tls(listener, commands, reloading_tls));
        addr
    }
```

Add `use std::time::Duration;` to `http.rs`'s top-level imports if not already present.

Add a reload test at the end of `mod tests`, matching Task 6 Step 4's `keel-controlplane` reload test but calling `run_tls`:

```rust
    #[test]
    fn reloading_tls_server_config_picks_up_a_replaced_certificate_without_restart() {
        let cert_dir = std::env::temp_dir().join(format!("keel-agentd-reload-test-{}", std::process::id()));
        std::fs::create_dir_all(&cert_dir).unwrap();
        let cert_path = cert_dir.join("node.crt");
        let key_path = cert_dir.join("node.key");
        std::fs::copy(fixture("fixture-node.crt"), &cert_path).unwrap();
        std::fs::copy(fixture("fixture-node.key"), &key_path).unwrap();

        let reloading = tls::ReloadingTls::spawn(
            cert_path.clone(),
            key_path.clone(),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_millis(50),
        )
        .unwrap();

        let state_dir = std::env::temp_dir()
            .join(format!("keel-agentd-reload-test-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_dir);
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler =
            Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), "zroot".to_string(), state_dir).unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || run_tls(listener, commands, reloading));

        let (status, _) = send_request_tcp(&addr, "GET", "/jails", "");
        assert_eq!(status, 200, "expected the initial fixture-node certificate to be served");

        std::fs::copy(fixture("wrong-ca-node.crt"), &cert_path).unwrap();
        std::fs::copy(fixture("wrong-ca-node.key"), &key_path).unwrap();
        thread::sleep(Duration::from_millis(200));

        let result = std::panic::catch_unwind(|| send_request_tcp(&addr, "GET", "/jails", ""));
        assert!(
            result.is_err() || result.unwrap().0 != 200,
            "expected the server's replaced certificate to be rejected by the client after reload"
        );
    }
```

- [ ] **Step 3: Change `keel-agentd/src/registration.rs`'s `spawn()` to take a `ReloadingTls`**

Replace:

```rust
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
```

with:

```rust
// each parameter is independently needed by the registration loop; bundling into a struct would be over-engineering for this single call site
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    reloading_tls: Arc<tls::ReloadingTls>,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            let client_config = reloading_tls.client_config();
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
```

`register_once`, `heartbeat_once`, and `send_request` keep their existing `&Arc<rustls::ClientConfig>` signatures unchanged — only `spawn()`'s loop now fetches a fresh snapshot from `reloading_tls` once per tick instead of closing over one Arc for the daemon's whole lifetime.

In `mod tests`, add two new helper functions alongside the existing `node_client_config()`/`wrong_ca_client_config()` (which stay, since `send_request` in the truncated-body test still takes a bare `&Arc<rustls::ClientConfig>` directly):

```rust
    fn node_reloading_tls() -> std::sync::Arc<crate::tls::ReloadingTls> {
        crate::tls::ReloadingTls::spawn(
            fixture("fixture-node.crt"),
            fixture("fixture-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap()
    }

    fn wrong_ca_reloading_tls() -> std::sync::Arc<crate::tls::ReloadingTls> {
        crate::tls::ReloadingTls::spawn(
            fixture("wrong-ca-node.crt"),
            fixture("wrong-ca-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap()
    }
```

Update the 3 existing `spawn(...)` call sites (in `registers_and_then_keeps_heartbeating`, `heartbeats_report_the_reconcilers_committed_resources`, `registration_with_a_wrong_ca_certificate_never_registers`) to pass `node_reloading_tls()` / `wrong_ca_reloading_tls()` instead of `node_client_config()` / `wrong_ca_client_config()`. For example:

```rust
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            node_reloading_tls(),
            commands,
        );
```

Also update `start_test_control_plane`'s own `keel_controlplane::http::run(...)` call (it constructs the fake control plane's own listener, not the thing under test) to use `keel_controlplane::tls::ReloadingTls`:

```rust
    fn start_test_control_plane() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let reloading_tls = keel_controlplane::tls::ReloadingTls::spawn(
            fixture("fixture-node.crt"),
            fixture("fixture-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap();
        thread::spawn(move || keel_controlplane::http::run(listener, commands, reloading_tls));
        addr
    }
```

- [ ] **Step 4: Wire `ReloadingTls` into `keel-agentd/src/main.rs`**

Replace:

```rust
        let (capacity_cpu, capacity_memory) = keel_agentd::capacity::detect()
            .unwrap_or_else(|e| panic!("failed to detect node capacity via sysctl: {e}"));
        let tls_server_config = keel_agentd::tls::load_server_config(&cert_file, &key_file, &ca_file, &crl_file)
            .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
        let tls_client_config = keel_agentd::tls::load_client_config(&cert_file, &key_file, &ca_file, &crl_file)
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

with:

```rust
        let (capacity_cpu, capacity_memory) = keel_agentd::capacity::detect()
            .unwrap_or_else(|e| panic!("failed to detect node capacity via sysctl: {e}"));
        let reloading_tls = keel_agentd::tls::ReloadingTls::spawn(
            cert_file,
            key_file,
            ca_file,
            crl_file,
            Duration::from_secs(30),
        )
        .unwrap_or_else(|e| panic!("failed to load TLS configuration: {e}"));
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
            std::sync::Arc::clone(&reloading_tls),
            commands.clone(),
        );

        eprintln!("keel-agentd: serving jails API over TLS on {advertise_addr}");
        let tcp_listener = TcpListener::bind(&advertise_addr)
            .unwrap_or_else(|e| panic!("failed to bind jails-API TCP listener on {advertise_addr}: {e}"));
        let tcp_commands = commands.clone();
        thread::spawn(move || keel_agentd::http::run_tls(tcp_listener, tcp_commands, reloading_tls));
    }
```

- [ ] **Step 5: Update `keelctl/tests/cli.rs`'s direct call to `keel_agentd::http::run_tls`**

Replace, in `start_test_agentd_tcp`:

```rust
    let tls_config = std::sync::Arc::new(
        keel_agentd::tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"), &fixture("crl.pem"))
            .unwrap(),
    );
    thread::spawn(move || keel_agentd::http::run_tls(listener, commands, tls_config));
```

with:

```rust
    let reloading_tls = keel_agentd::tls::ReloadingTls::spawn(
        fixture("fixture-node.crt"),
        fixture("fixture-node.key"),
        fixture("ca.crt"),
        fixture("crl.pem"),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    thread::spawn(move || keel_agentd::http::run_tls(listener, commands, reloading_tls));
```

- [ ] **Step 6: Run the crate's and workspace's tests**

Run: `cargo test -p keel-agentd && cargo test -p keelctl && cargo test --workspace`
Expected: all tests pass, including the new reload test.

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/src/tls.rs keel-agentd/src/http.rs keel-agentd/src/registration.rs keel-agentd/src/main.rs keelctl/tests/cli.rs
git commit -m "keel-agentd: hot-reload TLS cert/key/ca/crl on a background timer, no restart needed"
```

---

## VM Verification (after all 7 tasks, before considering the milestone complete)

Following this project's established discipline (every milestone since Milestone 2 has been verified on the real FreeBSD 15.1 VM, not assumed), perform this manual verification across the three-node setup before closing out the milestone:

1. Run `scripts/gen-certs.sh init` on the CA host if not already done (it is idempotent — safe to re-run against an existing Milestone 12 CA).
2. Confirm the `openssl ca`/`-gencrl` workflow behaves identically on FreeBSD 15.1's base `openssl` binary as it did in Task 1's development-machine verification (the Open Question flagged in the design spec) — run the exact Task 1 Step 2 command sequence directly on the VM.
3. Issue certificates for all three nodes and the control plane as usual, plus one operator certificate, and redistribute along with the initial (empty) `crl.pem`; confirm the Milestone 10/11/12 apply/get/delete-through-the-scheduler round trip works unchanged.
4. Pick one node, revoke its certificate on the CA host (`scripts/gen-certs.sh revoke <node-name>`), redistribute only the refreshed `crl.pem` to every host, and confirm that node is rejected in both directions (it can no longer register/heartbeat, and `keel-controlplane` can no longer forward to it) within one reload interval (30 seconds), with no restart of `keel-controlplane` or any `keel-agentd`.
5. Rotate a different node's certificate entirely (`scripts/gen-certs.sh node <node-name> <ip>` against an existing name), distribute the new cert+key plus the refreshed `crl.pem`, and confirm it's picked up live with no restart, while a scheduled apply/get/delete round trip keeps working throughout for every node not touched by either action.
6. Clean teardown on all three VMs afterward.

Record the outcome the same way Milestones 2-12 did in `README.md`'s "The journey so far" section once verification is complete.
