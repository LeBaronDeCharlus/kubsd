# Milestone 18: Stateful Services via Node-Pinning (Sub-Project 7, Second Milestone)

Status: Approved

Date: 2026-07-19

## Context

Milestone 17 built persistent volumes for `kind: Jail`, deliberately stopping at one node and leaving `kind: Service`/`JailTemplate` without a `volumes` field at all: "Stateful services are a distinct, later milestone that needs node-pinning or replication first." That milestone's own Open Questions named the two candidate mechanisms and left the choice to "a later milestone in this sub-project to decide, once this foundation has been used in practice": whether the eventual mechanism is scheduler node-pinning (simple, but reintroduces a single point of failure per replica) or `zfs send`/`receive` replication (correct, but a project-sized effort of its own with its own consistency/lag story).

This milestone answers that question with node-pinning: a `kind: Service` whose `template` declares volumes gets each replica pinned to whichever node it was first placed on. Self-healing reconciliation, which today reschedules a replica onto a different node the moment its own node goes `Dead`, is changed to never do that for a stateful replica, trading automatic failover away in exchange for never silently creating a fresh, empty volume on a different node in place of one that still holds real data. `zfs send`/`receive` replication remains a distinct, undesigned, later effort in this sub-project, exactly as Milestone 17 left it.

Concretely: a service's `template` gains an optional `volumes` field; presence of any entries there makes every replica of that service stateful, each replica getting its own separate volume dataset (derived from its own unique replica name), pinned to its first-placement node for the life of that replica.

## Goals

