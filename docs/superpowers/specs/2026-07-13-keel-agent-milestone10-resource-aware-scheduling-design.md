# Milestone 10: Resource-Aware Bin-Packing (Sub-Project 3, Second Milestone)

Status: Approved
Date: 2026-07-13

## Context

Milestone 9 gave `keel-controlplane` a scheduler, but a deliberately minimal
one: it picks the `Alive` node with the fewest jails *recorded*, a pure
count, with no idea how much CPU or memory those jails actually asked for.
The Milestone 9 design spec named this exact gap in its Non-Goals:
"Resource-aware bin-packing... gated on nodes reporting capacity at all,
which they don't today." This milestone closes it: nodes learn and report
their own capacity and committed load, and the scheduler ranks by resource
headroom instead of jail count.

The central architectural constraint carried forward from Milestones 8-9 is
that `keel-controlplane` never deserializes a `JailSpec` and has no
dependency on `keel-spec`. That constraint shapes this milestone's whole
design: the control plane cannot itself sum up "how much CPU/memory has
this node committed" by reading spec bodies, because it never sees spec
bodies at all, even when forwarding a scheduled apply. So the accounting
has to live where the spec bodies already are: on each node, inside
`keel-agentd`, which already parses `spec.resources.{cpu,memory}` via
`keel-spec::resources` for every jail it applies.

## Goals (Milestone 10)

- `keel-agentd` detects its own capacity once at startup, via `sysctl -n
  hw.ncpu` (CPU cores) and `sysctl -n hw.physmem` (memory bytes), the same
  "shell out to the real command" idiom already used for `jail(8)`/`zfs(8)`,
  no new dependency.
- `keel-agentd`'s registration (`POST /nodes/register`) carries its detected
  `capacity_cpu`/`capacity_memory`, sent once (capacity doesn't change for
  the life of a running node).
- `keel-agentd`'s heartbeat (`POST /nodes/<id>/heartbeat`, today an empty
  body) carries `committed_cpu`/`committed_memory`: the sum of
  `spec.resources.{cpu,memory}` across every jail its own `Reconciler`
  currently tracks, recomputed fresh every heartbeat tick (every 5 seconds),
  the same cadence already driving liveness.
- `keel-controlplane`'s `Registry` stores all four numbers per node and
  exposes them on `GET /nodes`, alongside the existing `id`/`addr`/
  `status`/`last_seen_secs`.
- `keel-controlplane`'s `scheduler::pick_node` is replaced with a
  resource-aware version: among `Alive` nodes, score each by
  `min(free_cpu/capacity_cpu, free_memory/capacity_memory)` (the fraction of
  headroom in its *most* constrained resource) and pick the highest score,
  ties broken by ascending node id, same tie-break rule as Milestone 9.

## Non-Goals (Milestone 10)

- **No pre-flight fit guarantee.** The scheduler ranks nodes by their
  currently-reported headroom; it never learns the *incoming* jail's own
  resource request, because that would require deserializing the spec body,
  which stays off-limits. A node that turns out unable to actually
  accommodate a jail (e.g., real capacity is tighter than reported, or a
  race with another concurrent apply) simply fails that apply the normal
  way; the control plane relays the failure, it does not retry elsewhere.
  This is a ranking heuristic, not an admission-control system, the same
  honest limitation Milestone 9's count-based version already had.
- **No live resource *usage* monitoring.** `committed_*` is the sum of
  *requested/reserved* resources (`spec.resources.cpu`/`memory`, the same
  numbers `rctl(8)` limits are already derived from), not real-time CPU or
  memory utilization read from `rctl(8)` accounting. This mirrors
  Kubernetes' own default scheduler, which schedules on requests, not
  measured usage.
- **No rebalancing of already-placed jails** when capacity or commitment
  changes. Milestone 9's sticky-on-reapply behavior is completely
  unchanged; a placed jail's re-apply still always resolves to its
  recorded node, never a fresh scheduling decision.
