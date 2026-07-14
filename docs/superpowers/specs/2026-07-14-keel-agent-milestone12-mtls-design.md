# Milestone 12: Mutual TLS for the Control Plane (Sub-Project 4, Second Milestone)

Status: Approved
Date: 2026-07-14

## Context

Milestone 11 closed the control plane's authorization gap with a single
shared bearer token: every request to `keel-controlplane` or a
`keel-agentd`'s opt-in TCP listener now has to carry a matching
`Authorization: Bearer <token>` header. That milestone's own design spec
was explicit about what it left undone: no confidentiality (an on-path
eavesdropper on the same LAN segment can still read the token and every
spec body in flight), no per-identity credential (one leaked token
compromises the whole cluster equally, with no way to revoke just one
node's or operator's access), and no rotation story beyond regenerating
the one shared secret and redistributing it everywhere. This milestone
replaces that shared secret with mutual TLS: every connection in the
system is encrypted, and identity is proven by a certificate signed by a
private CA the operator generates once, not by a string copied to every
host.

This is a bigger lift than any milestone since the control plane itself
(Milestone 7). It is this project's first genuinely new runtime
dependency: `rustls` and its companion `rustls-pemfile`, since hand-rolling
TLS is a security mistake, not a stylistic one, unlike every prior
milestone's "no new dependency" discipline. It also removes code
Milestone 11 only just added, since a shared secret and per-identity
certificates are not two mechanisms this project intends to run side by
side going forward.

## Goals (Milestone 12)

- `keel-controlplane`'s TCP listener and `keel-agentd`'s opt-in
  `--advertise-addr` TCP listener both require and verify a client
  certificate, signed by the shared private CA, during the TLS handshake
  itself, before any HTTP request is parsed.
- Every outbound connection this project already makes,
  `keel-agentd`'s registration/heartbeat calls, `keel-controlplane`'s
  forwarding calls to a node, and `keelctl`'s control-plane-routed calls,
  presents its own client certificate and verifies the remote party's
  certificate against the same CA.
- One certificate per node, named by its node id and carrying its
  `advertise-addr` as a subject alternative name (SAN), usable both as a
  TLS server certificate (when the control plane dials it to forward a
  request) and a client certificate (when it dials the control plane to
  register or heartbeat), the same "one identity, no artificial split"
  principle `node-id` already embodies everywhere else in this project.
  `keel-controlplane` itself is issued a certificate the identical way,
  under its own name (e.g. `controlplane`) rather than a node id, since it
  needs the same dual-use server-and-client capability: server when a
  node or `keelctl` connects to it, client when it forwards to a node.
- One certificate per human operator running `keelctl` in
  control-plane-routed mode.
- A new `scripts/gen-certs.sh`, alongside the existing
  `scripts/smoke-test.sh`, shells out to `openssl req`/`openssl x509`
  (the same "shell out to the real command" idiom `keel-jail`/`keel-zfs`
  already use for `jail(8)`/`zfs(8)`) to generate the CA once and issue
  named leaf certificates on demand.
- Milestone 11's shared-secret mechanism, the `auth` modules in both
  crates, `--auth-token-file`, and `keel-agentd`'s `route`/
  `route_authenticated` split, is removed entirely, superseded by
  certificate-based identity enforced at the transport layer.

## Non-Goals (Milestone 12)

- **No revocation (CRL/OCSP).** Revoking one compromised identity still
  means regenerating the CA and re-issuing/redistributing every
  certificate, the same all-or-nothing rotation cost Milestone 11's token
  already had, just scoped per-identity now instead of cluster-wide. A
  real, stated limitation, not solved here; a CRL/OCSP mechanism is
  substantial enough to deserve its own future milestone if it's ever
  needed.
- **No differentiated authorization by identity.** A valid certificate
  from any node or any operator gets exactly the same access as any
  other, matching today's uniform-access model exactly. Per-identity
  certificates exist for naming, audit, and a future revocation point,
  not to gate which routes a given identity may call.
- **No certificate rotation or renewal automation.** Rotating a
  certificate is a manual operator procedure. Certificates are issued
  with a 10-year validity specifically so this isn't a near-term concern;
  expiry monitoring/alerting is not built.
