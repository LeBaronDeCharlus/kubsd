# Milestone 21: Ingress and Automatic HTTPS (Sub-Project 9, First Milestone)

Status: Approved, not yet implemented
Date: 2026-07-22

## Context

Every workload primitive Keel has today (`Jail`, `Service`) lives entirely
on private VNET networking: static or VIP addresses on a `keel0`-style
bridge, reachable only from other jails on the same bridge or, since
Milestone 14, across nodes via the deterministic per-node subnet routing.
There is no path from the public internet to a jail at all. The
motivating use case for this milestone is concrete: serve a Hugo static
site and a self-hosted Umami analytics instance, both as ordinary `kind:
Service` specs, reachable over HTTPS on a real domain, with Umami kept
self-hosted rather than sending visitor data to a third party.

That reachability gap is a missing subsystem, not a config detail: Keel
needs a way to terminate TLS for a real domain and route inbound traffic
to a backend `Service`, with certificates issued and renewed without
manual intervention. This milestone adds exactly that, as the first
milestone of a new sub-project, the same way Milestone 2 introduced
`keel-jail`/`keel-zfs` before `keel-net` added networking two milestones
later.

The target deployment is a **single node** with a real public IP (a VPS),
not the multi-node cluster shape Milestones 14-19 exercise. Cross-node
ingress routing (an `Ingress` on one node fronting a `Service` whose
replicas are scheduled on another) is explicitly deferred; see Non-Goals.

The domain's DNS is hosted at OVH, which is why the ACME automation in
this milestone targets OVH's DNS API specifically rather than a
provider-agnostic plugin system.

## Goals

- A new `kind: Ingress` spec in `keel-spec`:

  ```yaml
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
  ```

  `host` is validated as a syntactically well-formed DNS name;
  `backend.service` is validated at apply time against the set of
  currently-known `Service` names (an `Ingress` cannot reference a
  `Service` that doesn't exist); `tls.email` is validated as a
  syntactically well-formed email address (the ACME account contact).

- A new crate, `keel-ingress`, structured like every other runtime crate
  in this project (`keel-jail`, `keel-net`, `keel-zfs`): traits plus a
  `Fake` implementation for fast cross-platform tests, plus a real
  implementation for the FreeBSD/production path.

  - `DnsProvider` trait: `create_txt_record`, `delete_txt_record`,
    `wait_for_propagation`. `FakeDnsProvider` is in-memory.
    `OvhDnsProvider` is the real implementation, calling OVH's signed
    REST API.
  - `AcmeClient` trait: `request_certificate(domain, &dyn DnsProvider) ->
    Result<Cert, AcmeError>`. `FakeAcmeClient` returns a self-signed
    dummy certificate instantly. The real implementation is built on the
    `instant-acme` crate, performing a DNS-01 challenge against Let's
    Encrypt.

- `keel-agentd` gains a reconciliation path for `Ingress` specs:

  - Ensures a single system-managed jail, name-prefixed like every other
    Keel-owned resource (e.g. `keel-ingress`), exists, VNET-attached to
    the node's internal bridge, running nginx.
  - Recomputes nginx's configuration as the union of every
    currently-applied `Ingress` spec: one `server` block per `host`,
    each proxying to its backend `Service`'s VIP:port (the existing
    Milestone 16 per-node proxy already listens there and relays to a
    replica).
  - Tracks each `Ingress`'s certificate expiry in the existing crash-safe
    state store (same temp-file-plus-rename pattern as `BackoffState`),
    and re-runs the ACME flow when a certificate is within 30 days of
    expiring.
  - Validates nginx's config (`nginx -t`, via `jexec`) before reloading;
    only reloads on success.

- Host-level `pf` redirect rules (applied once, not per-`Ingress`): the
  node's public IP's ports 80 and 443 forward to the ingress jail's
  bridge address. Port 80 exists for a plain HTTP-to-HTTPS redirect, not
  for ACME HTTP-01 (this milestone uses DNS-01 exclusively; see below).

- Real verification on the existing dev VM, using the real domain and a
  real OVH account: full ACME order → DNS-01 challenge via OVH → Let's
  Encrypt **staging** certificate issued → nginx serving it → a shortened
  renewal-threshold check exercising the renewal path. One additional
  pass against Let's Encrypt's **production** directory, confirming a
  real production cert issues cleanly, before this milestone is called
  done.

## Non-Goals

- **Building or publishing the Hugo/Umami images.** `spec.image` already
  just names a pre-existing ZFS base dataset; preparing that dataset
  (nginx plus the built Hugo output, or Node plus Umami plus Postgres) is
  a manual, out-of-band step, exactly like every other base image in
  this project today. This milestone's real verification uses a minimal
  test backend (any `Service` returning a distinguishable HTTP response
  is enough to prove routing works), not the actual Hugo/Umami images.