- **No overcommit protection beyond the ranking itself.** There is no hard
  cap that refuses to schedule onto a node already over 100% committed;
  the heuristic just makes it unlikely to be chosen while any less-loaded
  alive node exists.
- **Named-node routes are completely unaffected.** `PUT`/`GET`/
  `DELETE /nodes/<id>/jails/<name>` still let the caller target any node
  regardless of its capacity or commitment, exactly as in Milestones 8-9;
  this milestone adds no capacity checks there.
- **No `keel-spec` dependency added to `keel-controlplane`.** The control
  plane's opacity to `JailSpec` bodies, an explicit Milestone 8 invariant,
  is preserved end to end.
- **`Placements::counts()` (jail-count aggregation) is removed**, not kept
  alongside the new resource scoring. Milestone 9's count-based signal is
  fully superseded, not blended with it; keeping unused code around
  violates the project's own YAGNI discipline and would otherwise be a
  dead-code compiler warning.

## Architecture

### `keel-agentd`: capacity detection

New module `keel-agentd/src/capacity.rs`:

```rust
use std::process::Command;

pub fn detect() -> Result<(f64, u64), String> {
    let cpu = run_sysctl("hw.ncpu")?
        .parse::<f64>()
        .map_err(|e| format!("invalid hw.ncpu value: {e}"))?;
    let memory = run_sysctl("hw.physmem")?
        .parse::<u64>()
        .map_err(|e| format!("invalid hw.physmem value: {e}"))?;
    Ok((cpu, memory))
}

fn run_sysctl(name: &str) -> Result<String, String> {
    let output = Command::new("sysctl")
        .arg("-n")
        .arg(name)
        .output()
        .map_err(|e| format!("failed to run sysctl -n {name}: {e}"))?;
    if !output.status.success() {
        return Err(format!("sysctl -n {name} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
```

Called once in `main.rs`, only inside the existing control-plane opt-in
gate (alongside `registration::spawn` and the TCP listener); a node run
without control-plane flags never touches `sysctl` at all, same "second
door, first door unchanged" principle as every prior control-plane
addition.

### `keel-agentd`: `Reconciler::committed_resources`

New method on `Reconciler` (`reconciler.rs`, alongside `list`):

```rust
pub fn committed_resources(&self) -> (f64, u64) {
    self.records.values().fold((0.0, 0u64), |(cpu, mem), record| {
        let cpu_cores = keel_spec::parse_cpu_cores(&record.spec.spec.resources.cpu)
            .expect("resources were already validated at apply time");
        let mem_bytes = keel_spec::parse_memory_bytes(&record.spec.spec.resources.memory)
            .expect("resources were already validated at apply time");
        (cpu + cpu_cores, mem + mem_bytes)
    })
}
```

`.expect()`, not a defensive fallback: every `JailRecord` in `self.records`
already passed `keel_spec::parse_and_validate` at apply time, so a parse
failure here would mean invariant violation elsewhere, not a real runtime
case to handle gracefully, consistent with the project's "trust internal
invariants" convention.

### `keel-agentd`: `worker::Command::CommittedResources`

`worker.rs`'s `Command` enum (generic over `J, Z, N`) gains one variant:

```rust
pub enum Command {
    Apply(JailSpec, Sender<Result<(), ReconcileError>>),
    Get(Option<String>, Sender<Vec<JailStatus>>),
    Delete(String, Sender<Result<(), ReconcileError>>),
    Tick,
    CommittedResources(Sender<(f64, u64)>),
}
```

handled by:

```rust
Command::CommittedResources(reply) => {
    let _ = reply.send(reconciler.committed_resources());
}
```

### `keel-agentd`: `registration.rs` reports capacity and committed load

`spawn`'s signature grows by three parameters:

```rust
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()>
```

`register_once` includes the (static) capacity in its body, extending the
existing hand-formatted string (matching this file's existing convention of
building wire bodies with `format!` rather than `serde_yaml::to_string`,
even though `keel-agentd` already depends on `keel-controlplane` since
Milestone 7):

