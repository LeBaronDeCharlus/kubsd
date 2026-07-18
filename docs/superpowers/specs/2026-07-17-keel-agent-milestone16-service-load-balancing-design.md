# Milestone 16: Service Load Balancing via a Per-Node Virtual-IP Proxy (Sub-Project 6, Second Milestone)

Status: Approved
Date: 2026-07-17

## Context

Milestone 15 built the replica-set half of "service discovery and load
balancing": a `kind: Service` spec produces N deterministically-named jail
replicas, spread across nodes, and `GET /services/<name>` reports which of
them are currently healthy. Its own Non-Goals named exactly what it left
undone: "nothing in this milestone routes a request across replicas
automatically... building that mechanism (a proxy, DNS-based round robin,
client-side load balancing) is a distinct, later milestone in this
sub-project." This is that milestone.

Concretely, today a caller that wants to reach a healthy instance of a
service has to call `GET /services/<name>` itself and pick one. This
milestone removes that step: a caller connects to one stable address for
the service and is transparently forwarded to a currently-healthy replica,
the same job Kubernetes' Service/kube-proxy mechanism does, adapted to
FreeBSD jails, VNET bridges, and this project's existing hand-rolled,
dependency-light idioms.

## Goals

- `kind: Service`'s spec gains one new field, `spec.port: u16`: the port
  the proxy listens on for this service, and the port it forwards to on
  each replica (same port both sides, no separate "target port").
- `keel-controlplane` gains a new `--service-cidr` flag (a cluster-wide
  virtual-IP pool, entirely distinct from any node's own Milestone-14
  `pod_cidr`) and allocates one stable VIP per service, derived
  deterministically from the service name via a new function structurally
  analogous to Milestone 14's `derive_pod_cidr` (same FNV-1a-hash-then-
  probe shape, and it reuses that function's private hash helper
  directly) but operating at host-address granularity within
  `service_cidr`, not at `derive_pod_cidr`'s hardcoded /24-block
  granularity within `cluster_cidr` — `derive_pod_cidr` itself cannot be
  reused as-is here (see Architecture for why). Collision handling is new,
  not reused: a colliding VIP candidate is resolved by linear probing (see
  Architecture), unlike a colliding `pod_cidr`, which is an outright
  registration rejection.
