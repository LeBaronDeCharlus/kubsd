# Milestone 13: Certificate Revocation and Rotation Automation (Sub-Project 4, Third Milestone)

Status: Approved
Date: 2026-07-15

## Context

Milestone 12 replaced the control plane's shared bearer token with mutual
TLS: every connection is encrypted, and identity is proven by a certificate
signed by a private CA the operator generates once. That milestone's own
design spec was explicit about what it left undone: no revocation (the only
way to eject one compromised identity was regenerating the CA and
redistributing every certificate in the cluster, the same all-or-nothing
cost the shared-secret token already had, just scoped per-identity instead
of cluster-wide), and no rotation story beyond a fully manual procedure with
no way for a running `keel-controlplane` or `keel-agentd` to pick up a
replacement certificate, key, CA, or CRL without a restart. This milestone
closes both gaps: certificate revocation lists (CRLs), checked at the TLS
handshake itself using support already built into `rustls` 0.23 (no new
Rust dependency), and a background reload thread in both long-running
daemons so a freshly issued certificate or a freshly regenerated CRL takes
effect on disk, live, with no restart and no coordinated fleet-wide
redeploy.

## Goals (Milestone 13)

- Revoking one identity's certificate (`gen-certs.sh revoke <name>`) does
  not touch the CA or any other identity's certificate, and takes effect at
  the TLS handshake itself, before any HTTP request is parsed, the same
  transport-layer strength Milestone 12 already established for identity
  verification generally.
- Revocation is checked symmetrically: a listener verifying an incoming
  client certificate, and an outbound caller verifying the peer's server
  certificate, both consult the same CRL. A revoked node's certificate is
  rejected whether that node is dialing in (registering, heartbeating, or
  a `keelctl` operator's own certificate) or being dialed (`forward()`
  sending it a jail spec).
- Reissuing a node, client, or control-plane certificate under an existing
  name (`gen-certs.sh node <name> <addr>` / `client <name>`) automatically
  revokes that identity's previous certificate and regenerates the CRL, in
  the same command, only after the new certificate is confirmed issued.
  This is "rotation": replacing a credential, not just adding a spare one
  that coexists with the old.
- `keel-controlplane` and `keel-agentd` reload their certificate, key, CA,
  and CRL files from disk on a background timer and hot-swap the active
  TLS configuration with no restart, so distributing a rotated certificate
  or a refreshed CRL to a running fleet is the operator's only remaining
  manual step.
- A reload failure (a file caught mid-copy, a malformed PEM) never brings
  down a running process: the last-known-good TLS configuration keeps
  serving, and the failure is logged once.

## Non-Goals (Milestone 13)

- **No OCSP.** A live revocation-checking responder is a different,
  heavier mechanism than a CRL an operator regenerates and distributes as
  a file; CRLs are the only revocation mechanism this milestone builds.
- **No automatic/scheduled certificate renewal.** Issuing a fresh
  certificate is still an operator running `gen-certs.sh`; there is no
  ACME-style automation, no live CA endpoint, and no code that decides on
  its own that a certificate needs replacing.
- **No change to certificate validity periods.** Certificates are still
  issued with a 10-year default validity; this milestone does not shorten
  that default or add expiry-driven rotation. Rotation is something an
  operator decides to do (e.g., on suspected compromise, or as periodic
  hygiene), not something triggered by an approaching expiry date.
- **No CRL expiry enforcement.** `rustls`'s default `ExpirationPolicy` for
  CRL checking is `Ignore`, meaning a CRL's own `nextUpdate` field is not
  enforced; this milestone does not change that default. Keeping a CRL
  current is an operator responsibility, the same "real but distant, not
  solved here" framing Milestone 12 already used for certificate expiry
  itself.
- **No per-identity authorization change.** A valid, unrevoked certificate
  from any node or operator still gets exactly the same access as any
  other; revocation is a binary valid/invalid gate, not a permissions
  mechanism.
- **No new Rust dependency.** `rustls` 0.23's `WebPkiClientVerifier` and
  `WebPkiServerVerifier` both already support `with_crls(...)`, and
  `rustls-pemfile` already parses CRL PEM data into the type both expect.
  Certificate and CRL *generation* stays entirely in `gen-certs.sh`,
  shelling out to `openssl`, exactly as Milestone 12 kept it out of Rust.