- **A generic `Secret` kind.** The OVH API credentials (application key,
  application secret, consumer key) are node-level `keel-agentd` daemon
  config (e.g. `/usr/local/etc/keel/dns-ovh.toml`), read once at startup,
  not part of any YAML spec. This keeps credentials out of specs that
  might be committed to git without building a general secrets
  subsystem for a single credential.
- **Routing directly to a bare `Jail`.** `Ingress.spec.backend` only ever
  names a `kind: Service`, even a `replicas: 1` one. Services already
  have a stable VIP that survives rescheduling; `Ingress` never needs to
  track a raw jail address that can change.
- **Multi-node ingress routing.** This design assumes the ingress jail
  and every `Ingress`'s backend `Service` live on the same node. Fronting
  a `Service` whose replicas are scheduled on a different node than the
  ingress jail is deferred to a later milestone, if the deployment shape
  ever grows beyond a single node.
- **DNS providers other than OVH.** The `DnsProvider` trait is the seam
  a second provider would implement, but no second implementation is
  built now.
- **ACME HTTP-01 challenges.** DNS-01 is sufficient (it doesn't require
  the node to be publicly reachable to issue a certificate, only to
  serve one afterward) and is the only challenge type this milestone
  implements.
- **Wildcard certificates, multiple domains per `Ingress`, path-based
  routing, or rate-limit-aware request queuing.** None are needed for
  the motivating use case; all deferred until something needs them.
- **Automatic `pf` rule generation across arbitrary network topologies.**
  The `pf` rules this milestone writes assume exactly one public
  interface and one internal bridge, matching the single-node target.

## Architecture

### `keel-spec`: `IngressSpec`

```rust
pub struct IngressSpec {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: IngressSpecBody,
}

pub struct IngressSpecBody {
    pub host: String,
    pub backend: IngressBackend,
    pub tls: IngressTls,
}

pub struct IngressBackend {
    pub service: String,
    pub port: u16,
}

pub struct IngressTls {
    pub email: String,
}
```

`parse_and_validate_ingress` rejects a syntactically invalid `host`, a
`backend.service` that isn't a currently-known `Service` name, an
invalid `tls.email`, and `backend.port: 0`, mirroring the existing
validation style for `cpu`/`memory` strings and `Service`'s `port` field.

### `keel-ingress`: DNS-01 and ACME

```rust
pub trait DnsProvider {
    fn create_txt_record(&self, name: &str, value: &str) -> Result<(), DnsError>;
    fn delete_txt_record(&self, name: &str) -> Result<(), DnsError>;
    fn wait_for_propagation(&self, name: &str, value: &str) -> Result<(), DnsError>;
}

pub trait AcmeClient {
    fn request_certificate(
        &self,
        domain: &str,
        contact_email: &str,
        dns: &dyn DnsProvider,
    ) -> Result<Cert, AcmeError>;
}
```