- **No async runtime.** `rustls`'s blocking API is used directly against
  `std::net::TcpStream`, the same threading model every prior milestone
  has used; no `tokio` or other async runtime is introduced.
- **`keel-agentd`'s Unix socket is completely unaffected.** No TLS, no
  certificates; its `0600`-permission trust boundary, and the fact that
  `route()` itself needs no auth-awareness at all after this milestone
  (see Architecture), leaves the local API exactly as simple as it was
  before Milestone 11 ever touched it.
- **No new dependency beyond what TLS itself requires.** `rustls` and
  `rustls-pemfile` are the only additions; certificate *generation* stays
  entirely in a shell script, not a new Rust dependency like `rcgen`.

## Architecture

### Crate choice: `rustls` + `rustls-pemfile`

`rustls` is the clear right choice: pure Rust, no FFI/linking against a
system OpenSSL (which `native-tls` would require on FreeBSD), actively
maintained, and it exposes a blocking, `Read`/`Write`-compatible stream
type that layers directly on top of `std::net::TcpStream` with no async
runtime needed. `rustls-pemfile` is its standard, minimal companion for
parsing PEM-encoded certificates and keys into the DER form `rustls`
needs. Both are well short of "hand-rolling crypto," the one place in
this project where pulling in real, audited code is the responsible
choice rather than a shortcut.

One dependency detail matters enough to state explicitly, since it
bears directly on the "no FFI" reasoning above: `rustls` 0.23 made its
crypto backend pluggable, and `aws-lc-rs`, a vendored C/assembly
library, not Rust, is pulled in by default unless default features are
turned off. Left as-is, that would quietly undercut this section's own
argument and add a C-toolchain build requirement this project has never
had. `Cargo.toml` pins `rustls` with `default-features = false` and the
`ring` feature instead: `ring` is the backend `rustls` used exclusively
before 0.23, is Rust plus hand-written assembly with no external C
library and no `cc`/`cmake` build step, and has years of track record on
BSDs. Because the crypto provider is no longer installed implicitly,
`keel-controlplane`, `keel-agentd`, and `keelctl` each call
`rustls::crypto::ring::default_provider().install_default()` once at
process startup, before constructing any `ServerConfig` or
`ClientConfig`; `rustls` panics if one is built with no provider
installed, so this is a required line in each binary's startup
sequence, not optional hardening.

### `scripts/gen-certs.sh`: CA and identity issuance

```bash
./scripts/gen-certs.sh init                                   # generates ca.crt / ca.key once
./scripts/gen-certs.sh node node-4 192.168.64.4                # issues node-4.crt/.key, SAN=192.168.64.4
./scripts/gen-certs.sh node controlplane 192.168.64.2          # issues controlplane.crt/.key, same as a node
./scripts/gen-certs.sh client alice                            # issues alice.crt/.key
./scripts/gen-certs.sh node node-4 192.168.64.4 --days 36500   # optional override, used only for test fixtures
```

Every leaf certificate is signed by `ca.key` and gets both `serverAuth`
and `clientAuth` extended key usage, since a node's (and the control
plane's own) single identity certificate is used both ways: server when
accepting an inbound connection, client when dialing out. Validity
defaults to 10 years (`--days 3650`, matching `openssl`'s own flag
name), overridable with a trailing `--days N`; the only caller that
overrides it is this milestone's own fixture generation (see Testing
Strategy), which asks for `--days 36500` (~100 years) so `testdata/tls/`
never needs regenerating as the suite ages. The `node` subcommand issues
that dual-use certificate for anything that plays both roles, a node or
the control plane itself, distinguished only by the name and address
given to it; `client` certificates only ever dial out, so they only
exercise the `clientAuth` half, but the script does not special-case
that.

### Server side: `keel-controlplane::http::run` and `keel-agentd::http::run_tcp`

