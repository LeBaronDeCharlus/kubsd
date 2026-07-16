# Milestone 15: Service Discovery via Replica Sets (Sub-Project 6, First Milestone)

Status: Approved
Date: 2026-07-16

## Context

Milestone 14 gave every node a routable subnet and taught nodes to route
directly to each other's jails, closing the host/kernel-level half of the
roadmap's "cluster networking" line. Its own design spec explicitly
deferred the other half: "a stable name that resolves to, and
load-balances across, a jail's current instance" is a distinct,
control-plane/software-level problem, meaningless without the reachability
Milestone 14 just built, but not something that milestone's own scope
touched at all.

Every `JailSpec` in this project today is a strict 1:1 relationship: one
`metadata.name`, one instance, one node (`keel-controlplane`'s
`Placements` type is a plain `jail_name -> node_id` map). There is no
concept anywhere in `keel-spec`, the scheduler, or the reconciler of
multiple interchangeable copies of the same workload. "Service discovery
and load balancing" as a whole roadmap item is a substantially larger
scope than any single milestone this project has shipped so far, since it
requires introducing that replica concept from scratch before naming or
balancing across replicas means anything.

This milestone scopes down to exactly two things: a replica-set concept
(N copies of a jail template, scheduled across nodes, named
deterministically) and discovery (a way to query which of those replicas
are currently healthy and where). It deliberately does not build a
traffic-distribution mechanism (a proxy, DNS-based round robin, or any
other way of actually routing a request across replicas without the
caller doing it themselves) — that is a separate, later milestone in this
same sub-project, mirroring exactly how Milestone 14 itself deferred
"discovery and load balancing" out of "cluster networking."

## Goals (Milestone 15)

- A new `kind: Service` spec (`metadata.name`, `spec.replicas: N`,
  `spec.template:` the same fields `kind: Jail`'s `spec` already has,
  minus `network.address`) can be applied, producing `N` deterministically
  named jails (`<service-name>-0` .. `<service-name>-{N-1}`) scheduled
  across the cluster.
- Each replica's `network.address` is auto-assigned within its target
  node's Milestone-14 `pod_cidr`, never chosen by the operator.
- The scheduler prefers placing a service's replicas on distinct nodes,
  falling back to today's plain headroom-based bin-packing only once
  every `Alive` node already hosts one of that service's replicas.
- A new `GET /services/<name>` endpoint returns every replica whose node
  is `Alive` and whose own jail is currently `running`, with its node and
  address — the actual discovery mechanism this milestone exists to
  deliver.
- If a node hosting a replica goes `Dead` (or that replica starts
  crash-looping), the control plane automatically schedules a replacement
  on a healthy node, piggybacking on the same 5-second heartbeat traffic
  that already exists — no new background thread, no new polling cadence,
  matching Milestone 14's own established idiom for this project.
- `keelctl apply/get/delete` all work against `kind: Service` specs the
  same way they already work against `kind: Jail`.
- Plain single-node `keel-agentd` usage, and every existing `kind: Jail`
  workflow, are entirely unaffected — `keel-agentd` gains **zero** changes
  in this milestone; a replica is indistinguishable from any other jail to
  the node that hosts it.

## Non-Goals (Milestone 15)

- **No traffic load-balancing or proxying.** Nothing in this milestone
  routes a request across replicas automatically; `GET /services/<name>`
  returns a list, and it is entirely up to the caller (today, an operator
  or a future client library) to pick one. Building that mechanism (a
  proxy, DNS-based round robin, client-side load balancing) is a distinct,
  later milestone in this sub-project.
- **No DNS server.** Discovery is HTTP+YAML over the existing mTLS
  transport, matching `GET /nodes`/`GET /jails`'s exact idiom — not a new
  protocol, port, or resolver convention.