`OvhDnsProvider` implements `DnsProvider` against OVH's REST API
(`POST/DELETE /domain/zone/{zone}/record`, followed by
`POST /domain/zone/{zone}/refresh`), using OVH's request-signing scheme
(application key/secret/consumer key plus a SHA1 signature over
method+URL+body+timestamp). `wait_for_propagation` polls public DNS
resolvers directly (not OVH's API) until the TXT record is visible,
bounded by a timeout.

The real `AcmeClient` wraps `instant-acme`: create an account (or reuse
one persisted from a prior run), place an order for `domain`, fetch the
DNS-01 challenge token, call `dns.create_txt_record`, call
`dns.wait_for_propagation`, tell the ACME server the challenge is ready,
poll until the order is valid, finalize, download the certificate chain,
then call `dns.delete_txt_record` to clean up. The directory URL (Let's
Encrypt staging vs. production) is `keel-agentd` daemon config, not part
of the `Ingress` spec — staging is used for iteration, production for
the final verification pass and for real traffic.

### `keel-agentd`: the ingress reconciler

A new reconciliation path, parallel to the existing per-jail
`Reconciler` and the Milestone 16 proxy manager, but shaped differently:
where `Jail`/`Service` reconciliation is per-name, `Ingress`
reconciliation is a **singleton jail whose configuration is a function
of every currently-applied `Ingress` spec together**.

On each reconcile pass:

1. Ensure the `keel-ingress` jail exists (VNET, attached to the internal
   bridge), provisioning it the same way any other Keel-managed jail is
   provisioned, with automatic rollback on partial failure (reusing the
   existing provisioning path from Milestone 4/17).
2. For each `Ingress` spec, check the stored certificate expiry. If
   there is no certificate yet, or fewer than 30 days remain, run the
   ACME flow (with the existing `BackoffState` pattern on failure — a
   failed renewal doesn't block any other `Ingress`'s reconciliation,
   matching the "one broken jail never blocks the others" principle
   established in Milestone 4). On success, write the cert/key into the
   ingress jail's certificate directory and persist the new expiry.
3. Recompute nginx's configuration as the union of every `Ingress`
   spec's `server` block (`host` → `proxy_pass` to the backend
   `Service`'s VIP:port). Write it to a temp file inside the jail and
   rename over the live config (same crash-safe pattern used for
   `keel-agentd`'s own state store).
4. Validate with `nginx -t` (via `jexec`). On success, reload
   (`nginx -s reload`). On failure, log the error and leave the
   previously-valid config and running nginx process untouched — a bad
   new `Ingress` spec never takes down vhosts that were already working.

Host-level `pf` rules (`rdr` from the public IP's 80/443 to the ingress
jail's bridge address) are applied once, independent of how many
`Ingress` specs exist, and reconciled on their own schedule with the
same backoff-and-retry treatment as everything else — a `pf` failure
doesn't block per-`Ingress` cert/config reconciliation, since it's a
separate concern.

### Traffic path

```
browser --443--> node public IP --pf rdr--> keel-ingress jail (nginx,
  SNI picks the host's server block, terminates TLS with that host's
  cert) --proxy_pass, internal bridge--> backend Service's VIP:port
  (Milestone 16 per-node proxy) --round-robin relay--> a replica jail
```

### Cert issuance path

```
reconciler notices expiry < 30d or missing --> AcmeClient::request_certificate
  --> ACME order created --> DNS-01 challenge token --> DnsProvider::create_txt_record
  (OVH API) --> wait_for_propagation --> Let's Encrypt resolves the public TXT
  record (no inbound reachability to the node required) --> certificate issued
  --> written into the ingress jail --> DnsProvider::delete_txt_record cleanup
  --> nginx config recomputed --> nginx -t --> nginx -s reload
```

## Error Handling

- **OVH API error or DNS propagation timeout during a challenge:** retry
  with the existing `BackoffState` pattern (starts at 1s, doubles to a
  5-minute cap); the previously-issued certificate, if any, keeps
  serving in the meantime — a renewal attempt failing never tears down
  a vhost that was working.
- **ACME rate limiting:** avoided during development by pointing the
  real `AcmeClient` at Let's Encrypt's staging directory (daemon config);
  production is used only for the final verification pass.
- **nginx config write succeeds, `nginx -t` validation fails:** no
  reload; error logged; existing vhosts continue serving the last-valid
  config.
- **Ingress jail provisioning fails outright:** the existing
  provisioning rollback path applies — no half-created system jail is
  left behind.
- **`pf` rule application fails:** retried each reconcile pass with the
  same backoff pattern, independent of per-`Ingress` reconciliation.
- **Crash safety:** per-`Ingress` state (expiry timestamp, backoff state)
  is persisted via the existing crash-safe state store, so a
  `keel-agentd` restart mid-renewal doesn't lose track of what it was
  doing, consistent with every other piece of Keel state.

## Testing Strategy

- **Unit/fake tests** (fast, no FreeBSD, run anywhere): `IngressSpec`
  parsing/validation in `keel-spec`; nginx config templating (merged
  multi-host config from N `Ingress` specs); `DnsProvider`/`AcmeClient`
  behavior against their `Fake` implementations, including
  error-injection for the retry/backoff paths; reconciler behavior (one
  failing `Ingress` doesn't block others; the 30-day renewal threshold;
  crash-safe state round-trip across a simulated restart).
- **Real verification on the existing dev VM:** unlike the bhyve case
  (Milestone 20), DNS-01 does not require the node to be publicly
  reachable — Let's Encrypt only needs to resolve a public TXT record,
  and the dev VM only needs outbound internet access to reach OVH's API
  and Let's Encrypt's ACME endpoint. The full certificate lifecycle is
  therefore verifiable on the dev VM, using the real domain and real
  OVH account: a real ACME order, a real DNS-01 challenge served via
  OVH, a real Let's Encrypt **staging** certificate issued and served by
  nginx, and the renewal path exercised with an artificially shortened
  threshold. One additional real-production-directory issuance is run
  before this milestone is considered done.
- **Explicitly not verifiable until the VPS move** (called out, not
  assumed away): the `pf` redirect from an actual public IP, and a real
  browser reaching the site over the internet. This is tracked as a
  lightweight post-move smoke-test checklist, not a blocking task in
  this milestone's implementation plan, since the VPS doesn't exist yet.

## Open Questions / Deferred Decisions

- The exact `instant-acme` account-persistence strategy (where the ACME
  account key is stored so a `keel-agentd` restart doesn't create a new
  account on every run) is left to implementation-time discovery,
  similar to how Milestone 20 left bhyve process-tracking details open.
- Whether nginx's config needs per-host rate limiting or connection
  caps out of the box, or whether that's deferred until real traffic
  shows a need, is left open — leaning toward deferring it.
- The OVH zone name is not necessarily identical to `Ingress.spec.host`
  (a host can be a subdomain of a zone registered separately). Whether
  `OvhDnsProvider` derives the zone from the host automatically or takes
  it as additional daemon config is left to implementation-time
  discovery against the real OVH account.
