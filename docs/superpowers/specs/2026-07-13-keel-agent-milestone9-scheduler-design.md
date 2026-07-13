# Milestone 9: Scheduler, Automatic Node Placement (Sub-Project 3, First Milestone)

Status: Approved
Date: 2026-07-13

## Context

Milestone 8 closed out sub-project 2: a caller names an exact node id and
`keel-controlplane` routes the apply/get/delete there. Its own Non-Goals
section named the gap precisely: "Scheduling or placement logic. The
caller always names the exact target node id. Bin-packing across nodes
is a separate future milestone (README roadmap: 'Scheduler')." This is
that milestone, and the first of a new sub-project 3.

`keel-controlplane`'s `Registry` (Milestone 7) tracks only `id`, `addr`,
and heartbeat-derived liveness, nothing about capacity or current load.
Rather than extend the heartbeat wire format to report per-node resource
usage, a bigger change touching `keel-agentd` too, this milestone picks
the node with the fewest jails the control plane itself has placed there,
tracked in a new in-memory table it already owns. This mirrors the
project's existing self-healing-over-durability bet (Milestone 7's
registry is memory-only; a restart forgets everything and the system
recovers via re-registration/re-apply rather than persistence) applied to
placement instead of membership.

## Goals (Milestone 9)

- A new `Placements` table in `keel-controlplane` (`jail_name -> node_id`),
  owned by the same single worker thread that already owns `Registry`, so
  scheduling decisions and bookkeeping updates happen without new
  concurrency primitives.
- A new route family, `PUT`/`GET`/`DELETE /jails/{name}` (no node segment
  in the path), alongside the unchanged Milestone 8 `/nodes/{id}/jails/...`
  routes:
  - `PUT`: if `{name}` already has a recorded placement, forward to that
    *same* node (sticky; an apply is an update-in-place, not a
    migration). Otherwise, schedule: pick the `Alive` node with the fewest
    jails recorded in `Placements`, ties broken by ascending node id.
  - `GET`/`DELETE`: forward to whatever node `{name}` is recorded against;
    no placement means 404.
- The existing named-node routes (`PUT`/`DELETE /nodes/{id}/jails/{name}`)
  also update `Placements` on success, so jail counts and the new
  scheduled routes stay accurate no matter which route family a jail was
  placed through.
- `keelctl`'s `--node` flag becomes optional: `--control-plane-addr`
  alone means "let the control plane schedule it"; `--control-plane-addr
  --node <id>` keeps today's exact-node behavior unchanged; `--node`
  without `--control-plane-addr` remains a usage error.
- A new `503 Service Unavailable` status/reason-phrase entry for "no
  alive nodes to schedule onto."

## Non-Goals (Milestone 9)

- **Resource-aware bin-packing.** Placement is by jail *count* only, not
  actual CPU/memory pressure. Nodes report no capacity or utilization
  today; teaching them to is a separate, larger future milestone if it's
  ever needed.
- **Rebalancing or migrating an already-placed jail.** If a jail's sticky
  node goes `Dead`, re-applying to it returns the same `Dead` error the
  named-node route already returns for a dead target; the scheduler does
  not pick a replacement node automatically. A human can still explicitly
  `PUT /nodes/{new-id}/jails/{name}` to repin it, but this only updates
  the control plane's own bookkeeping — it does not delete the jail from
  its previous node. That is a deliberate manual escape hatch, not a
  migration feature: a caller who actually wants to move a jail must
  delete it from the old node first.
- **Cluster-wide listing or aggregation.** There is still no route that
  fans out across every node (deferred since Milestone 8). The scheduled
  route family adds `{name}`-scoped `GET`/`DELETE` only, not a
  scheduler-aware `GET /jails` across the whole cluster.
- **Persisting `Placements` across a `keel-controlplane` restart.** A
  restart forgets every placement, same as it already forgets every node
  registration; a caller can re-apply, which either lands on the same
  node (if it re-registered with a live jail already there and the name
  is re-applied) or gets freshly scheduled.
- **Any change to `keel-agentd` or the `JailSpec` schema.** The scheduler
  is entirely a `keel-controlplane`/`keelctl` concern; a node never learns
  it was chosen by a scheduler rather than named directly.