Both gain a `rustls::ServerConfig`, built once at startup from `ca.crt`
(as the client-certificate-verifying root store) plus the node's or
control plane's own certificate and key, with client authentication set
to *required*. Each accepted `TcpStream` is wrapped in a
`rustls::StreamOwned<ServerConnection, TcpStream>` before anything else
happens:

```rust
pub fn run(listener: TcpListener, commands: Sender<Command>, tls_config: Arc<rustls::ServerConfig>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = Arc::clone(&tls_config);
        thread::spawn(move || {
            let Ok(conn) = rustls::ServerConnection::new(tls_config) else { return };
            let mut tls_stream = rustls::StreamOwned::new(conn, stream);
            let _ = handle_connection(&mut tls_stream, &commands);
        });
    }
}
```

A connection that doesn't present a client certificate, or presents one
signed by a different key, fails during `StreamOwned`'s first read/write
(which drives the handshake) with an `io::Error`; `handle_connection`
never runs. This is a strictly stronger property than Milestone 11's
`401`: an unauthenticated caller learns nothing beyond "the connection
closed," not even that a real service is listening. `route()` itself
needs **no auth-awareness at all** after this milestone, since a call
only ever reaches it once the transport layer has already verified
identity: this is where Milestone 11's `auth::check` call and
`keel-agentd`'s `route`/`route_authenticated` split disappear.
`keel-agentd` goes back to one `route()`, called by both the Unix path
(unauthenticated, filesystem-permission-gated, exactly as it always was)
and the now-TLS-wrapped TCP path (authenticated by the handshake, not by
anything inside `route()`).

### Client side: `forward()`, `registration.rs`, `keelctl`

Each of this project's three outbound call sites
(`keel-controlplane::http::forward`,
`keel-agentd::registration::send_request`,
`keelctl::send_request_tcp`) gains a `rustls::ClientConfig` (root store =
`ca.crt`, client certificate+key = its own identity) and wraps its
`TcpStream::connect(...)` result the same way, via
`rustls::StreamOwned<ClientConnection, TcpStream>`, before writing the
hand-built HTTP request into it exactly as today. `rustls::ClientConfig`
verifies the server's presented certificate against `ca.crt` and checks
its SAN against the dialed address using `rustls`'s own hostname/IP
verification; no new verification code is needed for that check.

One piece of glue every one of these three call sites needs that none of
them have today: `ClientConnection::new` takes a
`rustls::pki_types::ServerName`, not a socket address string, while the
addresses actually in hand
(`registration.rs`'s/`keelctl`'s `control_plane_addr`, `forward()`'s
per-node `addr`) are `host:port` strings, and every certificate's SAN is
issued against the bare host (`gen-certs.sh`'s own examples pass
`192.168.64.4`, not `192.168.64.4:7621`). Each call site splits off the
port and parses the remaining host into a `ServerName::IpAddress`, every
address in this project is a literal IP, never a DNS name, so no
resolution path is needed, and passes that alongside the config to
`ClientConnection::new`. This is a few lines of new plumbing per call
site, not new verification logic: `rustls` still does the actual SAN
comparison once it has the right `ServerName` to compare against.

### What gets removed

- `keel-controlplane/src/auth.rs` and `keel-agentd/src/auth.rs` (the
  whole modules: `load_token`, `check`, `constant_time_eq`), deleted
  outright, not deprecated. No compatibility shim, consistent with every
  prior milestone's wire-format changes in this project.
- `--auth-token-file` removed from all three binaries' flag parsing,
  replaced by `--tls-ca-file`, `--tls-cert-file`, `--tls-key-file`
  (three flags rather than one), required together in exactly the place
  `--auth-token-file` was required: unconditionally on
  `keel-controlplane`, alongside the existing control-plane flags on
  `keel-agentd`/`keelctl`.
- `keel-agentd`'s `route_authenticated`/`route` split, collapsed back to
  one `route()`.
- The `Authorization: Bearer <token>` header line, removed from every
  hand-built request in the codebase.

## Error Handling

- A TLS handshake failure (missing client certificate, a certificate
  signed by a different CA, an expired certificate, a SAN mismatch when
  dialing out) surfaces as a plain `io::Error` from the first read/write
  on the wrapped stream, and the connection is simply closed; no HTTP
  response is ever sent, since there is no valid transport to send one
  over.