- **No rolling updates.** Once a `Service` exists, only `spec.replicas`
  can change (scaling up or down). Changing `spec.template` (image,
  command, resources, ...) on an existing service is rejected, the same
  `409` precedent `kind: Jail`'s `validate_transition` already establishes
  for its own two immutable fields (`spec.image` and
  `spec.network.address`, both currently rejected via
  `SpecError::ImmutableField`). An operator who wants to change a
  service's template deletes and re-applies it. A rolling (gradual,
  zero-downtime) update strategy is out of scope for this milestone
  entirely.
- **No control-plane persistence of `Service` definitions.** Matching
  this project's established stance since Milestone 7: a control-plane
  restart forgets every `Service` (the same way it already forgets every
  `Placements` entry today), and the operator must re-`apply` it to resume
  automatic scheduling/healing. Already-running replica jails are
  unaffected — each node's own `keel-agentd` persists its own specs
  locally, exactly as it does for any other jail.
- **No cross-service scheduling awareness.** The scheduler's
  same-service spreading preference has no knowledge of, or preference
  about, *other* services' replica placement — only a service's own
  replicas avoid each other.
- **No anti-affinity guarantee stronger than "prefer."** If there are
  fewer `Alive` nodes than requested replicas, multiple replicas of the
  same service can and will land on the same node rather than the `apply`
  failing or replicas going permanently unplaced.
- **No IPv6, no new dependency, no new wire protocol beyond plain
  HTTP+YAML** — consistent with every prior milestone.

## Architecture

### `keel-spec`: the new `Service` kind

A new top-level spec type, parsed and validated the same way `JailSpec`
already is (`parse_and_validate`-style, reusing `validate_name` for
`metadata.name` and the existing resource/network validation for
`spec.template`'s fields):

```rust
pub struct ServiceSpec {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ServiceSpecBody,
}

pub struct ServiceSpecBody {
    pub replicas: u32,
    pub template: JailTemplate, // Spec minus `network.address`
}
```

`keelctl apply -f <file>` sniffs `kind` out of the parsed YAML before
deciding whether to parse the rest as a `JailSpec` (existing path,
`PUT /jails/<name>`) or a `ServiceSpec` (new path, `PUT /services/<name>`)
— `keelctl`'s existing `run_apply` already fully parses the YAML via
`keel_spec::parse_and_validate` for the jail case; the new code path reads
`kind` first and branches before committing to either parser.

### `keel-controlplane`: a new `Services` registry

A new type, structurally parallel to the existing `Registry`/`Placements`
(same "in-memory, no persistence, forgotten on restart" contract):

```rust
pub struct ServiceRecord {
    desired_replicas: u32,
    template: JailTemplate,
}

pub struct Services {
    by_name: HashMap<String, ServiceRecord>,
}
```

Replica *placement* reuses the *existing* `Placements` map unchanged — to
that map, `web-0` is exactly as ordinary a jail name as any `kind: Jail`
ever produced. `Services` only tracks the desired shape (how many, what
template); "where is `web-1` right now" is still answered by the same
`Placements::get` every other routed jail already goes through.

### Scheduling: same-service spreading

`scheduler::pick_node` itself is unchanged — spreading is applied by its
caller. Before calling `pick_node` for a given service's next unplaced
replica, the worker filters the candidate `NodeResources` list to exclude
any node that's already hosting another replica of the *same* service (a
lookup against `Placements`, matching replica names by prefix
`<service-name>-`). If that filtered list is empty (every `Alive` node
already has one), the *unfiltered* list is used instead, falling back to
today's plain headroom-based pick. This keeps `scheduler.rs`'s own,
already-tested pure function completely untouched.

### Addressing: auto-assignment within the target node's `pod_cidr`