```rust
fn register_once(
    control_plane_addr: &str,
    node_id: &str,
    advertise_addr: &str,
    capacity_cpu: f64,
    capacity_memory: u64,
) -> Result<(), String> {
    let body = format!(
        "id: {node_id}\naddr: {advertise_addr}\ncapacity_cpu: {capacity_cpu}\ncapacity_memory: {capacity_memory}\n"
    );
    send_request(control_plane_addr, "POST", "/nodes/register", &body)
}
```

`heartbeat_once` queries the worker for current committed resources before
each beat, over the same command-channel pattern used everywhere else in
this project:

```rust
fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) =
        rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body)
}
```

`main.rs` calls `capacity::detect()` once inside the control-plane gate,
`panic!`s with a clear message on failure (same "fail loudly at startup"
treatment as every other unrecoverable startup condition in this file, e.g.
a bad socket bind), and passes the detected values plus a cloned `commands`
sender into `registration::spawn`.

### `keel-controlplane`: wire format

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub id: String,
    pub addr: String,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub committed_cpu: f64,
    pub committed_memory: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub id: String,
    pub addr: String,
    pub status: NodeState,
    pub last_seen_secs: u64,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
    pub committed_cpu: f64,
    pub committed_memory: u64,
}
```

No compatibility shim for the old two-field `NodeRegistration` or the old
empty heartbeat body: every prior milestone has changed wire contracts
outright when the design called for it (e.g. Milestone 8's
`--advertise-addr`), and this project has no external consumers to break.

### `keel-controlplane`: `Registry`

`NodeRecord` gains the same four fields as `NodeStatus`. `register` takes
`capacity_cpu`/`capacity_memory` and initializes `committed_cpu: 0.0`/
`committed_memory: 0` (a re-registration, like today, fully resets the
record; the very next heartbeat, 5 seconds later, refreshes committed load
to the real number). `heartbeat` takes `committed_cpu`/`committed_memory`
and updates them on the matched record, alongside its existing
`last_heartbeat` update:

```rust
pub fn register(&mut self, id: String, addr: String, capacity_cpu: f64, capacity_memory: u64, now: Instant) {
    self.nodes.insert(
        id,
        NodeRecord { addr, last_heartbeat: now, capacity_cpu, capacity_memory, committed_cpu: 0.0, committed_memory: 0 },
    );
}

pub fn heartbeat(
    &mut self,
    id: &str,
    committed_cpu: f64,
    committed_memory: u64,
    now: Instant,
) -> Result<(), UnknownNode> {
    match self.nodes.get_mut(id) {
        Some(record) => {
            record.last_heartbeat = now;
            record.committed_cpu = committed_cpu;
            record.committed_memory = committed_memory;
            Ok(())
        }
        None => Err(UnknownNode(id.to_string())),
    }
}
```

`list` and `resolve` are structurally unchanged; `list` simply carries the
four new fields through into each `NodeStatus` it builds.

### `keel-controlplane`: `scheduler.rs`, replaced

```rust
pub struct NodeResources {
    pub id: String,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
    pub committed_cpu: f64,
    pub committed_memory: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no alive nodes available to schedule onto")]
    NoAvailableNodes,
}

pub fn pick_node(nodes: &[NodeResources]) -> Result<String, ScheduleError> {
    nodes
        .iter()
        .map(|n| (headroom_score(n), n.id.as_str()))
        .fold(None, |best: Option<(f64, &str)>, candidate| match best {
            None => Some(candidate),
            Some(current) if candidate.0 > current.0 || (candidate.0 == current.0 && candidate.1 < current.1) => {
                Some(candidate)
            }
            _ => best,
        })
        .map(|(_, id)| id.to_string())
        .ok_or(ScheduleError::NoAvailableNodes)
}