- **Authentication/authorization.** Still unaddressed, same deferral as
  every prior milestone.

## Architecture

### `keel-controlplane`: a new `Placements` table

New module `placements.rs`, structurally parallel to `registry.rs`:

```rust
#[derive(Debug, Default)]
pub struct Placements {
    by_jail: HashMap<String, String>, // jail_name -> node_id
}

impl Placements {
    pub fn get(&self, jail_name: &str) -> Option<&str> { ... }
    pub fn set(&mut self, jail_name: String, node_id: String) { ... }
    pub fn remove(&mut self, jail_name: &str) { ... }
    /// node_id -> number of jails currently recorded against it.
    pub fn counts(&self) -> HashMap<&str, usize> { ... }
}
```

### `keel-controlplane`: the scheduler

New module `scheduler.rs`, a pure function with no state of its own:

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no alive nodes available to schedule onto")]
    NoAvailableNodes,
}

pub fn pick_node(alive_ids: &[String], counts: &HashMap<&str, usize>) -> Result<String, ScheduleError> {
    alive_ids
        .iter()
        .min_by_key(|id| (counts.get(id.as_str()).copied().unwrap_or(0), (*id).clone()))
        .cloned()
        .ok_or(ScheduleError::NoAvailableNodes)
}
```

The `(count, id)` tuple key gives lowest-count-first, ascending-id
tie-break, in one pass, with no pre-sorting needed. Trivially unit-tested
against fakes; no FreeBSD involved, since this is pure control-plane
logic with no OS interaction, unlike most prior milestones.

### `keel-controlplane`: `worker.rs` gains two resolution commands and two mutation commands

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleOrResolveError {
    #[error(transparent)]
    Schedule(#[from] ScheduleError),           // NoAvailableNodes
    #[error(transparent)]
    Resolve(#[from] ResolveError),             // Unknown/Dead, for a sticky placement
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("no known placement for jail '{0}'")]
    NotPlaced(String),
    #[error(transparent)]
    Resolve(#[from] ResolveError),             // the placed node is Unknown/Dead
}

pub enum Command {
    // ...existing Register/Heartbeat/List/Resolve unchanged...
    // Returns the resolved (node_id, addr) pair for the caller to forward to.
    ResolveOrSchedule(String /* jail_name */, Sender<Result<(String, String), ScheduleOrResolveError>>),
    ResolvePlacement(String /* jail_name */, Sender<Result<(String, String), PlacementError>>),
    RecordPlacement(String /* jail_name */, String /* node_id */, Sender<()>),
    RemovePlacement(String /* jail_name */, Sender<()>),
}
```

`worker::spawn` currently takes only a `Registry` (`pub fn spawn(mut registry: Registry)`).
This milestone changes its signature to also own a `Placements`
(`pub fn spawn(mut registry: Registry, mut placements: Placements)`), since
the whole point is one worker thread owning both tables. `keel-controlplane`'s
`main.rs` (currently `worker::spawn(Registry::new())`) and `http.rs`'s
`start_test_server` test helper (currently the same one-argument call) both
need updating to pass `Placements::new()` alongside `Registry::new()`.

- `ResolveOrSchedule` (used only by `PUT /jails/{name}`, the scheduled
  route): if `Placements.get(name)` is `Some(node_id)`, resolve it
  through `Registry::resolve` exactly like today's `Resolve` command
  (surfacing `Dead`/`Unknown` the same way). Otherwise, take `Registry`'s
  current `Alive` ids and `Placements`' counts, call
  `scheduler::pick_node`, and resolve the winning id's address. It does
  **not** call `Placements::set` itself — that only happens after the
  forward succeeds (see below), so a failed apply never leaves a phantom
  placement. (The bookkeeping that runs after a successful named-node
  `PUT` is the separate `RecordPlacement` command described next, not
  `ResolveOrSchedule`.)
- `ResolvePlacement` (used by `GET`/`DELETE /jails/{name}`): looks up
  `Placements.get(name)` only, no scheduling; `None` is a distinct
  `PlacementError::NotPlaced`, `Some` resolves through `Registry` the same
  Dead-aware way.