Once a replica's target node is chosen, its `network.address` is the
first address in that node's `pod_cidr` (Milestone 14's `NodeStatus`
field), starting from network-plus-2, not already used by another jail on
that node. `NodeStatus.pod_cidr` is a plain `String` on the wire (the
control plane's internal `NodeRecord` keeps it typed as `Ipv4Net`, but
nothing outside `keel-controlplane` sees that), so this milestone's
address-assignment code parses it back into a network type before doing
arithmetic on it — the same way any other consumer of `NodeStatus` would
have to. Network-plus-1 (e.g. `10.0.60.1` for `pod_cidr 10.0.60.0/24`) is
permanently excluded — Milestone 14's `attach_jail` (via `keel-net`'s
`bridge_gateway` helper) already assigns that exact address to the node's
`keel0` bridge as its gateway, so it is reserved before this milestone's
own "used addresses" tracking ever starts, the same way network-plus-1 is
implicitly off-limits everywhere else in this project's addressing story.
This needs a small new per-node "used addresses" set, seeded with
network-plus-1 and otherwise populated the same way `Placements` already
is (recorded when a replica is scheduled, freed when it's torn down) — no
new persistence, no new wire type, just one more in-memory map living next
to `Placements`/`Services`.

### Health signal: extending the existing heartbeat

`keel-agentd`'s existing 5-second heartbeat body (`committed_cpu`,
`committed_memory`) gains one more field: the `running`/backoff status of
every jail it currently hosts (the same data `GET /jails` already returns
locally, just also folded into the outbound heartbeat body). This is the
*only* wire-format change `keel-agentd` needs in this whole milestone —
everything else about how it applies, runs, and reports on a jail is
completely unchanged, since a replica is not a distinct concept to a node
at all.

This also changes an internal, not just wire, type: `keel-controlplane`'s
`Command::Heartbeat` variant (`worker.rs`) is today a positional tuple
`Heartbeat(String, f64, u64, Sender<Result<(), UnknownNode>>)` carrying
just `(node_id, committed_cpu, committed_memory, reply)`. It gains a fifth
field carrying the per-jail running/backoff statuses, which every existing
call site constructing this variant (including `worker.rs`'s own tests,
which build it directly over a bare `mpsc::channel()`) must be updated to
populate.

### Discovery: `GET /services/<name>`

Filters `Placements`' current replicas for this service down to those
whose node is `Alive` (per `Registry`) *and* whose last-reported
heartbeat marked them `running` (per the health signal above), returning
each one's name, node, and address as YAML:

```yaml
- name: web-0
  node: node-4
  address: 10.0.60.5
- name: web-2
  node: node-5
  address: 10.0.207.6
```

`GET /services` (no name) lists every known service, a bare-collection
route mirroring `GET /nodes`'s existing convention at the control-plane
layer. (There is no existing control-plane-level `GET /jails` bare
collection to mirror alongside it — today a bare `GET /jails` only exists
one layer down, scoped to a single node, inside `keel-agentd` itself;
`keel-controlplane` only exposes `GET /jails/<name>`, resolved via
`Placements`, and the per-node-scoped `GET /nodes/<id>/jails` forward. So
`GET /services` is a new bare-collection precedent at the control-plane
layer, not an existing one being reused.)

### Self-healing reconciliation: piggybacked on `Command::Heartbeat`

No new thread, no new timer. Every time the worker processes an
incoming `Command::Heartbeat` (already happening once per node per 5
seconds), it also walks every `Service` and compares its
`desired_replicas` against how many of its replicas currently resolve to
an `Alive`+`running` placement:

- **Missing replicas** (desired > healthy count) get scheduled via the
  same spreading + auto-addressing logic above, for names starting from
  the lowest unplaced index.
- **Excess replicas** (a scale-down: healthy count > desired) get torn
  down starting from the *highest* index, deterministically, via the
  existing per-node `DELETE /nodes/<id>/jails/<name>` forwarding path.

A `Service` applied when there isn't yet enough cluster capacity for all
its replicas is **not an error**: `apply` succeeds, places as many
replicas as currently possible, and the gap closes automatically on a
later heartbeat tick once capacity (or a recovered node) becomes
available — matching this project's consistent "best-effort, retried on
the next tick" reconciliation philosophy.

### `keelctl`: `get`/`delete` fall back from jail to service

`keelctl get NAME` and `keelctl delete NAME` try the existing
`/jails/NAME`-shaped path first; on a `404`, they retry against
`/services/NAME`. No new flag, since jail names and service names share
one flat namespace and a `404` on one path is a cheap, unambiguous signal
to try the other. `apply` doesn't need this fallback since it always
knows `kind` upfront from the parsed YAML.

