# Milestone 14: Cross-Node Overlay Networking (Sub-Project 5, First Milestone)

Status: Approved
Date: 2026-07-16

## Context

Every milestone through 13 built a multi-node control plane (registry,
scheduling, resource-aware placement, mTLS, revocation/rotation) without
ever touching cross-node data-plane networking. Concretely: each node's
`keel0` bridge and its jails' `epair(4)` attachments are entirely
host-local, with no L2 or L3 connectivity between different nodes' jails.
A jail on `node-4` cannot reach a jail on `node-5` today, regardless of
what address either one is given; `keel-net`'s `NetManager` trait
(`ensure_bridge_exists`/`attach_jail`/`detach_jail`) never leaves the local
host, and `keel-spec`'s `NetworkSpec.address` is a plain, unvalidated,
user-chosen CIDR string with no cluster-wide coordination behind it. This
gap has been on record as an explicit non-goal or "not yet designed" item
since Milestone 7's own design spec, and is the last item in the
README's roadmap not yet started.

The roadmap bundles "cluster networking" together with "service
discovery/load balancing," but these are different kinds of problems: one
is host/kernel-level (can a packet from one node's jail reach another
node's jail at all), the other is control-plane/software-level (a stable
name that resolves to, and load-balances across, a jail's current
instance). The second is meaningless without the first, so this milestone
scopes to cross-node reachability only; service discovery and load
balancing are deliberately deferred to a separate, later sub-project.

Every VM this project has ever verified against sits on the same
L3-reachable network (`192.168.64.0/24`, no NAT between nodes), and every
prior milestone's trust model has explicitly assumed same-network trust.
This milestone leans into that reality rather than building for a topology
this project has never actually run on: nodes are assumed to already be
able to route to each other's real (non-jail) addresses directly, so
cross-node jail reachability is solved with plain kernel routing, not a
tunnel protocol (vxlan/gre/WireGuard). No new dependency, no new
encapsulation, no new operational surface beyond a routing table entry per
peer node.

## Goals (Milestone 14)

- A jail on one node is reachable by IP from a jail on any other node,
  over the existing physical network, with no tunnel interface and no new
  dependency.
- Each node is assigned a distinct, non-overlapping subnet block (a `/24`
  by default), derived deterministically from its `node-id` and the
  operator-configured cluster CIDR. The same `node-id` always derives the
  same block, so a control-plane restart, which forgets all other registry
  state exactly as it always has since Milestone 7, needs to remember
  nothing about subnet assignments either — they're recomputed identically
  every time.
- Nodes automatically learn every peer's subnet and real address, and keep
  their own kernel routing table in sync as nodes join, go `Dead`, or
  disappear, piggybacking on `keel-agentd`'s existing 5-second
  registration/heartbeat loop. No new background thread, no new polling
  cadence.
- A `JailSpec` applied with a `network.address` outside its own node's
  assigned subnet is rejected at apply time, before any provisioning is
  attempted, with an error naming both the address and the node's actual
  block.
- Plain single-node `keel-agentd` usage (no `--control-plane-addr`) is
  entirely unaffected, as it has been through every prior milestone.

## Non-Goals (Milestone 14)

- **No service discovery or load balancing.** A stable, relocation-proof
  name for a jail (or a set of jails) is a separate, later sub-project;
  this milestone only makes IP-level reachability possible, the
  prerequisite for that later work, not the naming/balancing layer itself.
- **No support for non-L3-adjacent nodes.** If two nodes cannot already
  route to each other's real addresses (separate networks, NAT), this
  milestone's routing-based approach does not help; a tunnel protocol
  would be required, and is explicitly out of scope. Every deployment this
  project has ever run assumes same-LAN/same-routed-network trust already
  (stated as far back as Milestone 7), so this is a continuation of an
  existing assumption, not a new one.
- **No dynamic `--cluster-cidr` reconfiguration on a live cluster.**
  Changing it changes every node's deterministically-derived subnet on its
  next registration, which would silently break cross-node reachability
  for jails already running under the old assignment, the same class of
  disruption a changed CA would cause for certificates. Operators are not
  expected to change it once a cluster has jails running; no migration
  tooling is built.
- **No firewalling or network policy between jails.** Cross-node
  reachability is cluster-wide with no segmentation, consistent with this
  project's existing "same-network trust assumed" stance for the control
  plane itself (Milestones 7-10).
- **No IPv6.** Every address in this project has been IPv4 since
  Milestone 1; this milestone does not change that.
- **No control-plane persistence of any kind.** The deterministic
  derivation function is specifically what makes new persistence
  unnecessary; introducing a stored node→subnet mapping (or a shared
  clustered database, considered and rejected during this milestone's own
  design) would be solving a problem this scheme doesn't have.
- **No per-node subnet size configurability.** The per-node block size is
  a hardcoded `/24` (256 usable addresses per node, 256 possible node
  blocks within a `/16` cluster CIDR), not a flag. If a real need for a
  different size ever surfaces, that's a small, self-contained follow-up,
  not a reason to add a knob nobody has asked for yet.

## Architecture

### Deterministic subnet derivation