- A one-line `eprintln!` on handshake failure (server side, in
  `handle_connection`/`handle_connection_tcp`'s error branch) is the only
  operational visibility this milestone adds, matching this project's
  existing minimal, no-logging-framework convention. It does not attempt
  to identify the caller, since a failed handshake often hasn't
  progressed far enough to know who the peer claims to be.
- Startup certificate/key/CA loading failures (missing file, malformed
  PEM, mismatched key) are `panic!` on `keel-controlplane`/`keel-agentd`,
  a graceful `Err`/`ExitCode::FAILURE` on `keelctl`, the exact fail-fast
  convention Milestone 11 already established for the token file.
- Outbound connect/handshake failures (a forwarding target that's down,
  or presents an unexpected certificate) reuse the existing
  `FORWARD_CONNECT_TIMEOUT`/`FORWARD_READ_TIMEOUT` machinery unchanged,
  since those timeouts are set on the underlying `TcpStream` before it is
  wrapped in TLS, and the handshake's own reads/writes go through that
  same, already-timed-out socket. No new timeout mechanism is needed.
- Certificates are issued with a 10-year validity by `gen-certs.sh`,
  since this milestone has no rotation story; expiry is a real but
  distant concern, tracked by the operator, not by any code this
  milestone adds.

## Testing Strategy

- A small set of fixture certificates (`testdata/tls/`: `ca.crt`, one
  node certificate+key, one client certificate+key, all generated once
  via `gen-certs.sh` with a ~100-year validity since they are test-only
  and expiry is irrelevant) are committed to the repository and shared
  across `keel-controlplane`, `keel-agentd`, and `keelctl`'s test suites,
  so all three interoperate against the same trust root in tests exactly
  as they will in production.
- Certificate/key loading: unit tests for valid PEM, a missing file, and
  malformed PEM, in whichever module owns loading, mirroring Milestone
  11's `load_token` tests.
- `keel-controlplane`/`keel-agentd` server wiring: integration tests
  binding a real local `TcpListener`, wrapping it in a real
  `rustls::ServerConfig` built from the fixtures, and driving real client
  connections: a client presenting the correct fixture certificate
  succeeds through to a normal HTTP response; a client presenting no
  certificate, or one signed by a throwaway *different* CA generated ad
  hoc in the test, fails the handshake and the connection closes with no
  response.
- Outbound wiring (`forward()`, `registration.rs`, `keelctl`): the
  equivalent test in the other direction, a fake TLS-wrapped server
  (fixtures) receiving a real outbound connection, confirming the client
  certificate is actually presented and the server certificate is
  actually verified (a fake server presenting a certificate from the
  wrong CA must cause the outbound call to fail, not silently proceed).
- VM verification (the real proof, same discipline as every prior
  milestone), across the three-node setup: run `gen-certs.sh init` once,
  issue `node-2`/`node-4`/`node-5` certificates (SANs matching their real
  IPs) and one operator certificate, distribute each to its host; confirm
  the exact Milestone 10/11 apply/get/delete-through-the-scheduler round
  trip works unchanged over real mTLS; confirm a `keelctl` call with no
  certificate, or a certificate from a freshly generated *second*,
  unrelated CA, is rejected; confirm a node started with the wrong node's
  certificate (SAN mismatch) fails to register.

## Open Questions / Deferred Decisions

- Revocation (CRL/OCSP) remains out of scope; the only revocation story
  today is regenerating the CA and redistributing everything.
- Certificate rotation/renewal automation is manual, the same story
  Milestone 11's token rotation already had.
- Per-identity authorization (different certificates granted different
  access) is unaddressed; every valid identity has uniform access,
  matching today's model exactly.
- No expiry monitoring or alerting exists; a 10-year validity defers the
  problem rather than solving it.
- Whether handshake failures should eventually get richer diagnostics
  (for example, logging a presented certificate's CN when the handshake
  gets far enough to see one) is left open, deferred until this project
  has any logging framework at all, a pre-existing gap this milestone
  does not attempt to close.