- `keel-spec`'s `JailTemplate` gains `volumes: Vec<VolumeMount>` (`#[serde(default)]`, reusing the exact `VolumeMount` type Milestone 17 already defined, no new type). This is the field Milestone 17's own Non-Goals explicitly deferred adding.
- `keel-spec`'s `parse_and_validate_service` (or wherever `JailTemplate` is validated, the same call site validating `template.resources`/`template.image` today) additionally calls the existing `validate::validate_volumes(&spec.spec.template.volumes)`: no new validation function, Milestone 17's `validate_volumes` is reused verbatim against a second struct.
- `JailTemplate::to_jail_spec(name: &str, address: &str) -> JailSpec` (the function that already builds one replica's concrete spec from the template) now also maps `self.volumes` into that replica's own volumes: each `VolumeMount`'s `mount_path`/`size` pass through unchanged, but `name` becomes `format!("{name}-{volume_name}")` (e.g. replica `web-0`'s template volume `data` becomes the replica's own volume `web-0-data`). Since replica names are already globally unique (the existing name-collision check every `kind: Jail`/`kind: Service` replica already goes through), the derived per-replica volume names are automatically unique too, no new collision-checking logic anywhere.
- `keel-controlplane`'s `Command::ReconcileServices` handler (`worker.rs`) computes `present_indices` differently depending on whether a service is stateful (`!record.template.volumes.is_empty()`):
  - **Stateless (today's existing behavior, unchanged):** an index counts as present only if its recorded placement's node currently resolves as reachable (`registry.resolve(node_id, now).is_ok()`); a `Dead`-node replica drops out and gets rescheduled onto a different node.
  - **Stateful (new):** every placed index counts as present regardless of whether its node currently resolves. A replica pinned to a `Dead` node is neither torn down nor replaced; it simply waits. Everything downstream of `present_indices` (`diff_replicas`, the `to_add`/`to_remove` split, `ReplicaAction::Schedule`/`TearDown` execution) is unchanged code: this is the entire mechanism.
- No `keel-agentd` changes at all: `spec.volumes` handling in `Reconciler::provision`/`delete` (Milestone 17) already works for any `JailSpec`, regardless of whether it arrived via a plain `PUT /jails/<name>` or a service-scheduled `ReplicaAction::Schedule`'s own forwarded `PUT`.
- No new HTTP routes, no new `keelctl` verbs. `keelctl delete-volume <name> --node <id>` (Milestone 17) is already sufficient to clean up an orphaned per-replica volume after a scale-down.

## Non-Goals

- **No automatic failover.** A stateful replica whose pinned node is permanently gone stays down forever, by design. This milestone explicitly trades that away for correctness: it never silently creates a fresh, empty volume on a different node in place of one that still holds real data on the original node.
- **No operator "force re-pin" or "force reschedule" verb.** If a pinned node is genuinely gone for good, there is no way in this milestone to manually evict that one replica and let the scheduler place a fresh one elsewhere: `keelctl delete <replica-name>`, routed through the scheduled-delete path, forwards to the *recorded* node and fails (500) if that node is unreachable, since a placement is only cleared after a confirmed successful remote delete. This is an accepted, bounded gap, the same shape as Milestone 17's own accepted "delete-then-recreate onto a different node silently starts empty" gap; a later milestone could add an explicit force-unpin verb if this turns out to matter in practice.
- **No cross-node replication.** `zfs send`/`receive` (or any other data-movement mechanism) remains fully undesigned, exactly as Milestone 17 left it. Node-pinning is this sub-project's answer for now, not a stepping stone this milestone builds toward replication.
- **No partial statefulness within one service.** `volumes` lives on the one `template` every replica shares, so either every replica of a service is stateful (pinned, with its own volumes) or none are. There is no per-replica opt-out.
- **Scale-down does not clean up orphaned volumes**, matching Milestone 17's "a volume is only ever destroyed by an explicit, separate operation" principle exactly. Scaling a stateful service from `replicas: 3` to `replicas: 1` tears down `web-1`/`web-2`'s jails (as it already does for any service) but leaves `web-1-data`/`web-2-data` intact; `keelctl delete-volume web-1-data --node <id>` is the explicit cleanup path. Scaling back up to `3` later finds the old data still there rather than starting fresh, a deliberate consequence of the same principle, not a special case.
- **No change to `GET /services/<name>` discovery or the heartbeat's per-service proxy table.** Both already use the existing `Alive`+`running` filter (`healthy_replicas`), which is intentionally *stricter* than the pinning logic's health-blind `present_indices`: a pinned-but-currently-unreachable replica correctly disappears from what the load-balancing proxy actually routes traffic to, even though the control plane keeps its placement recorded and waits for it rather than replacing it.

## Architecture

### `keel-spec`: `JailTemplate.volumes`

`JailTemplate` gains:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailTemplate {
    pub image: String,
    pub command: Vec<String>,
    pub network: TemplateNetworkSpec,
    pub resources: ResourcesSpec,
    #[serde(rename = "restartPolicy")]
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
}
```

`parse_and_validate_service` gains one more call, alongside its existing `resources::parse_cpu_cores(&spec.spec.template.resources.cpu)`/`parse_memory_bytes` calls:

```rust
validate::validate_volumes(&spec.spec.template.volumes)?;
```

`JailTemplate::to_jail_spec` gains the per-replica volume-name rewrite:

```rust
pub fn to_jail_spec(&self, name: &str, address: &str) -> JailSpec {
    JailSpec {
        api_version: "keel/v1".to_string(),
        kind: "Jail".to_string(),
        metadata: Metadata { name: name.to_string() },
        spec: Spec {
            image: self.image.clone(),
            command: self.command.clone(),
            network: NetworkSpec {
                vnet: self.network.vnet,
                bridge: self.network.bridge.clone(),
                address: address.to_string(),
            },
            resources: self.resources.clone(),
            restart_policy: self.restart_policy,
            volumes: self
                .volumes
                .iter()
                .map(|v| VolumeMount {
                    name: format!("{name}-{}", v.name),
                    mount_path: v.mount_path.clone(),
                    size: v.size.clone(),
                })
                .collect(),
        },
    }
}
```

Since `Services::apply` already rejects any `template` change on an existing service (`ApplyServiceError::TemplateChanged`), a stateful service's `volumes` are already immutable once created: no new immutability logic needed on top of what Milestone 15 already built.

### `keel-controlplane`: pinning in `Command::ReconcileServices`

The only behavioral change. Today's `present_indices` computation in `worker.rs`'s `Command::ReconcileServices` handler:

```rust
let present_indices: BTreeSet<u32> = placed
    .iter()
    .filter(|(_, _, node_id)| registry.resolve(node_id, now).is_ok())
    .map(|(idx, _, _)| *idx)
    .collect();
```

becomes:

```rust
let present_indices: BTreeSet<u32> = if record.template.volumes.is_empty() {
    placed
        .iter()
        .filter(|(_, _, node_id)| registry.resolve(node_id, now).is_ok())
        .map(|(idx, _, _)| *idx)
        .collect()
} else {
    // Stateful: a placement is "present" regardless of whether its node
    // currently resolves. A replica pinned to a Dead node is neither torn
    // down nor replaced elsewhere, it simply waits for that node to come
    // back, since keel-agentd persists its own jail records to disk and
    // will reconcile the replica back to running on its own once its
    // process (or the node) returns, with no control-plane involvement.
    // This is the entire node-pinning mechanism: everything downstream
    // (diff_replicas, to_add/to_remove, ReplicaAction execution) is
    // unchanged.
    placed.iter().map(|(idx, _, _)| *idx).collect()
};
```

`diff_replicas`, `to_add`, `to_remove`, and the `ReplicaAction::Schedule`/`TearDown` execution paths are entirely unchanged. This is deliberate: `to_remove`'s existing handling already tolerates a currently-unreachable node gracefully (`let Ok(node_addr) = registry.resolve(&node_id, now) else { continue }`, skipping that tick and retrying on a later one), so scaling a stateful service down still works correctly even if the replica being removed happens to be on an unreachable node at that exact moment.

A brand-new stateful service (no placements recorded yet) schedules its first `desired_replicas` exactly like any other service: `present_indices` is empty either way when nothing is placed, so the stateful/stateless branch only diverges once at least one replica has an existing placement.

### Data flow

Apply a `kind: Service` "db" with `replicas: 2`, `template.volumes: [{name: data, mountPath: /var/db, size: 5G}]` → scheduled exactly like any service today, landing `db-0` on node A and `db-1` on node B (same-service spreading, unchanged) → each node's `provision` (Milestone 17, unmodified) creates and mounts `db-0-data`/`db-1-data` respectively → node A's `keel-agentd` process dies (or the node itself goes down) → the control plane's registry marks node A `Dead` within the existing 15-second threshold → the next `ReconcileServices` tick sees `db`'s `template.volumes` non-empty, so `db-0`'s placement (still recorded, pointing at node A) counts as present regardless of node A's `Dead` status → `db-0` is neither torn down nor rescheduled onto node C; `db-1` keeps serving on node B, and `GET /services/db` correctly reports only `db-1` as healthy (the existing `Alive`+`running` filter is untouched) → node A comes back and its `keel-agentd` restarts → its own persisted `JailRecord` for `db-0` (Milestone 4's crash-safe on-disk state, never touched by any of this) reconciles `db-0` back to running, its dataset and mount already exist and are already correct → node A re-registers, goes `Alive` again, and the next `ReconcileServices` tick sees `db-0`'s placement (which was never removed) resolving successfully again, with nothing further to do.

## Error Handling

- `spec.template.volumes` validation (name format, duplicate names, quota grammar) happens at the same `apply`-time validation step as every other template field, via the exact same `validate_volumes` function Milestone 17 already built and tested: no new error variant, no new validation code path.
- A `template.volumes` change on an already-applied service is rejected via the existing `ApplyServiceError::TemplateChanged`, since `volumes` is just one more field of the already-fully-immutable `template`: no new error variant needed.
- A stateful replica's node going `Dead` produces no error at all from `ReconcileServices`: it's a normal, expected state (the replica is "present but unreachable"), not a failure. `GET /services/<name>` continues to simply omit it from the healthy list, the same way it already omits a crash-looping-but-`Alive`-node replica.
- Scaling a stateful service down while the to-be-removed replica's node is unreachable is not an error either: `to_remove`'s existing `let ... else { continue }` tolerance (already present, unmodified) simply retries that teardown on a later `ReconcileServices` tick once the node becomes reachable again.
- Manually deleting one stateful replica by name while its pinned node is unreachable surfaces the ordinary `500 failed to reach node` error the scheduled-delete route already gives for any unreachable node: this milestone adds no special handling for that case, per Non-Goals above.

## Testing Strategy

- **`keel-spec`:** unit tests for `JailTemplate.volumes` (de)serialization, including the empty-list backward-compatibility case for existing `kind: Service` YAML with no `volumes` key under `template` (mirroring Milestone 17's identical test for `Spec.volumes`); a `validate_volumes`-via-`parse_and_validate_service` test confirming a malformed template volume (bad name, duplicate name, bad quota) is rejected the same way Milestone 17's own tests already prove for `kind: Jail`; a `to_jail_spec` test confirming a template volume named `data` becomes `web-0-data` for replica name `web-0` and `web-1-data` for replica name `web-1`, with `mountPath`/`size` unchanged, and a service with an empty `volumes` list produces replicas with an empty `volumes` list too (no behavior change for existing, non-stateful services).
- **`keel-controlplane`:** fake-backed `Command::ReconcileServices` tests: a stateful service (non-empty `template.volumes`) whose one replica's node has gone `Dead` produces neither a `Schedule` nor a `TearDown` action for that index (the core pinning regression test); a stateless service (empty `template.volumes`) in the identical scenario still produces a `Schedule` action rescheduling that index elsewhere, proving the existing self-healing behavior for ordinary services is completely unchanged; scaling a stateful service down (`desired_replicas` decreased) while the to-be-removed replica's node is `Dead` produces no action that tick (skipped, not an error) and a `TearDown` action once that node is `Alive` again on a later tick; a brand-new stateful service with zero existing placements schedules its full `desired_replicas` normally, spread across distinct nodes exactly like a stateless service does.
- **VM verification (3 real nodes, per this project's standing discipline):** apply a 2-replica stateful service; write distinct, per-replica data into each replica's mounted volume; confirm both are running on different nodes with correct, distinct data. Kill one replica's node's `keel-agentd` process (not the VM) to simulate `Dead`; confirm the surviving replica keeps serving via `GET /services/<name>` (only it listed as healthy); confirm the dead replica's index is *not* recreated anywhere else (`jls` on the third node shows nothing new); confirm the dead replica's volume dataset is untouched (`zfs list` on its original node, once reachable again, still shows it). Restart that node's `keel-agentd`; confirm the pinned replica comes back running on the same node with its data intact, with no control-plane action taken. Scale the service down by one and confirm the removed replica's volume survives, cleaned up explicitly via `keelctl delete-volume`; scale back up and confirm the old data is found intact rather than starting empty.