A new, small, pure function (`keel-controlplane/src/subnet.rs`): given the
operator-configured `--cluster-cidr` (e.g. `10.0.0.0/16`) and a hardcoded
`/24` per-node block size, a node's subnet is:

```rust
pub fn derive_pod_cidr(node_id: &str, cluster_cidr: &IpNet) -> IpNet {
    let block_count = 1u32 << (24 - cluster_cidr.prefix_len()); // e.g. 256 for a /16 cluster CIDR
    let index = fnv1a(node_id.as_bytes()) % block_count;
    // offset `index` /24 blocks into cluster_cidr's base address
}
```

`fnv1a` is a hand-rolled, few-line implementation of the well-known FNV-1a
hash, not Rust's standard `Hasher` (which is deliberately randomized
per-process for DoS resistance and would break the one property this
scheme depends on: the same `node_id` string must hash to the same value
on every call, across every process, forever). This is the same
"hand-roll it, no new dependency" idiom this project already uses for
constant-time comparison (Milestone 11) and HTTP parsing throughout.

### Registration flow and wire protocol change

`keel-agentd`'s registration request body is unchanged (`id`, `addr`,
`capacity_cpu`, `capacity_memory`) — the node doesn't choose its own
subnet, so it has nothing new to report. `Registry`'s handling of
`Command::Register` (or wherever the worker processes it) now also:

1. Calls `derive_pod_cidr(node_id, cluster_cidr)`.
2. Checks the result against every other **currently registered** node's
   already-derived `pod_cidr`. A genuine collision (two different
   `node_id`s whose hash lands on the same block, rare but possible with a
   `/16` cluster CIDR's 256 slots) is rejected: the registration fails
   with an error naming the conflicting node-id, rather than silently
   double-assigning a block to two nodes.
3. On success, stores the derived value in `NodeRecord` (a new
   `pod_cidr: String` field alongside the existing `addr`/capacity/
   committed-resource fields) and returns it in the response body, which
   changes from today's empty `200` to a YAML body: `pod_cidr:
   10.0.4.0/24`.

`GET /nodes`'s existing per-node YAML also gains `pod_cidr`, since every
peer needs to see every other peer's block to build its own routes, not
just its own.

### `keel-net`: two new `NetManager` methods

```rust
fn add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), NetError>;
fn remove_route(&self, subnet: &str) -> Result<(), NetError>;
```

`ProcessNetManager` implements these by shelling out to `route(8)`
(`route add -net <subnet> <gateway>` / `route delete -net <subnet>`),
tolerating "already exists"/"not in table" the same idempotent way
`ensure_bridge_exists` and `attach_jail` already tolerate analogous
conditions. `FakeNetManager` gets a trivial in-memory route set for
fast, FreeBSD-free tests. These stay on the existing `NetManager` trait
rather than a new one: host-level routing-table state is conceptually
adjacent to the bridge/epair state that trait already owns, and this
project has consistently preferred one trait per crate-level concern over
proliferating narrow ones.

### `keel-agentd`: route reconciliation on the existing heartbeat loop

`registration.rs`'s background thread already wakes every 5 seconds to
register or heartbeat. This milestone adds, on every tick regardless of
that tick's register/heartbeat outcome:

1. On a **successful registration**, the returned `pod_cidr` is stored in
   a small shared slot (readable by the HTTP handler for apply-time
   validation, described below).
2. **Every tick**, fetch `GET /nodes` (a new outbound call, reusing the
   exact same TLS/CRL-checked connection machinery every other outbound
   call in this project already uses) and diff the returned peer list
   against a locally-tracked "which routes are currently installed" map:
   a peer that's `Alive` and not yet routed gets `add_route(peer.pod_cidr,
   peer.addr)`; a peer that's now `Dead` or missing from the list entirely
   (e.g. after a control-plane restart wiped the registry, the same
   "forget and re-heal" event Milestone 7 already established for
   liveness itself) gets `remove_route(peer.pod_cidr)`.
3. A single `route(8)` failure is logged and retried on the next tick,
   exactly like every other best-effort reconciliation loop in this
   project (the reconciler's own per-jail failure tolerance, the
   registration loop's own retry-on-any-failure behavior) — it never
   aborts the whole tick or crashes the process.

### `keel-agentd`: apply-time subnet validation

`handle_apply` (`http.rs`) checks, before any ZFS/jail/network side effect
is attempted: if this node has a stored `pod_cidr` (i.e., it registered
successfully at least once), is the incoming `JailSpec.spec.network.address`
inside it? If not, `400`, naming both the given address and the node's
actual block, at the same point in the handler where the existing
`metadata.name` mismatch check already short-circuits. A node with no
`pod_cidr` (plain single-node mode, or one that has never yet registered
successfully) skips the check entirely — the same "single-node usage
completely unaffected" invariant every milestone since 7 has preserved.

### CLI flags

`keel-controlplane` gains a new `--cluster-cidr <CIDR>` flag, required
unconditionally (matching how `--tls-*-file` became unconditionally
required in Milestone 12 — there is no "cluster networking off" mode for
the control plane once this ships, since every registered node needs a
consistent `pod_cidr` for the scheme to hold together). `keel-agentd`
gains no new flags: it needs no cluster-wide configuration of its own,
since its subnet is handed to it by the control plane at registration,
not derived locally.

## Error Handling

- A `pod_cidr` collision at registration is treated exactly like any other
  registration failure this project already tolerates: the registering
  `keel-agentd` logs it and retries on the next tick (Milestone 7's
  existing "any heartbeat/registration failure just re-registers"
  behavior), so a persistent collision surfaces as a node that never goes
  `Alive` — visible in `GET /nodes` and in the node's own log — not a
  silent wrong-network jail.
- Apply-time subnet validation failures are a plain `400`, the same shape
  as every other pre-provisioning validation failure in this project
  (mismatched name, invalid YAML, immutable-field change).
- Route reconciliation failures (a single `route(8)` call failing) are
  logged once and retried on the next 5-second tick; they never stop the
  rest of that tick's reconciliation (other routes still get added/removed
  normally) or crash the process, matching this project's consistent
  "one failure doesn't block everything else" reconciliation philosophy.
- A node that never successfully registers (collision, or the control
  plane being unreachable) can still serve locally-applied jails exactly
  as it always could; the only consequence is that it never becomes
  cross-node-routable, which is discoverable the same way an `Alive`
  check already is today, not a new failure mode this milestone needs to
  separately guard against.

## Testing Strategy

- **Pure-function unit tests (no FreeBSD required):** `derive_pod_cidr`
  is deterministic across repeated calls with the same inputs (proving it
  does not depend on Rust's randomized default hasher); different
  `node_id`s spread across the available block range; a fabricated
  collision (two ids whose hash lands on the same block within a small
  test `--cluster-cidr`) is detected and rejected, naming both node-ids.
- **Registry/worker tests:** registering a node returns its derived
  `pod_cidr`; a second, colliding registration fails with the expected
  error and the first node's assignment is untouched; `GET /nodes`
  includes `pod_cidr` per entry.
- **`keel-agentd` apply-path tests:** an address inside the stored
  `pod_cidr` is accepted unchanged; one outside is rejected with `400`
  before any `FakeJailRuntime`/`FakeZfsManager`/`FakeNetManager` call is
  made, provable the same way the existing `metadata.name` mismatch test
  already proves it (asserting the fakes were never touched).
- **`FakeNetManager` route tests:** `add_route`/`remove_route` are
  idempotent (adding twice, removing something never added, are both
  no-ops); the route-reconciliation diff logic, given a fabricated
  `GET /nodes` peer list across several simulated ticks (a peer added,
  then marked `Dead`, then removed entirely), produces exactly the
  expected sequence of `add_route`/`remove_route` calls, using a fake
  control-plane HTTP layer the same way `registration.rs`'s existing
  tests already fake it.
- **`ProcessNetManager` real-command tests (FreeBSD-only):**
  `add_route`/`remove_route` against real `route(8)`, asserting the
  kernel routing table actually gained/lost the expected entry and that
  both are idempotent against the real OS, mirroring `keel-net`'s existing
  `ensure_bridge_exists`/`attach_jail` test pattern.
- **VM verification (three real nodes, the same discipline as every
  milestone since Milestone 2):** start all three with a shared
  `--cluster-cidr`, confirm each derives a distinct `pod_cidr` (visible
  via `GET /nodes`) and that restarting one node re-derives the identical
  block; apply a jail on `node-4` and one on `node-5`, each addressed
  within its own node's block, and confirm one can reach the other's jail
  IP directly, proving the routed model end-to-end on real hardware, not
  just `ProcessNetManager`'s own unit-level `route(8)` calls; kill one
  node's `keel-agentd` and confirm the other nodes' routing tables drop
  its route within one heartbeat-tick window with no restart of anyone,
  mirroring the "no restart" proof pattern Milestone 13 established for
  TLS reload; apply a jail with an out-of-range address and confirm the
  `400` with no ZFS/jail/network side effects; clean teardown confirmed on
  all three VMs afterward.

## Open Questions / Deferred Decisions

- Whether a node that never successfully registers should be blocked from
  serving locally-applied jails at all (rather than just never becoming
  cross-node-routable) is left as today's status quo: no additional block,
  since the existing `Alive` visibility already makes the gap
  discoverable.
- A shared clustered database (etcd-style) for control-plane state was
  considered and explicitly rejected during this milestone's design: it
  would solve a problem (durable state across restarts, or multiple
  control-plane replicas) this project doesn't have, at an operational and
  dependency cost far larger than anything this project has taken on
  before, when a pure deterministic function solves the actual problem
  (subnet stability across a single control-plane instance's restarts)
  with zero new state and zero new dependency.
- Whether `--cluster-cidr`'s per-node block size should ever become
  configurable (rather than a hardcoded `/24`) is deferred until a real
  need surfaces; not designed here.
- Service discovery and load balancing, the other half of the roadmap's
  original "cluster networking" line, remain a distinct, not-yet-designed
  future sub-project, deliberately out of scope for this milestone.