- **`keel-agentd`'s Unix socket is completely unaffected.** No TLS, no
  certificates, no CRL, matching every prior milestone's treatment of the
  local API.
- **No reload-interval flag.** The background reload thread's interval is
  a fixed constant in each binary, matching this project's existing
  convention of hardcoded timing constants (heartbeat interval, liveness
  threshold, backoff caps) rather than a configurable flag for every
  timer.

## Architecture

### `gen-certs.sh`: from ad hoc issuance to a real CA database

Producing a CRL requires `openssl ca -gencrl`, which requires a real
`openssl ca` certificate database (`index.txt`, `serial`, `crlnumber`, a
config file), not the ad hoc `openssl x509 -req -CAcreateserial` issuance
Milestone 12 used. `init` now also creates `$OUT_DIR/ca-db/` (empty
`index.txt`, a starting `serial`/`crlnumber`, and a minimal generated
`openssl.cnf` pointing at the CA's key/cert and that database) and
immediately generates an empty `$OUT_DIR/crl.pem`, since `--tls-crl-file`
becomes a required flag from day one and every binary needs a real file to
load at first startup even before anything has ever been revoked.

`node`/`client` issuance moves from `openssl x509 -req -CA ca.crt -CAkey
ca.key` to `openssl ca -config ca-db/openssl.cnf -in <name>.csr -out
<name>.crt -days N -batch`, which records the issued serial in
`index.txt`.

Today's `issue_leaf` writes `$OUT_DIR/$name.key` and `$OUT_DIR/$name.crt`
directly, in place, starting with `openssl genrsa -out $name.key` as its
very first step. That's fine for first-time issuance, but reissuing a name
that already has a live certificate can't reuse that same in-place write:
if the new keypair or signing step failed partway, `$name.key` would
already be clobbered by the new (uncertified) key while the *old*,
still-CA-valid certificate's matching key is gone, stranding the identity
even though the CA still considers its old serial unrevoked. Reissuance
therefore generates the new key/CSR/certificate into temporary files
first, and only after `openssl ca` confirms the new certificate is signed
does it: copy the about-to-be-replaced `$name.crt` aside to
`$name.crt.previous` (the input `-revoke` needs), rename the temp key and
certificate into `$name.key`/`$name.crt`, revoke `$name.crt.previous`'s
serial, and regenerate the CRL. In order:

```bash
./scripts/gen-certs.sh node node-4 192.168.64.4
#  1. generate a fresh keypair + CSR + certificate for node-4 into temp
#     files (new serial); $OUT_DIR/node-4.key and node-4.crt are untouched
#     so far, so a failure here leaves the existing identity fully intact
#  2. only once step 1 succeeds: if node-4 already had a certificate on
#     record, copy it aside to node-4.crt.previous, then rename the temp
#     key/cert into node-4.key/node-4.crt
#  3. revoke node-4.crt.previous's serial:
#     openssl ca -config ca-db/openssl.cnf -revoke node-4.crt.previous
#  4. regenerate the CRL: openssl ca -config ca-db/openssl.cnf -gencrl -out crl.pem
```

Doing the new keypair/CSR/signing work against temp files, before touching
anything under `$name.key`/`$name.crt`, means a failed reissue (e.g., a
transient `openssl` error) never leaves an identity with zero valid
certificates *and no way to use them*: the old key and certificate stay on
disk, untouched, until the new pair is confirmed good.

New subcommands:

```bash
./scripts/gen-certs.sh revoke <name>   # revoke without reissuing, then regenerate crl.pem
./scripts/gen-certs.sh crl             # force-regenerate crl.pem with no status change
```

`revoke` looks up `$OUT_DIR/<name>.crt`, runs `openssl ca -revoke` against
it, and regenerates `crl.pem` in the same command, so a single operator
action (revoke) always leaves a ready-to-distribute CRL behind, never a
revoked-in-the-database-but-not-yet-reflected-in-the-file gap.

**Flagged risk:** `openssl ca`/`-gencrl` is a heavier, more
configuration-format-sensitive subcommand than the `req`/`x509 -req`
primitives every prior milestone has relied on. Whether FreeBSD 15.1's base
`openssl` binary runs this workflow identically to the development
machine's needs to be verified directly on the VM before the implementation
plan locks in the exact config file this script generates, the same
"verify the FreeBSD-specific thing for real" discipline Milestones 6 and 7
already applied to `daemon(8)` pidfile flags and `TcpListener` rebind
behavior.

### CRL enforcement in `tls.rs` (both crates), symmetric

`load_server_config` (used by `keel-controlplane::http::run` and
`keel-agentd::http::run_tls`) gains a `crl_path: &Path` parameter. The CRL
is parsed via `rustls_pemfile::crls()` into `CertificateRevocationListDer`,
and `WebPkiClientVerifier::builder(roots).with_crls(crls).build()`
replaces today's plain `WebPkiClientVerifier::builder(roots).build()`. A
client presenting a revoked certificate now fails the handshake exactly
the way a client presenting no certificate, or one signed by an unrelated
CA, already does.

`load_client_config` (used by `keel-controlplane::http::forward`,
`keel-agentd::registration::send_request`, and
`keelctl::send_request_tcp`) also gains a `crl_path` parameter. Verifying
the *peer's* certificate with CRL awareness needs a custom verifier, so
`ClientConfig::builder().with_root_certificates(roots)` is replaced by:

```rust
let server_verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
    .with_crls(crls)
    .build()
    .map_err(|e| format!("failed to build server certificate verifier: {e}"))?;
rustls::ClientConfig::builder()
    .dangerous()
    .with_custom_certificate_verifier(server_verifier)
    .with_client_auth_cert(certs, key)
```

(`dangerous()` is `rustls`'s name for "supply your own verifier"; using its
own `WebPkiServerVerifier` here changes nothing about what's actually
verified, it only adds the CRL check on top of the same CA-chain
verification `with_root_certificates` already did.) This closes the
asymmetric gap where a revoked node could no longer dial in, but
`keel-controlplane` would still happily dial out and forward live jail
specs to it, since forwarding is an outbound connection verifying the
node's *server* certificate, not its client certificate.

### Live reload: `ReloadingTls`

A new type in each crate's `tls.rs` (duplicated per crate, matching this
project's existing choice to keep `tls.rs` crate-local rather than extract
a shared library crate):