- The existing 5-second heartbeat is extended: its response (today an
  empty `200`) now carries every known service's `{name, vip, port,
  replicas}`, computed from the exact same `Alive`+`running` filter
  `GET /services/<name>` already uses.
- Every node's `keel-agentd`, on each heartbeat round-trip, keeps its own
  view of every service in sync: the VIP aliased on that node's own
  `keel0` bridge as a second address alongside its existing Milestone-14
  gateway address, and a TCP relay listener bound to
  `<vip>:<port>` forwarding each accepted connection to a currently
  healthy replica, retrying once against a different replica if the first
  connect attempt fails.
- A caller jail, anywhere in the cluster, connects to `<service-vip>:
  <port>` and is transparently relayed to a healthy replica, with zero
  caller-side changes, zero knowledge of which node it's actually
  talking to.
- An operator can discover a service's VIP and port without inspecting
  any node directly: `GET /services` gains `vip`/`port` fields, the only
  human-facing surface this milestone adds for the VIP (the heartbeat
  body carrying it to `keel-agentd` is internal, control-plane-to-node
  traffic). Reaching it today means a direct HTTP call against the
  control plane, not `keelctl`: `keelctl get NAME` only ever resolves a
  single jail or service by name (`/jails/NAME` falling back to
  `/services/NAME`, per Milestone 15), never the bare collection —
  Milestone 15 added `GET /services` at the HTTP layer without giving
  `keelctl` a verb for it, and this milestone doesn't change that.

## Non-Goals

- **External/outside-cluster reachability.** The VIP is reachable only
  from other jails/nodes inside the cluster, the exact boundary
  Milestone 14's `pod_cidr` already has (no NAT or ingress from outside
  exists anywhere in this project). A NodePort/LoadBalancer-equivalent is
  a distinct, later milestone if ever needed.
- **L7/HTTP-aware routing.** The proxy is a pure L4 TCP byte relay; it
  never inspects, parses, or terminates whatever protocol is inside the
  connection, including TLS (passthrough only, never termination).
  Path/header-based routing is a separate Ingress-style concept, not
  built here.
- **Session affinity.** Each new TCP connection picks a healthy replica
  independently; there is no client-IP stickiness, matching a Kubernetes
  Service's own default (non-`sessionAffinity: ClientIP`) behavior.
- **Rebalancing already-established connections.** A replica-list change
  (scale up/down, a replica dying, a fresh reconcile) only affects new
  connections; existing ones are never migrated or forcibly dropped.
- **Any new health-checking.** The proxy reuses exactly the `Alive`-node-
  plus-`running`-jail definition `GET /services/<name>` already computes.
  No new active probing (no HTTP health-check endpoint, no additional TCP
  pre-check beyond the connect-and-retry-once behavior below).
- **VIP persistence across a control-plane restart.** Matches `Services`'
  own existing non-persistence (Milestone 15): a restart forgets every
  service definition and its VIP alike. Because VIP derivation is
  deterministic, re-applying the same service name after a restart
  happens to recover the same VIP, but this is a byproduct of the
  derivation, not a guarantee this milestone is designed to provide.
- **Any change to the scheduler, `pod_cidr` addressing, or discovery's own
  filter.** All of Milestone 15's machinery is reused completely
  unchanged; this milestone is additive on top of it.
- **A load-balancing algorithm more sophisticated than round-robin.**
  Least-connections, weighted, or latency-aware selection are all
  deferred; round-robin is simple, fair, deterministic, and easy to test.

## Architecture

### `keel-spec`: the new `port` field

`ServiceSpecBody` (Milestone 15) gains one new field, a sibling of
`replicas`/`template`, not a part of `JailTemplate` itself since it
describes the service as a whole, not any one replica's jail:

```rust
pub struct ServiceSpecBody {
    pub replicas: u32,
    pub port: u16,
    pub template: JailTemplate,
}
```

`parse_and_validate_service` rejects `port: 0` the same way it already
rejects an invalid `cpu`/`memory` string. The port is never written into a
replica's own `JailSpec` (`JailTemplate::to_jail_spec` is unchanged) — a
replica jail doesn't need to know it's being proxied at all; `spec.port`
is purely the contract the operator declares ("this service's app listens
on this port") that the proxy relies on.

### `keel-controlplane`: VIP allocation and carrying the port through

`Services` (Milestone 15) gains a new construction-time input,
`service_cidr: Ipv4Net`, threaded through `worker::spawn` the same way
`Registry::new` already takes `cluster_cidr`. `ServiceRecord` gains two
fields: `vip: Ipv4Addr`, populated once, on first creation of a service
(not on a scale-only re-apply, the same "template is otherwise immutable"
precedent Milestone 15 already established for everything else about a
service's identity), and `port: u16`, populated the same way — set once
on first creation, then preserved on every later re-apply regardless of
what `spec.port` says at the time. `port` is as much a part of a
service's identity as `template` already is (Goals: "the port ... it
forwards to on each replica"), so `Services::apply` rejects a `port`
change on an already-existing service exactly like it already rejects a
`template` change, via a new `ApplyServiceError::PortChanged` variant
alongside the existing `TemplateChanged` — silently accepting a changed
port on a routine scale-up/down would otherwise let a typo in a re-applied
spec retarget a live VIP's listener, a case none of this milestone's
proxy-manager machinery is designed to handle. `Command::ApplyService`
gains `port: u16` as a new parameter (alongside the existing
name/replicas/template), and `handle_apply_service` (`http.rs`) passes
`spec.spec.port` through to it.
`wire::ServiceSummary` (Milestone 15's `GET /services` per-service
listing type) gains the same two fields, sourced directly from
`ServiceRecord` at the `Command::ListServices` call site
(`worker.rs`'s `services.list()` map, alongside the existing
`desired_replicas`) — this is the one operator-facing place this
milestone exposes a VIP; `DiscoverService`/`GET /services/<name>`'s
existing `Vec<ServiceReplica>` response shape is untouched (see
Non-Goals: discovery's own filter and response shape are out of scope).

VIP derivation cannot literally reuse `derive_pod_cidr`: that function
hardcodes `POD_PREFIX_LEN = 24` and always returns a whole `/24`-aligned
`Ipv4Net` block (its hash picks a *block index* within `cluster_cidr`,
never a host address, and the returned address always ends in `.0`). Given
a `service_cidr` no larger than a `/24` — this document's own worked
example uses exactly `10.0.250.0/24` — `derive_pod_cidr`'s block count
would be `1 << (24 - 24) = 1`, so every service name would hash to the
identical single candidate `10.0.250.0`, not a spread of host addresses;
that's visibly inconsistent with the Data Flow example below, which
derives the host address `10.0.250.7` (not the block address
`10.0.250.0`) for service "web". Instead, `subnet.rs` gains a new
function, `derive_service_vip(service_name: &str, service_cidr: &Ipv4Net)
-> Ipv4Addr`, that reuses only `derive_pod_cidr`'s private `fnv1a` hash
helper directly (same file, so no visibility change needed), but at
host-address granularity: hash the service name modulo the number of
usable host addresses in `service_cidr` (`1 << (32 -
service_cidr.prefix_len())`), and add that offset to `service_cidr`'s
network address to get the candidate `Ipv4Addr`. Collision *resolution*
is new behavior on top of that, not a reuse of an existing pattern:
Milestone 14 itself does not probe on a `pod_cidr` collision, it rejects
the colliding node's registration outright (`Registry::register`'s
`PodCidrCollision` error). A service has no analogous "pick a different
id and retry" escape hatch available to a caller, so this milestone
instead linearly probes forward host-by-host from the candidate (bounded
by `service_cidr`'s actual host-address count) until a free address is
found or the pool is exhausted (a hard `apply`-time error in the latter
case — see Error Handling).

### Wire format: the heartbeat response gains a body

Every heartbeat before this milestone gets a `(200, empty body)` response.
This is the first time that response carries data. New wire type:

```rust
pub struct ServiceProxyEntry {
    pub name: String,
    pub vip: String,
    pub port: u16,
    pub replicas: Vec<ServiceReplica>, // reuses Milestone 15's type verbatim
}
```

`handle_heartbeat`'s success path becomes `(200, yaml_response(&entries))`,
one `ServiceProxyEntry` per known service, `replicas` computed with the
identical `Alive`+`running` filter `DiscoverService`/`GET /services/<name>`
already uses (so the two never drift). `keel-agentd`'s `registration.rs`,
previously only checking the heartbeat response's status code, now parses
and acts on its body.

### `keel-net`: generalized alias management

Two new `NetManager` methods, idempotent like every existing one:

```rust
fn add_alias(&self, bridge: &str, address: &str) -> Result<(), NetError>;
fn remove_alias(&self, bridge: &str, address: &str) -> Result<(), NetError>;
```

implemented via `ifconfig <bridge> alias`/`-alias`. Milestone 14's
`bridge_gateway` sets a bridge's *first* address with a plain `ifconfig
<bridge> inet <gateway>` (no `alias` keyword, since it's the only address
the bridge has ever needed); `add_alias` is what actually introduces the
`alias` keyword to this project, needed here because FreeBSD requires it
to add a *second* address (the VIP) to an interface without replacing the
first (the gateway). `FakeNetManager` gains an in-memory equivalent.

### `keel-agentd`: the proxy manager

A new module owns, per known service, one bound `TcpListener` and a
shared, atomically-swappable list of current replica socket addresses.
Each accepted connection is handled on its own thread: pick the next
replica in round-robin rotation (a simple atomic counter per service),
attempt to connect, and on success relay bytes bidirectionally (two
directions of `std::io::copy` over cloned `TcpStream` halves) until either
side closes. A failed connect attempt retries the rotation's next replica
exactly once before giving up and closing the incoming connection.

On every heartbeat round-trip, the manager diffs the desired service set
(from the response body) against what it's currently running:

- **New service:** `add_alias(bridge, vip)`, then bind and spawn a
  listener on `<vip>:<port>`.
- **Known service:** swap in the fresh replica list; already-accepted
  connections are unaffected, only connections accepted after the swap
  see the new list.
- **Disappeared service** (deleted, or no longer reported): stop
  accepting new connections (drop the listener) and `remove_alias`.

`add_alias`/`remove_alias` reach the reconciler's `NetManager` the same
way `reconcile_routes` already reaches it for pod_cidr routes: two new
`worker::Command` variants (`AddServiceAlias`/`RemoveServiceAlias`,
mirroring the existing `AddRoute`/`RemoveRoute`) sent over the existing
worker channel — not a second, independently-owned `NetManager` instance.

A service with zero currently-healthy replicas keeps its alias and
listener running, but every accepted connection is closed immediately
rather than attempting to relay anywhere.

### Data flow

Apply `kind: Service` "web" (2 replicas, `spec.port: 8080`) → control
plane derives VIP `10.0.250.7` from "web" within `--service-cidr
10.0.250.0/24` → an operator can confirm that assignment immediately via
a direct `GET /services` call against the control plane (no `keelctl`
verb reaches the bare collection — see Goals), without waiting on any
heartbeat → Milestone 15's existing reconciliation places the two
replicas exactly as it already does today → every node's next heartbeat
response includes `{name: web, vip: 10.0.250.7, port: 8080, replicas:
[...]}` → every node aliases `10.0.250.7` on its own bridge and runs a
relay listener on `10.0.250.7:8080` → a caller jail anywhere in the
cluster connects to `10.0.250.7:8080`; its own node's bridge already owns
that address, so the connection is delivered locally with no network hop
needed to "find" the VIP → the local relay picks a replica (a real network
hop if that replica lives on a different node) and copies bytes until
either side closes.

## Error Handling

- VIP allocation is a hard requirement, unlike replica placement (which
  Milestone 15 tolerates failing partially): a service without a VIP
  can't function at all. A collision during derivation is resolved by
  linear probing, new behavior for this milestone (unlike `pod_cidr`'s
  existing collision handling, which rejects the colliding registration
  outright rather than probing — see Architecture); exhausting
  `service_cidr` entirely is an outright `apply` rejection, not a silent
  partial success.
- `spec.port` must be a valid port (1-65535), rejected at spec-validation
  time the same way `keel-spec` already validates `resources.cpu`/
  `memory`.
- Changing an already-applied service's `port` is rejected the same way a
  `template` change already is (`ApplyServiceError::PortChanged`, the same
  "identity is immutable, only replicas scale" precedent as `template`);
  only `replicas` may change on a re-apply.
- A node that can't refresh (heartbeat failing, e.g. the control plane is
  briefly down) keeps serving its last-known replica list rather than
  tearing down its aliases/listeners — a failed heartbeat means "can't
  confirm," not "the service is gone," matching this project's
  self-healing-over-durability stance — Milestone 7's deliberately
  memory-only node registry (a departure from Milestone 4's on-disk,
  durability-first jail records), not Milestone 4 itself.
- Deleting a service tears down the path for *new* connections only;
  already-established relays drain naturally to completion, the same "no
  rebalancing" behavior as any other replica-list change.
- A listener bind failure for one service (an unexpected address/port
  conflict) is logged and leaves that one service unproxied until a later
  heartbeat retries; it does not crash `keel-agentd` or affect any other
  service, continuing this project's "one bad actor doesn't block the
  others" principle.
- A service with no currently-healthy replicas keeps its VIP alias and
  listener up, but every accepted connection is closed immediately.

## Testing Strategy

- **`keel-spec`:** unit tests for `ServiceSpecBody`'s new `port` field
  parsing and its rejection of `port: 0` via `parse_and_validate_service`,
  matching the existing `cpu`/`memory` validation test shapes.
- **`keel-net`:** unit tests for `add_alias`/`remove_alias` against
  `FakeNetManager`; real, FreeBSD-only VM-verified tests against actual
  `ifconfig` behavior, matching every prior milestone's "verify the one
  genuinely OS-level part for real" discipline.
- **`keel-controlplane`:** unit tests for deterministic VIP derivation
  (same name always yields the same VIP), collision resolution (two names
  hashing to the same candidate resolve deterministically to distinct
  VIPs), and `service_cidr` exhaustion producing a clean rejection; a wire
  round-trip test for the heartbeat response's new service-table body; an
  HTTP-layer test confirming a heartbeat's response body reflects the
  currently healthy replica set; an HTTP-layer test confirming
  `GET /services` reports the applied service's `vip`/`port`; a unit test
  confirming a scale-only re-apply with a changed `port` is rejected via
  `ApplyServiceError::PortChanged`, mirroring the existing `TemplateChanged`
  test shape.
- **`keel-agentd`:** fake-backed tests for the proxy manager's diff logic
  (new/updated/removed service in response to synthetic heartbeat-response
  inputs), using real local `127.0.0.1` listeners standing in for
  replicas (the same idiom already used for fake-remote-agent tests
  elsewhere) to verify actual byte relay correctness, the retry-once
  behavior (first replica's listener refuses, second one succeeds), and
  connection refusal when the replica list is empty.
- **VM verification (three real nodes, same discipline as every
  milestone):** apply a 2-replica service, read its VIP back via a
  direct `GET /services` HTTP call to the control plane (`keelctl` has
  no verb for the bare collection — see Goals — so this is the one step
  in this milestone's VM verification that doesn't go through it; still
  preferable to deriving the VIP by hand or reading a node's `ifconfig`
  output), confirm that VIP is aliased on all three nodes' bridges;
  connect to `<vip>:<port>` from a jail and confirm it
  reaches a replica, repeated enough times to confirm both replicas
  actually get used; kill one replica's node and confirm the survivor
  keeps answering after the next reconcile; delete the service and
  confirm the VIP alias disappears from every node and new connections
  are refused.

## Open Questions / Deferred Decisions

- Round-robin is the only selection algorithm built here; least-connections,
  weighted, or latency-aware selection are left for a future milestone if
  round-robin proves insufficient in practice.
- Whether `service_cidr` needs to be sized/validated against the number of
  services expected at any real operational scale is left unaddressed;
  today's collision-and-probe handling assumes collisions are rare, not
  frequent.
- Whether the heartbeat response's service table should ever be filtered
  per-node (e.g. only sending a node the services it actually needs to
  proxy) rather than the full cluster-wide list to every node is deferred;
  today every node aliases and proxies every service, since any node might
  host a caller for any service.
- External reachability (NodePort/LoadBalancer-equivalent) remains a
  distinct, not-yet-designed future milestone in this sub-project,
  deliberately out of scope here, exactly as this document's own
  Non-Goals state.
