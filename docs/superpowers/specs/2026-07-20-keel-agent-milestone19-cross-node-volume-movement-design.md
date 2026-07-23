# Milestone 19: Cross-Node Volume Movement via Replication and Force Re-Pin (Sub-Project 7, Third Milestone)

Status: Approved

Date: 2026-07-20

## Context

Milestone 17 built persistent volumes for `kind: Jail` on a single node, explicitly deferring "the interesting cross-node questions" (move a stateful replica's data when its node dies, via node-pinning or via `zfs send`/`receive` replication) to a later milestone once the single-node foundation had been used in practice. Milestone 18 answered half of that: node-pinning, so a stateful replica's placement is never silently abandoned or replaced with an empty volume elsewhere. It deliberately traded away automatic failover to get there, and named two accepted, bounded gaps as a result: a pinned replica whose node is permanently gone stays down forever, and there is no operator verb to force it elsewhere.

This milestone closes that gap with the second mechanism Milestone 17 named: `zfs send`/`receive` replication, plus the "force re-pin" verb Milestone 18 explicitly deferred. Each stateful replica gets exactly one standby node, chosen at schedule time. The primary node's `keel-agentd` continuously replicates each volume's dataset to that standby (roughly 30-second snapshot cadence, so a bounded, small window of data loss on failover — acceptable for this project's existing volume use cases: small persistent application state, not high-throughput databases). If the primary is confirmed `Dead`, an operator runs `keelctl force-repin <replica-name>` to promote the standby using its last-received snapshot, reschedule the jail there, and assign that replica a fresh standby. If the original node later comes back, it would otherwise resurrect the stale jail from its own crash-safe on-disk `JailRecord` — the exact mechanism Milestone 18 relies on for the *pinned, still-alive* case, but wrong once that replica has moved. The control plane fences it: the node's next heartbeat carries a forced delete for that jail.

Failover stays fully manual by design, for the same reason Milestone 18 went health-blind rather than auto-rescheduling: an automatic promotion on node-death detection reopens the split-brain/false-positive risk that milestone deliberately avoided, and would need real failure-detection fencing (not just heartbeat-timeout-based `Dead` status) to be safe. This milestone makes replicated data available and gives an operator a safe, explicit way to use it; it does not make the system self-healing for permanent node loss.

## Goals

- Exactly one standby node per stateful replica, chosen by the scheduler at the same time it picks the primary, reusing the existing same-service spreading logic to pick a second, different, capacity-available node.
- `keel-agentd` on the primary continuously replicates each stateful volume's dataset to its standby: snapshot, then a full or incremental `zfs send` over a new direct node-to-node stream, on a roughly 30-second cadence, for as long as that replica exists.
- `keelctl force-repin <replica-name>`: promotes a stateful replica's standby to primary, but only when the current primary is confirmed `Dead` and the standby has completed at least one full replication. Reschedules the jail onto the (former) standby node against its already-replicated dataset, and assigns a fresh standby.
- Fencing: once a replica has been force-re-pinned away from a node, the control plane remembers that node is owed a forced delete for that replica's stale jail, and pushes it the moment that node's next heartbeat arrives — piggybacked on the same heartbeat-handling mechanism Milestones 15/16 already use for self-healing.
- `keel-zfs` gains the primitives none of this exists without today: `snapshot`, `send_snapshot` (full or incremental), `receive_snapshot`.

## Non-Goals

- **No automatic failover.** `force-repin` is always an explicit operator action. A `Dead` primary with a healthy, fully-replicated standby still just sits there, exactly like an unstandby'd pinned replica did under Milestone 18, until an operator intervenes.
- **No configurable replication factor.** Exactly one standby, always. A later milestone could generalize to N if this turns out to matter in practice.
- **No rebalancing while both nodes are alive.** If the standby itself goes `Dead` (primary still `Alive`), replication attempts simply fail and retry every tick; there is no automatic re-selection of a different standby, and no operator verb to force one either. The replica is temporarily unprotected until the standby returns.
- **No mid-flight promotion of a partial replication.** If the standby has never completed a first full send (`ReplicaTarget.last_snapshot` is still `None`), `force-repin` refuses outright rather than promoting an empty or partial dataset.
- **No encryption of the replication stream beyond Milestone 14's existing LAN-trust boundary.** The new node-to-node replication listener is plain TCP, following Milestone 16's proxy-relay precedent exactly (`proxy.rs`'s node-to-node data connections are already unauthenticated plain TCP over the routed overlay); this milestone does not change that boundary even though it now carries raw filesystem data rather than proxied jail traffic.
- **No replication for `kind: Jail` or stateless services.** Only a stateful `kind: Service` replica (one whose `template.volumes` is non-empty, per Milestone 18) ever gets a standby.
- **No cleanup of the old primary's abandoned volume dataset after a successful `force-repin`.** It sits there, orphaned, until an operator runs the existing `keelctl delete-volume` against that node — matching Milestone 17/18's standing "a volume is only ever destroyed by an explicit, separate operation" principle.
- **No change to how a still-pinned (not yet force-re-pinned) replica behaves when its node is merely temporarily `Dead`.** Milestone 18's health-blind `present_indices` logic is completely unchanged; this milestone only adds what happens *in addition*, once an operator decides a node is permanently gone.

## Architecture

### Control-plane state: `Standbys` and `PendingFences`

Two new flat maps, matching the existing style of `Placements`/`UsedAddresses` (plain `HashMap`s, no richer struct, pure in-memory, unpersisted like every other piece of control-plane state today):

```rust
struct Standbys { by_replica: HashMap<String, String> }       // replica_name -> standby node_id
struct PendingFences { by_replica: HashMap<String, String> }  // replica_name -> node_id owed a forced delete
```

`Standbys` is populated whenever the scheduler places a stateful replica's first index (alongside the existing `Placements` entry) and updated on every successful `force-repin`. `PendingFences` gains an entry only as part of a successful `force-repin` and loses one only once that node's heartbeat handling confirms the forced delete succeeded (see "Fencing" below).

Like `Placements` and `UsedAddresses`, both maps are owned by the single-writer `worker::spawn` thread alongside the `Registry` and `Services`, not read or written directly by HTTP handlers. Getting or updating them means adding new `Command` enum variants (e.g. `ResolveStandby`, `RecordStandby`, `CheckPendingFence`) that round-trip through the existing `mpsc::Sender<Command>` / oneshot-reply pattern `resolve_placement` already uses, matching how every other piece of control-plane state is touched today.

### Scheduling: picking a standby

Today's `services::pick_node_for_service` (and the `scheduler::pick_node` it wraps) returns exactly one node per call; nothing in the scheduler currently picks two nodes for one replica. Placing a stateful replica's standby means calling `pick_node_for_service` a second time, with the just-chosen primary node added to the busy-node exclusion set the first call already builds from `nodes_hosting_service`, so the two calls can never return the same node. `force-repin` step 5 reuses this same two-call shape when it needs to hand the freshly-promoted primary a fresh standby.

### `Spec` gains `replicate_to`

Alongside `volumes` (`keel-spec/src/types.rs`) — on the inner `Spec` struct, not the outer `JailSpec` envelope (`apiVersion`/`kind`/`metadata`/`spec`):

```rust
replicate_to: Option<String>  // standby node's advertised "host:port" for its replication listener
```

Set by the control plane whenever it forwards a stateful replica's spec to a node: at first scheduling, and again (pointing at a newly-chosen standby) as part of a `force-repin` promotion. `keel-agentd` persists this in its own on-disk `JailRecord`, the same record Milestone 4 already keeps every other field in (write-then-rename to a `.yaml.tmp` path, so a crash can't leave a corrupt record — not a durability/fsync guarantee, just the existing "never partially written" property).

A new endpoint, `PUT /jails/<name>/replicate-to`, lets the control plane retarget an *already-running* primary's replication without a full re-provision: it just updates the field on the existing `JailRecord`. This is the only mechanism `force-repin` needs to give a freshly-promoted primary a new standby — no separate "start replicating" command required, since the replication loop (below) polls this field on its own schedule.

### Standby side: `ReplicaTarget`, a jail-less record

A node holding a replicated copy runs no jail at all until `force-repin` promotes it — matching Milestone 17's existing precedent that a volume dataset is already independent of any jail record (`GetVolume`/`DeleteVolume` act purely on dataset paths). It gets its own small on-disk record type:

```rust
struct ReplicaTarget {
    replica_name: String,
    volume_dataset: String,        // e.g. tank/keel/volumes/db-0-data
    source_node_addr: String,      // primary's advertised host:port, diagnostics only
    last_snapshot: Option<String>, // None until the first full send completes
}
```

Created on first contact from a primary's replication stream (see below), and consulted by `force-repin` to decide whether promotion is even possible.

### `keel-zfs`: new primitives

`ZfsManager` gains three methods none of `create_volume`/`clone_from_base`/`destroy_dataset` cover:

```rust
fn snapshot(&self, dataset: &str, snapshot: &str) -> Result<(), ZfsError>;
fn send_snapshot(&self, dataset: &str, snapshot: &str, base: Option<&str>, out: &mut dyn Write) -> Result<(), ZfsError>;
fn receive_snapshot(&self, dataset: &str, input: &mut dyn Read) -> Result<(), ZfsError>;
```

`send_snapshot`/`receive_snapshot` shell out to `zfs send [-i <base>] <dataset>@<snapshot>` / `zfs receive <dataset>`, piping the spawned child process's stdout/stdin through the caller's `Write`/`Read`. This is new subprocess-handling plumbing for `keel-zfs`, not just new methods in the existing style: every current `CliZfsManager` method (`create_volume`, `clone_from_base`, `destroy_dataset`) uses `Command::output()`, which buffers everything and returns only once the child has already exited, whereas streaming a live `zfs send`/`receive` needs `Command::spawn()` with `Stdio::piped()` so the caller can read/write the child while it's still running, then check its exit status once the stream is fully drained. `base: None` means a full send: the first replication ever, or any time the standby reports it doesn't have the claimed base snapshot. `FakeZfsManager` has no snapshot concept today at all (even `clone_from_base`'s fake just inserts a dataset name, with no notion of the `@keel` snapshot the real CLI implementation takes internally), so modeling snapshots there — an in-memory dataset/snapshot map and synthetic byte markers — is wholly new test scaffolding, not an extension of existing fake state.

### Replication wire protocol

`keel-agentd` binds one new plain-TCP listener, `--replicate-addr`, its own port distinct from the HTTP API and from Milestone 16's per-VIP proxy ports. Like `proxy.rs`'s existing node-to-node relay, it carries no TLS of its own, relying on the same Milestone 14 LAN-trust boundary (an explicit Non-Goal above, not an oversight).

Framing: a small length-prefixed header (`replica_name`, `snapshot_id`, `base_snapshot_id: Option<String>`), then the raw `zfs send` byte stream until the sender closes the connection. On receipt:

1. Look up (or create, on first contact) the local `ReplicaTarget` for `replica_name`.
2. If `base_snapshot_id` doesn't match the target's own `last_snapshot`, reply with a one-byte "send full" error and close; the sender's next tick retries with `base: None`.
3. Otherwise, stream into `receive_snapshot`; on success, update `last_snapshot` to the new snapshot id.

This keeps both sides self-healing after any dropped or partial transfer with no extra coordination: a mismatch just triggers a fresh full send.

### `keel-agentd`: the replication loop

When `provision` creates a `JailRecord` whose spec has both `volumes` and `replicate_to` set, `keel-agentd` spawns one background thread for that replica — the same category of thing as its existing heartbeat-sender loop (`registration.rs`), just a second self-scheduled periodic loop rather than a new kind of mechanism. Every tick (~30s):

1. Re-read `replicate_to` from the on-disk `JailRecord`. This is how a `force-repin`'s `PUT /jails/<name>/replicate-to` call takes effect: no signal, no restart, just picked up on the next tick.
2. Snapshot the volume's dataset.
3. Connect to the standby's `replicate_to` address, send the header, then the `zfs send` stream (incremental from the last snapshot this loop successfully confirmed, or full if none yet or the standby rejected the base).
4. On success: prune the previous snapshot (keep exactly one — the new incremental base, no unbounded snapshot growth) and record the new one as last-confirmed-sent.
5. On any failure (unreachable standby, rejected stream, etc.): log and retry next tick. No backoff escalation, no fatal state — matches the "no rebalancing" Non-Goal directly.

### `force-repin`

`keelctl force-repin <replica-name>` → a new control-plane route, handled synchronously like every other client-facing route (the same `forward()`-based pattern `handle_scheduled_delete` already uses, no new async machinery):

1. Look up the replica's current node in `Placements`. **404** if `replica_name` isn't a currently-placed name at all, matching the existing "unplaced name" response shape everywhere else in this codebase.
2. Look up its standby in `Standbys`. **400** if there's no entry (not a stateful replica — a plain `kind: Jail` or stateless-service name never gets one).
3. `registry.resolve(current_node)` must fail (`Dead`/unresolvable). **409** if the current primary still resolves `Alive` — the split-brain guard.
4. Check the standby's `ReplicaTarget.last_snapshot` is `Some`. **409** if `None` (never completed a first full replication — nothing real to promote).
5. Pick a fresh standby (any `Alive` node other than the promoted node and the now-fenced old node, via the same selection logic used at initial scheduling).
6. Forward the replica's `JailSpec` to the (former) standby node's normal provision route, with `replicate_to` set to the fresh standby's address, and with volume creation skipped — the dataset already exists via `ReplicaTarget`, going through the same "don't recreate if present" idempotent path `create_volume` already has. The node starts the jail against the existing dataset and immediately begins a full-send baseline to its new standby (which has nothing yet).
7. On success: `Placements[replica_name] = new_node`, remove the old `Standbys` entry and set `Standbys[replica_name] = fresh_standby`, and add `PendingFences[replica_name] = old_node`.

### Fencing

Checked inside `reconcile_and_execute`, which already runs unconditionally on every heartbeat as this project's established piggyback-self-healing mechanism (the same spot Milestones 15/16 hook into): if the heartbeating node's id matches any `PendingFences` entry, synchronously forward a `DELETE /jails/<replica_name>` to it — the exact same call the scheduled-delete path already makes. On success or a `404` (already gone, e.g. an operator cleaned it up first), remove the `PendingFences` entry; on failure, leave it, so the next heartbeat from that node retries. No detection of "this is specifically the first heartbeat since the node came back" is needed or attempted — the entry just keeps firing on every heartbeat from that node id until it succeeds, which is simpler than latching a Dead-to-Alive transition (something the registry doesn't track today at all).

### Data flow

Apply a 2-replica stateful service "db" (`template.volumes: [{name: data, ...}]`) → scheduled exactly as Milestone 18 describes, `db-0` on node A, `db-1` on node B, plus this milestone's new step: the scheduler also picks standbys, say node C for `db-0` and node A for `db-1` → node A's `keel-agentd` starts replicating `db-0-data` to node C every ~30s (full send first, incremental after); node B does the same for `db-1-data` to node A → node A dies → the registry marks it `Dead`; Milestone 18's pinning keeps `db-0`'s and `db-1`'s standby-role-holding aside, `db-0`'s placement stays exactly as-is (still "present", per Milestone 18, node A) → an operator confirms node A is gone for good and runs `keelctl force-repin db-0` → the control plane verifies node A is `Dead`, node C's `ReplicaTarget` has a `last_snapshot`, forwards `db-0`'s spec to node C (skipping volume creation, pointing `replicate_to` at a freshly-chosen node D), node C starts the jail against its already-replicated dataset → `Placements["db-0"] = node C`, `Standbys["db-0"] = node D`, `PendingFences["db-0"] = node A` → node C begins a fresh full-send baseline of `db-0-data` to node D → node A eventually comes back online; its `keel-agentd` restarts and would resurrect `db-0` from its own stale `JailRecord`, but its first heartbeat to the control plane finds `PendingFences["db-0"] = node A` and gets a forced `DELETE /jails/db-0` pushed back at it inline, tearing down the stale jail before it can do any damage; `PendingFences["db-0"]` is cleared.

## Error Handling

- **Standby goes `Dead` while primary is `Alive`:** replication send attempts fail and retry every tick; no error surfaced to an operator, no automatic re-standby-selection (Non-Goal).
- **Both primary and standby `Dead` simultaneously:** `force-repin`'s step 5 forward itself fails with the ordinary "failed to reach node" error — the direct consequence of having exactly one standby, not a special case.
- **`force-repin` called twice in a row:** the second call's `Placements` lookup now shows the newly-promoted node as current primary; if it's `Alive` (the normal case), step 2's Dead-check fails with `409` — naturally idempotent-safe with no extra logic.
- **Old node returns after already being manually cleaned up by an operator:** fencing's forced `DELETE` gets a `404`, treated identically to success.
- **Incremental base mismatch** (standby's `last_snapshot` doesn't match what the primary believes it last sent): standby rejects, primary redoes a full send next tick. Self-healing, no operator step, per the wire protocol above.
- **`force-repin` against a name that was never a stateful replica:** `400` (no `Standbys` entry ever existed for it), distinct from the `404` for a name that was never placed at all.

## Testing Strategy

- **`keel-zfs`:** unit tests against `FakeZfsManager` for `snapshot`/`send_snapshot`/`receive_snapshot` — full vs. incremental byte streams, base-mismatch rejection surfaced correctly to the caller.
- **`keel-agentd`:** replication-loop tests against fakes — spawns on provision when `volumes` and `replicate_to` are both set, does not spawn otherwise; sends full on first tick, incremental thereafter; prunes the previous snapshot after a confirmed send; re-reads and retargets to a new `replicate_to` after the `PUT` endpoint updates the on-disk record; retries quietly (no panic, no fatal state) when the standby connection fails.
- **`keel-controlplane`:**
  - Scheduling: a stateful replica's first placement also produces a distinct, capacity-available `Standbys` entry.
  - `force-repin` happy path: promotes, updates `Placements`/`Standbys`/`PendingFences` exactly as described, and the forwarded spec has volume creation skipped and the correct fresh `replicate_to`.
  - `force-repin` refusals: `409` when the current primary still resolves `Alive`; `409` when the standby's `ReplicaTarget.last_snapshot` is `None`; `400` for a non-stateful or unplaced name.
  - Fencing: a heartbeat from a node with a matching `PendingFences` entry triggers the forced delete and clears the entry on success; a heartbeat from an unrelated node id leaves `PendingFences` untouched; a failed forced delete leaves the entry in place for the next heartbeat.
- **Real 3-node VM verification** (matching every prior milestone's closing step): apply a 2-replica stateful service; confirm each replica's standby (on a third or peer node) accumulates a real, growing dataset via periodic `zfs list`/checksums as data is written to the primary. Kill one replica's primary node's `keel-agentd` process; confirm the standby's replicated data is present and current within one replication interval. Run `force-repin`; confirm the jail comes up on the former standby with the data intact, and that a fresh standby is assigned and begins a new baseline replication. Power the original node back on; confirm its resurrected-from-`JailRecord` attempt is torn down by the fencing push within one heartbeat interval, leaving exactly one running copy of that replica cluster-wide.