- `RecordPlacement`/`RemovePlacement`: direct `Placements` mutations, sent
  by `http.rs` only after it has already seen a successful (`2xx`)
  response relayed from the target node.

This keeps the existing Milestone 8 principle intact: a resolution is a
pure read that can't corrupt state, and any write to `Placements` happens
only once, in response to a confirmed outcome, matching how
`Reconciler`/`Registry` already treat one bad actor as never blocking or
corrupting anything else.

### `keel-controlplane`: `http.rs` routing

Three new arms for the unscoped `/jails/{name}` family, and light
additions to the four existing `/nodes/{id}/jails/...` arms:

```rust
("PUT", ["jails", name]) => handle_scheduled_apply(name, &request.body, commands),
("GET", ["jails", name]) => handle_scheduled_read(name, "GET", commands),
("DELETE", ["jails", name]) => handle_scheduled_delete(name, commands),
```

- `handle_scheduled_apply`: `ResolveOrSchedule` → on `Ok((node_id, addr))`,
  forward `PUT /jails/{name}` to `addr` (same `forward()` helper Milestone
  8 already has); on a `2xx` response, send `RecordPlacement(name,
  node_id)` before returning the relayed response to the caller.
  `ScheduleError::NoAvailableNodes` → `503`; a `Dead`/`Unknown` sticky
  node → `404`, identical wording to Milestone 8's existing named-node
  errors.
- `handle_scheduled_read`/`handle_scheduled_delete`: `ResolvePlacement` →
  `PlacementError::NotPlaced` → `404 "no known placement for jail '{name}'"`;
  `Dead` → `404` as above; `Ok` → forward, and for delete, `2xx` →
  `RemovePlacement(name)` before responding.
- The four existing named-node arms (`PUT`/`GET`/`DELETE
  /nodes/{id}/jails/{name}` and the list `GET /nodes/{id}/jails`) keep
  their exact Milestone 8 behavior. `handle_forward` itself stays generic
  (it's also used by the list route, which has no single jail name to
  record); the bookkeeping is added in `route()`'s `PUT`/`DELETE` arms,
  which already have `name` in scope, wrapping the existing call:

  ```rust
  ("PUT", ["nodes", id, "jails", name]) => {
      let (status, body) =
          handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands);
      if (200..300).contains(&status) {
          send_record_placement(name, id, commands);
      }
      (status, body)
  }
  ("DELETE", ["nodes", id, "jails", name]) => {
      let (status, body) = handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands);
      if (200..300).contains(&status) {
          send_remove_placement(name, commands);
      }
      (status, body)
  }
  ```

  `send_record_placement`/`send_remove_placement` are small shared
  helpers (fire-and-forget the `RecordPlacement`/`RemovePlacement`
  command and wait for the reply) reused by `handle_scheduled_apply` and
  `handle_scheduled_delete` too, so the bookkeeping call is written once.
  The `GET` single-jail arm and the list arm are untouched, since neither
  is a write.

`reason_phrase` gains one entry: `503 => "Service Unavailable"`.

### `keelctl`

`Target::ControlPlane { addr: String, node: Option<String> }` (was a
required `String`). Argument parsing:

```rust
let target = match (control_plane_addr, node) {
    (Some(addr), node) => Target::ControlPlane { addr, node }, // node may be None
    (None, Some(_)) => { eprintln!("error: --node requires --control-plane-addr"); return ExitCode::FAILURE; }
    (None, None) => Target::Socket(socket),
};
```

`jails_path` gains a third arm: `ControlPlane { node: None, .. }` builds
the bare `/jails/...` path (the new scheduled route), same as `Socket`
does today; `ControlPlane { node: Some(id), .. }` keeps building
`/nodes/{id}/jails/...` exactly as it does now. No other call site
changes: `run_apply`/`run_get`/`run_delete`/`dispatch` are untouched.

## Error Handling

- Scheduling and bookkeeping are split so a forwarding failure never
  corrupts `Placements`, the same principle Milestone 8 already applies
  to `Registry`: `ResolveOrSchedule`/`ResolvePlacement` are pure reads;
  only a confirmed `2xx` from the target node triggers
  `RecordPlacement`/`RemovePlacement`.