```rust
pub struct ReloadingTls {
    server: Arc<RwLock<Arc<rustls::ServerConfig>>>,
    client: Arc<RwLock<Arc<rustls::ClientConfig>>>,
}

impl ReloadingTls {
    pub fn spawn(
        cert_path: PathBuf, key_path: PathBuf, ca_path: PathBuf, crl_path: PathBuf,
        reload_interval: Duration,
    ) -> Result<Self, String> {
        // Initial load reuses load_server_config/load_client_config and
        // propagates failure to the caller, which panics — startup stays
        // exactly as fail-fast as Milestone 12 already made it.
        ...
        // Spawns one background thread that wakes every reload_interval,
        // reloads both configs from the same four paths, and swaps them
        // into the RwLocks on success. On failure: one eprintln!, keep
        // serving the last-good configuration, never panic.
    }

    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        self.server.read().unwrap().clone()
    }
    pub fn client_config(&self) -> Arc<rustls::ClientConfig> {
        self.client.read().unwrap().clone()
    }
}
```

`reload_interval` is a real constructor parameter, not a hardcoded private
constant, following the exact pattern `registration::spawn`'s
`heartbeat_interval` already established: production code (`main.rs`)
passes a real interval (proposed: 30 seconds), tests pass short durations
like `Duration::from_millis(50)` so reload behavior can be verified without
slow, wall-clock-bound tests.

`keel-controlplane::main.rs` and `keel-agentd::main.rs` each construct one
`ReloadingTls` in place of today's one-shot `load_server_config`/
`load_client_config` + `Arc::new(...)`, and thread it (or an `Arc` around
it) down to `http::run`/`forward`/`registration::spawn`, each of which
calls `.server_config()`/`.client_config()` fresh at each accept or dial
instead of closing over a bare `Arc` captured once at startup.