fn headroom_score(n: &NodeResources) -> f64 {
    let cpu_frac = if n.capacity_cpu > 0.0 { (n.capacity_cpu - n.committed_cpu) / n.capacity_cpu } else { 0.0 };
    let mem_frac = if n.capacity_memory > 0 {
        (n.capacity_memory as f64 - n.committed_memory as f64) / n.capacity_memory as f64
    } else {
        0.0
    };
    cpu_frac.min(mem_frac)
}
```

`NodeResources` is a small, `wire`-independent struct local to `scheduler`,
the same reason Milestone 9's version took a plain `&[String]` rather than
`&[NodeStatus]`: the scheduler stays pure and testable with hand-built
values, no `Registry`/`wire` dependency. `pick_node`'s manual fold (rather
than `min_by_key`/`max_by_key`) exists because `f64` has no `Ord`; the fold
explicitly encodes "highest score wins, ties broken by ascending id",
mirroring Milestone 9's `(count, id)` tuple ordering without needing `f64`
to implement it.

Headroom can go negative (a node already over-committed relative to its
own reported capacity); `headroom_score` allows this on purpose rather
than clamping to zero, since clamping would make two over-committed nodes
indistinguishable from each other when one is in fact less over-committed,
throwing away real signal the ranking should still use.

### `keel-controlplane`: `worker.rs`

`Command::Register`/`Command::Heartbeat` grow the same new fields as the
`Registry` methods they wrap:

```rust
pub enum Command {
    Register(String, String, f64, u64, Sender<()>),
    Heartbeat(String, f64, u64, Sender<Result<(), UnknownNode>>),
    // List, Resolve, ResolveOrSchedule, ResolvePlacement, RecordPlacement, RemovePlacement: unchanged
}
```

`ResolveOrSchedule`'s scheduling branch (the `else` arm reached only when
`jail_name` has no recorded placement) is rewritten to build
`scheduler::NodeResources` from `Registry::list` instead of counting
`Placements`:

```rust
let nodes: Vec<scheduler::NodeResources> = registry
    .list(now)
    .into_iter()
    .filter(|status| status.status == NodeState::Alive)
    .map(|status| scheduler::NodeResources {
        id: status.id,
        capacity_cpu: status.capacity_cpu,
        capacity_memory: status.capacity_memory,
        committed_cpu: status.committed_cpu,
        committed_memory: status.committed_memory,
    })
    .collect();