## Error Handling

- Applying a `Service` whose `template` differs from an already-existing
  service of the same name (anything other than `replicas`) is rejected
  with `409`, the same shape and status code as `kind: Jail`'s existing
  immutable-field rejection (`SpecError::ImmutableField` mapped to `409`
  in `keel-agentd`'s `status_for_error`, today covering `spec.image` and
  `spec.network.address`).
- Applying a `Service` (or a plain `kind: Jail`) whose derived/given name
  collides with an existing jail owned by a *different* service, or with
  a plain unmanaged `kind: Jail`, is rejected at apply time with `400`,
  naming the conflicting owner — before any scheduling is attempted.
- `replicas: 0` is valid: it's a "scaled to zero" service, not an error;
  reconciliation tears down any existing replicas and the service sits
  idle until scaled back up.
- A service that can't currently place all its desired replicas (not
  enough distinct capacity) is not surfaced as a hard failure anywhere —
  `GET /services/<name>` simply returns fewer entries than
  `desired_replicas` until reconciliation closes the gap, discoverable the
  same way an under-capacity cluster is already discoverable via
  `GET /nodes` today.
- `GET /services/<name>` on an unknown service name returns `404`,
  matching `GET /jails/<name>`'s existing behavior for an unknown jail.

## Testing Strategy

- **`keel-spec` unit tests:** `ServiceSpec` parses valid YAML; rejects a
  service with an embedded `network.address` in its template (that field
  doesn't belong there); the usual name/resource validation reused from
  `JailSpec` applies identically.
- **`keel-controlplane` unit tests:** `Services`/`ServiceRecord`
  create/scale-up/scale-down/delete; the spreading filter (same-service
  replicas prefer distinct nodes, fall back to bin-packing once every
  node has one) as a pure-function test against `scheduler::pick_node`'s
  existing test harness; address auto-assignment picks the first free
  address in the target node's `pod_cidr` and never double-assigns one
  still in use; `Command::Heartbeat`-triggered reconciliation schedules a
  missing replica and tears down an excess one, using the same
  fake-command-channel harness `worker.rs`'s existing tests already use.
- **HTTP-layer tests:** `PUT /services/<name>` create/scale/reject-
  template-change/reject-name-collision; `GET /services/<name>` returns
  only `Alive`+`running` replicas, omitting a `Dead`-node or
  crash-looping one; `GET /services` lists all; `DELETE /services/<name>`
  cascades to every currently-placed replica.
- **`keelctl` tests:** `apply` routes `kind: Service` YAML to
  `/services/<name>`, `kind: Jail` YAML to `/jails/<name>` as before;
  `get`/`delete` fall back from the jail path to the service path on
  `404`.
- **VM verification (three real nodes, same discipline as every milestone
  since Milestone 2):** apply a 3-replica service across the real
  cluster, confirm distinct placement and correct auto-assigned addresses
  via `GET /services/<name>`; kill one node's `keel-agentd` and confirm
  the control plane schedules a replacement replica onto a remaining
  healthy node within one heartbeat-tick window, with no restart of
  anyone; scale the service up and down and confirm the replica count and
  named instances converge correctly each time; delete the service and
  confirm every replica jail is torn down on its respective node.

## Open Questions / Deferred Decisions

- Whether `Service`'s spreading preference should ever become configurable
  (e.g. a required, not merely preferred, anti-affinity) is deferred until
  a real need surfaces — not designed here.
- The actual load-balancing/traffic-distribution mechanism (a proxy, DNS,
  or client-side selection library) remains a distinct, not-yet-designed
  future milestone in this sub-project, deliberately out of scope here,
  exactly as this document's own Non-Goals state.
- Rolling updates (changing a `Service`'s template without a delete/
  re-apply cycle) are left as a future milestone's concern if ever
  needed; today's "template is immutable, replicas is the only mutable
  field" is a deliberate simplification, not an oversight.