- A jail whose sticky node has gone `Dead` is **not** silently
  rescheduled; the caller gets the same `Dead` error a named-node route
  would give for that node, so a re-apply to a dead node fails loudly
  rather than quietly creating a second copy of the jail elsewhere.
- `503` is reserved specifically for "no `Alive` node exists to schedule
  onto," distinct from `404`'s "this specific node/placement doesn't
  exist," so a client can tell "the cluster has no capacity right now"
  apart from "you asked about something that isn't there."

## Testing Strategy

- `placements.rs`: unit tests for `set`/`get`/`remove`, `counts`
  aggregating correctly across multiple jails on the same node, and a
  repeated `set` on the same jail name overwriting rather than
  duplicating.
- `scheduler.rs`: unit tests for zero alive nodes (`NoAvailableNodes`), a
  single alive node always winning, least-count winning among several,
  ties broken by ascending id, and a node with a lower count that is
  `Dead` never being picked (it's simply absent from `alive_ids`).
- `worker.rs`: `ResolveOrSchedule` on a fresh jail name schedules onto the
  least-loaded alive node; on an already-placed name it returns the same
  node regardless of other nodes' current counts (sticky); on a name
  whose recorded node has since gone `Dead` it returns the `Dead` error,
  not a fresh schedule. `ResolvePlacement` on an unplaced name returns
  `NotPlaced`. `RecordPlacement`/`RemovePlacement` visibly affect
  subsequent `counts()`/`get()` calls.
- `http.rs`: reusing Milestone 8's fake-remote-`keel-agentd` pattern —
  `PUT /jails/{a}` and `PUT /jails/{b}` against two registered alive
  nodes land on different nodes when counts are equal (deterministic
  ascending-id tie-break); re-`PUT`-ing `{a}` after registering a third,
  emptier node still hits `{a}`'s original node (sticky); `GET`/`DELETE
  /jails/{missing}` returns `404`; `DELETE /jails/{a}` followed by `GET
  /jails/{a}` returns `404` (placement actually removed); `PUT
  /jails/{c}` with zero registered nodes returns `503`; `PUT
  /nodes/{id}/jails/{d}` (named route) followed by `GET /jails/{d}`
  (scheduled route) succeeds and relays from the same node, proving the
  two route families share one `Placements` table.
- `keelctl`: parsing tests for `--control-plane-addr` alone (valid,
  schedules), `--control-plane-addr --node <id>` (valid, exact-node,
  unchanged from Milestone 8), and `--node` alone (still a usage error);
  `jails_path` tests confirming the scheduled case builds a bare
  `/jails/...` path.
- Light VM sanity check on the existing three-VM setup (not expected to
  surface OS-specific bugs, since nothing here touches jails/ZFS/VNET
  directly, but confirms the real wiring works end to end): apply two
  differently-named specs with no `--node` and confirm they land on two
  different nodes; re-apply one of them and confirm it stays on the same
  node even after a third node joins with less load.

## Open Questions / Deferred Decisions

- Automatic rescheduling when a sticky node dies (rather than surfacing
  `Dead` and requiring a human to intervene) is deferred; a natural
  candidate for a future rebalancing-focused milestone once there's a
  real need for it.
- Resource-aware placement (real cpu/memory bin-packing) stays future
  work, gated on nodes reporting capacity at all, which they don't today.
- Cluster-wide listing/aggregation remains deferred from Milestone 8,
  unchanged by this milestone.
- Whether `keel-controlplane` needs its own `rc.d` script is still the
  same open question carried since Milestone 7; unaddressed here too.
- "Least-loaded" is best-effort under concurrent fresh applies: since
  `RecordPlacement` only commits after a confirmed response, two
  concurrent `PUT`s for two new, distinct jail names can both observe the
  same (stale) counts and land on the same node instead of balancing
  across two. This never corrupts state (stickiness and the
  no-phantom-placement guarantee both still hold) and doesn't violate any
  goal this milestone claims, since precise bin-packing under concurrency
  is explicitly out of scope, but it's worth naming so a future reader
  doesn't mistake it for a bug.