`keelctl` is untouched by this section: a single CLI invocation already
loads fresh from disk on every run, so "rotation" already happens for it
for free. It only gains the new `crl_path` parameter on `load_client_config`
from the previous section.

### CLI flags

A new `--tls-crl-file <path>`, required in exactly the same grouping
`--tls-cert-file`/`--tls-key-file`/`--tls-ca-file` already are:
unconditionally on `keel-controlplane`, paired with the existing
control-plane flag group on `keel-agentd`/`keelctl`. Plain single-node
usage of either binary remains entirely unaffected, as always.

## Error Handling

- A revoked certificate fails the handshake with a plain `io::Error`,
  the same shape as a missing or wrong-CA certificate already produces;
  no new error path, no HTTP response, the connection is simply closed.
- Startup certificate/key/CA/CRL loading failures: `panic!` on
  `keel-controlplane`/`keel-agentd`, a graceful `Err`/`ExitCode::FAILURE`
  on `keelctl`, unchanged from Milestone 12.
- Reload-thread failures (a file caught mid-copy by a concurrent `scp`, a
  malformed PEM, a CRL that fails to parse): one `eprintln!` in the reload
  loop's error branch, and the last-known-good configuration keeps being
  served. A reload failure never panics a running process; this is what
  makes unattended background reload safe to run continuously.
- `gen-certs.sh revoke`/reissue-triggered revocation: if the new
  certificate issuance step fails, the old certificate and serial are left
  untouched and unrevoked, so a failed rotation attempt never strands an
  identity with zero valid certificates.

## Testing Strategy

- Unit tests for CRL loading (valid PEM, missing file, malformed PEM) in
  each crate's `tls.rs`, mirroring Milestone 12's existing certificate/key
  loading tests.
- A new fixture: `testdata/tls/crl.pem` includes one already-revoked
  certificate (and its matching cert+key fixture), alongside the existing
  valid node/client fixtures, so tests can prove rejection without needing
  to actually run `gen-certs.sh revoke` inside the test suite.
- Integration tests, both directions: a listener (`keel-controlplane`/
  `keel-agentd`) rejects a client presenting the revoked fixture
  certificate, handshake fails, no response; an outbound call site
  (`forward()`, `registration::send_request`, `keelctl`) rejects a fake
  server presenting the revoked fixture certificate, the call fails
  rather than silently succeeding.
- `ReloadingTls` tests: construct with a short `reload_interval`, make a
  successful connection against the initial certificate, replace the
  on-disk cert/key files with a different fixture pair, wait past the
  interval, and confirm the *next* connection negotiates against the new
  certificate, proving the swap takes effect with no restart. A companion
  test replaces the files with a malformed pair and confirms the old,
  last-known-good configuration keeps being served rather than the
  reload thread panicking or serving nothing.
- VM verification (three real nodes, the same discipline as every prior
  milestone): revoke one live node's certificate on the CA host,
  redistribute only the refreshed `crl.pem` to every host, and confirm
  that node is rejected, in both directions, within one reload interval
  with no restart of `keel-controlplane` or any `keel-agentd`. Separately,
  rotate a different node's certificate and key entirely (reissue,
  auto-revoking its old serial), distribute the new cert+key plus the
  refreshed CRL, and confirm it is picked up live with no restart, while a
  scheduled apply/get/delete round trip keeps working throughout for
  every node not touched by either action.

## Open Questions / Deferred Decisions

- Whether `openssl ca -gencrl`'s config-file requirements behave
  identically on FreeBSD 15.1's base `openssl` as on the development
  machine is unverified; this must be checked on the VM before the
  implementation plan finalizes the exact `openssl.cnf` `gen-certs.sh`
  generates.
- CRL expiry (`nextUpdate`) enforcement remains off by `rustls`'s own
  default; if that default ever needs tightening, it's a small,
  self-contained follow-up, not a reason to revisit this milestone's
  scope.
- Scheduled/automatic renewal ahead of a certificate's expiry remains
  unaddressed; rotation is still something an operator decides to do, not
  something the system detects and triggers on its own.
- No expiry or revocation-related monitoring/alerting exists; an operator
  who revokes a certificate has to independently confirm the affected
  identity is actually rejected, the same "tracked by the operator, not by
  any code this milestone adds" framing Milestone 12 already used.