scheduler::pick_node(&nodes).map_err(ScheduleOrResolveError::from).and_then(|node_id| {
    registry.resolve(&node_id, now).map(|addr| (node_id, addr)).map_err(ScheduleOrResolveError::from)
})
```

The sticky branch (an existing placement found in `Placements`) is
completely unchanged: it still resolves straight through `Registry::resolve`
with no scheduling involved. `Placements::counts()` is deleted along with
its call site, per the Non-Goals above.

### `keel-controlplane`: `http.rs`

`handle_register` deserializes the extended `NodeRegistration` and passes
its two new fields through to `Command::Register`. `route()`'s heartbeat
arm changes from `handle_heartbeat(id, commands)` (no body) to
`handle_heartbeat(id, &request.body, commands)`; `handle_heartbeat`
deserializes the new `Heartbeat` body (400 on invalid YAML, matching
`handle_register`'s existing treatment) before forwarding
`committed_cpu`/`committed_memory` to `Command::Heartbeat`. Every other
route (`GET /nodes`, the named-node and scheduled jails routes,
`RecordPlacement`/`RemovePlacement` bookkeeping) is untouched; this
milestone's HTTP-layer surface is exactly these two handlers.

## Error Handling

- A malformed heartbeat body (missing or non-numeric `committed_cpu`/
  `committed_memory`) is a `400`, the same treatment `handle_register`
  already gives a malformed registration body. It is not silently
  defaulted to zero: a node reporting a body the control plane can't parse
  is a real signal something is wrong with that node's build, and hiding
  it behind a zero would make the scheduler mis-rank it as maximally free.
- `ScheduleError::NoAvailableNodes` keeps its exact Milestone 9 meaning:
  zero `Alive` nodes exist. It is not renamed or split for "alive nodes
  exist but all are full", because `pick_node` never refuses to return an
  alive node purely for being heavily committed (see the negative-headroom
  note above); running out of nodes and running out of room are genuinely
  different failure modes, but only the first one is a hard `pick_node`
  failure in this design.
- A `Dead` node's stale `committed_*` values are irrelevant: `pick_node`
  only ever receives nodes already filtered to `Alive` by the caller
  (`ResolveOrSchedule`'s existing filter step), the same structural
  guarantee Milestone 9 already relied on for its own `Dead`-node
  exclusion.

## Testing Strategy

- `keel-agentd::capacity::detect`: a real, FreeBSD-only test invoking the
  actual `sysctl` binary and asserting the parsed CPU count and memory are
  both positive (no fake needed or possible here, since the whole point is
  reading the real host; this is the one genuinely OS-level part of the
  milestone, verified for real on the VM per this project's standing
  discipline, not assumed).
- `Reconciler::committed_resources`: fake-backed unit tests — zero records
  sums to `(0.0, 0)`; multiple applied records with known `cpu`/`memory`
  strings sum correctly; a deleted record's resources drop out of the sum.
- `worker::Command::CommittedResources`: a fake-backed integration test
  confirming the command returns the same numbers `committed_resources`
  would, round-tripped through the channel.
- `registration.rs`: unit/integration tests confirming `register_once`'s
  body includes the given capacity values, and `heartbeat_once` actually
  queries the command channel before sending (a fake worker returning a
  known `(cpu, memory)` pair, asserting the sent heartbeat body matches).
- `keel-controlplane::registry`: unit tests for `register` initializing
  `committed_*` to zero, `heartbeat` updating `committed_*` on a known
  node while leaving `capacity_*` untouched, and `list` carrying all four
  new fields through into `NodeStatus` correctly. All existing
  registry/http tests that construct `NodeStatus`/call `register`/
  `heartbeat` need their call sites and literal comparisons updated for
  the new fields, not new behavior, just a wider signature.
- `keel-controlplane::scheduler`: unit tests for `pick_node` mirroring
  Milestone 9's exact test shapes but with resource data instead of
  counts: no alive nodes; a single alive node; the node with more headroom
  in its most-constrained resource wins even when it has less headroom in
  the other; a tie on the min-fraction score broken by ascending id; a
  node that's over-committed (negative headroom) is still picked over no
  node at all when it's the only alive one.
- `keel-controlplane::worker`/`http`: updated versions of Milestone 9's
  sticky/fresh-schedule tests, now registering nodes with explicit
  capacity and heartbeating explicit committed values instead of relying
  on jail counts, confirming the same sticky-on-reapply and
  no-alive-nodes-yields-503 behaviors still hold under the new scoring.
- VM verification, extended from Milestone 9's three-node setup: confirm
  each node's own `sysctl`-detected capacity appears correctly in
  `GET /nodes`; apply jails with different resource requests and confirm
  placement favors the node with more headroom in its tightest resource,
  not just fewer jails; confirm re-registering a node (e.g. after a
  restart) resets its reported committed load to zero until its next
  heartbeat, matching the design's stated re-registration semantics.

## Open Questions / Deferred Decisions

- Whether to eventually feed real `rctl(8)` usage (not just requested
  resources) into the score is deferred; requested-resource scheduling is
  the same choice Kubernetes' own default scheduler makes, and doing both
  would need a second, clearly-labeled signal rather than conflating them.
- Whether a future milestone should add a hard admission check (rejecting
  an apply before forwarding it, once the control plane has *some* way to
  learn the incoming jail's own resource request without fully
  deserializing the spec, e.g. a lightweight scheduling-hint header) is
  left open; this milestone deliberately ships the simpler heuristic-only
  version first.
- Whether `capacity_cpu`/`capacity_memory` should ever be operator-
  overridable (e.g. to reserve headroom for non-Keel workloads on a shared
  host) is deferred; today they are exactly what `sysctl` reports, no
  override flag exists.
