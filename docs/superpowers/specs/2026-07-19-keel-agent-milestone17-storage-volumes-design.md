# Milestone 17: Persistent Volumes on a Single Node (Sub-Project 7, First Milestone)

Status: Approved
Date: 2026-07-19

## Context

Every milestone through 16 treats a jail's entire filesystem as disposable:
`keel-agentd`'s `Reconciler::provision` clones a jail's rootfs dataset
(`{pool}/keel/jails/{name}`) from a base image, and `Reconciler::delete`
destroys that same dataset the instant the jail is deleted (`reconciler.rs`).
There is no way to declare data that should outlive a jail, and no mount
concept beyond the rootfs itself.

The README's roadmap lists "storage orchestration beyond a single host's
ZFS pool" as a future, not-yet-designed sub-project. Discussion before this
document settled the shape of that work: the interesting cross-node
questions (does a stateful service's data follow a rescheduled replica via
node-pinning, or via `zfs send`/`receive` replication?) are unanswerable
until a `Jail` can have data that outlives it *on one node* at all. This
milestone builds exactly that foundation, deliberately stopping at one node,
mirroring how Milestone 15 built the replica-set concept before Milestone 16
added cross-node load balancing on top of it.

Concretely: a jail can declare one or more named volumes; a volume's ZFS
dataset is created the first time a jail that references it is provisioned,
survives that jail being deleted, and is only ever destroyed by an explicit,
separate operation.

## Goals

- `keel-spec`'s `Spec` (`kind: Jail` only, see Non-Goals) gains
  `volumes: Vec<VolumeMount>`, defaulted to empty so every existing spec
  without the field keeps parsing unchanged:
  ```rust
  pub struct VolumeMount {
      pub name: String,
      #[serde(rename = "mountPath")]
      pub mount_path: String,
      pub size: String, // e.g. "1G" — same string-quantity convention as ResourcesSpec
  }
  ```
- `keel-zfs`'s `ZfsManager` gains `create_volume(dataset, quota) -> Result<(), ZfsError>`,
  a plain `zfs create -o quota=<size> <dataset>`, tolerant of "already
  exists" the same way `clone_from_base` already tolerates a pre-existing
  snapshot.
- `keel-jail` gains a new `MountManager` trait (plus `CliMountManager`/
  `FakeMountManager`, the same real/fake split every existing OS-interaction
  trait in this project uses):
  ```rust
  pub trait MountManager {
      fn mount_nullfs(&self, source: &Path, target: &Path) -> Result<(), MountError>;
      fn unmount(&self, target: &Path) -> Result<(), MountError>;
      fn is_mounted(&self, target: &Path) -> Result<bool, MountError>;
  }
  ```
  `CliMountManager` shells `mount -t nullfs`, `umount`, and `mount -p`
  (parsed for `target`'s path), matching this project's existing "shell the
  real command, never touch a syscall or fuse-style library directly"
  idiom.
- `Reconciler::provision` (`keel-agentd/src/reconciler.rs`), after cloning
  the rootfs and before `jails.create`, ensures each declared volume exists
  and is mounted: `mkdir -p` the mount point inside the (not-yet-jailed)
  rootfs path, `create_volume` if the dataset doesn't exist yet, `mount_nullfs`
  if `is_mounted` says it isn't already — the same idempotent-on-restart
  shape `clone_from_base`'s snapshot check already has, so an agentd
  restart with the jail already running and already mounted is a no-op.
- `Reconciler::delete` unmounts every declared volume (tolerating "not
  mounted" the same way it already tolerates `NotFound` from `destroy`/
  `destroy_dataset`) before destroying the rootfs dataset, and never calls
  `destroy_dataset` on a volume's own dataset — that is this milestone's
  entire "decoupled lifecycle" guarantee.
- `keel-agentd`'s HTTP layer gains `GET /volumes/<name>` and
  `DELETE /volumes/<name>`, dispatched through two new `Command` variants
  the same way `handle_get`/`handle_delete` already dispatch
  `Command::Get`/`Command::Delete` (`http.rs`). `DELETE` calls
  `zfs.destroy_dataset` directly (not gated through jail state); ZFS itself
  refuses to destroy a still-mounted dataset, so an in-use volume's delete
  surfaces that failure rather than corrupting a running jail (see Error
  Handling).
- `keel-controlplane`'s `route()` (`http.rs`) gains two forwarding-only
  arms, structurally identical to the existing `("GET"/"DELETE", ["nodes",
  id, "jails", name])` arms, reusing `handle_forward` verbatim:
  ```rust
  ("GET", ["nodes", id, "volumes", name]) => handle_forward(id, "GET", &format!("/volumes/{name}"), &[], commands, client_config),
  ("DELETE", ["nodes", id, "volumes", name]) => handle_forward(id, "DELETE", &format!("/volumes/{name}"), &[], commands, client_config),
  ```
  No registry, placement, or scheduling logic is added — a volume is never
  itself scheduled, it simply lives wherever the jail that first referenced
  it was already scheduled by Milestone 9's existing scheduler.
- `keelctl` gains a `delete-volume <name>` verb, reusing the existing
  `--node`/`--control-plane-addr` (or `--socket` for a bare single-node
  `keel-agentd`) targeting `main.rs` already has for jail/service commands.

## Non-Goals

- **`kind: Service`/`JailTemplate` never gain `volumes`.** A service
  replica can be rescheduled to any node by Milestone 15's existing
  self-healing reconciliation; attaching node-local data to something that
  moves without its data is the exact footgun this document's Context
  section already rejected. Stateful services are a distinct, later
  milestone that needs node-pinning or replication first.
- **No cross-node volume movement, replication, or scheduler
  volume-awareness.** A volume never leaves the node its dataset was
  created on. The scheduler (Milestone 9/10) is completely unmodified and
  has no idea volumes exist.
- **No sharing story.** Two jails referencing the same volume `name` on the
  same node both get the same dataset nullfs-mounted; nothing in this
  milestone prevents or coordinates concurrent writers. Single-writer is
  the only supported use case.
- **No live volume changes on an already-applied jail.** Changing
  `spec.volumes` on a re-apply is rejected outright (see Architecture), not
  reconciled by remounting.
- **No `kind: Volume` top-level resource.** Considered and rejected in
  favor of the inline `spec.volumes` field: a standalone resource would
  need `keel-controlplane` to route and place something that has no
  workload to schedule from, real added scope for a milestone deliberately
  kept single-node. Revisit this if/when cross-node replication needs a
  cluster-wide volume identity independent of any one jail.
- **Volume persistence across a full delete-then-recreate onto a different
  node.** See Open Questions: this is an accepted, bounded gap, not solved
  here.

## Architecture

### `keel-spec`: the `volumes` field and its validation

`Spec` gains `volumes: Vec<VolumeMount>` with `#[serde(default)]`. A new
`validate_volumes` (mirroring `validate_address`'s shape) checks each
`VolumeMount.name` with the existing `validate_name` (so a volume name is
constrained exactly like a jail name, since it becomes a ZFS dataset path
component), rejects a duplicate `name` within the same jail's list, and
parses `size` with a new `parse_zfs_quota` helper reusing `parse_memory_bytes`'s
existing K/M/G-suffix grammar (a ZFS quota and a memory size are the same
kind of quantity; no new grammar is invented). `validate_transition` gains
one more field comparison, `old.spec.volumes != new.spec.volumes` →
`SpecError::ImmutableField("spec.volumes")`, the same precedent `image` and
`network.address` already established: this milestone's reconciler has no
logic to diff and live-remount a running jail's volumes, so a change is
rejected rather than silently ignored or partially applied.

### `keel-zfs`: dataset creation without cloning

`clone_from_base` is specific to rootfs provisioning (snapshot-then-clone
from a shared base image); a volume is a plain, independent dataset with no
base image, so it gets its own method rather than reusing that one:
```rust
fn create_volume(&self, dataset: &str, quota: &str) -> Result<(), ZfsError>;
```
implemented as `zfs create -o quota=<quota> <dataset>`, tolerating a
"dataset already exists" failure by checking `dataset_exists` first, the
same existing idiom `clone_from_base` uses for its snapshot step.
`destroy_dataset` (already exists, already used for jail rootfs teardown)
is reused unchanged for explicit volume deletion — no new destroy method
needed.

### `keel-jail`: `MountManager`, a new trait alongside `JailRuntime`

Mounting is not jail-specific at the OS level (`mount(8)`/`umount(8)` know
nothing about `jail(8)`), but the mount target is always a path inside a
jail's rootfs, so this trait lives in `keel-jail` beside `JailRuntime`
rather than in `keel-zfs` (whose concern stops at the dataset, not the
mountpoint) or as a new crate (not enough surface to justify one).
`CliMountManager::is_mounted` runs `mount -p` (parseable, stable-format
output) and checks whether `target`'s path appears as a mountpoint column;
`FakeMountManager` keeps an in-memory `HashSet<PathBuf>` of currently
"mounted" targets, following the same in-memory-state idiom
`FakeZfsManager`/`FakeJailRuntime` already use. `MountError` follows the
exact same shape `JailError`/`ZfsError` already establish: `Spawn`,
`CommandFailed`, and a `NotMounted(PathBuf)` variant (the `unmount`-on-
already-unmounted case `Reconciler::delete` tolerates, the same way it
already tolerates `JailError::NotFound`/`ZfsError::NotFound`).

### `keel-agentd`: provisioning and teardown

`Reconciler::provision` gains, after `clone_from_base`/before `jails.create`
(mounts must exist before the jail that will see them starts):
```rust
for volume in &record.spec.spec.volumes {
    let dataset = record::volume_dataset_path(&self.pool, &volume.name);
    let target = rootfs.join(volume.mount_path.trim_start_matches('/'));
    std::fs::create_dir_all(&target)?; // maps to a new ReconcileError::Io variant
    if !self.zfs.dataset_exists(&dataset)? {
        self.zfs.create_volume(&dataset, &volume.size)?;
    }
    if !self.mounts.is_mounted(&target)? {
        self.mounts.mount_nullfs(&record::volume_mountpoint(&self.pool, &volume.name), &target)?;
    }
}
```
`record::volume_dataset_path(pool, name)` returns `{pool}/keel/volumes/{name}`,
structurally identical to the existing `jail_dataset_path`/`base_dataset_path`
helpers; `record::volume_mountpoint` is the corresponding `/{pool}/keel/volumes/{name}`
host path the dataset is mounted at by ZFS's own default (unset)
`mountpoint` property, the same convention `jail_rootfs_path` already
relies on for the rootfs dataset.

`Reconciler::delete` gains, before the existing `destroy_dataset(&jail_dataset)`
call (unmounting before destroying the rootfs dataset avoids the exact
"device busy" class of failure `keel-zfs/src/cli.rs`'s own `destroy_dataset`
retry loop was written against for the rootfs mount itself):
```rust
for volume in &record.spec.spec.volumes {
    let target = jail_rootfs_path(...).join(volume.mount_path.trim_start_matches('/'));
    match self.mounts.unmount(&target) {
        Ok(()) | Err(MountError::NotMounted(_)) => {}
        Err(e) => return Err(e.into()),
    }
}
```
The volume's own dataset is never destroyed here — that is the whole
point of this milestone.

### `keel-agentd` HTTP + `Command` channel

Two new `Command` variants, `GetVolume(String, Sender<Result<VolumeStatus, ReconcileError>>)`
and `DeleteVolume(String, Sender<Result<(), ReconcileError>>)`, handled by
the reconciler worker loop the same way `Command::Get`/`Command::Delete`
already are. `DeleteVolume` calls `zfs.destroy_dataset` directly against
`record::volume_dataset_path`; it does not consult `self.records` at all,
since a volume can outlive every jail record that ever referenced it (that
is, again, the entire point). `route()` dispatches `GET`/`DELETE
/volumes/<name>` to these, mirroring `handle_get`/`handle_delete`'s
existing shape.

### `keel-controlplane` + `keelctl`

Two forwarding-only route arms (Goals, above) reusing `handle_forward`
exactly as `/nodes/{id}/jails/...` already does — no new command type, no
new worker state. `keelctl` gets `delete-volume <name>`, sent as `DELETE
/volumes/<name>` directly against `--socket`, or `DELETE
/nodes/{node}/volumes/<name>` when routed through
`--control-plane-addr`/`--node` (identical branch structure to `main.rs`'s
existing jail/service dispatch).

### Data flow

Apply a `kind: Jail` "web-1" with one volume (`name: web-data, mountPath:
/data, size: 1G`) → scheduled or routed to a node exactly as any other jail
already is (Milestone 8/9, completely unmodified) → that node's
`provision` clones the rootfs, creates `zroot/keel/volumes/web-data`
(quota `1G`), nullfs-mounts it onto `<rootfs>/data`, then starts the jail →
the app inside writes to `/data` → `keelctl delete web-1` tears down the
jail and rootfs dataset, unmounts `/data`, but `zroot/keel/volumes/web-data`
survives → re-applying "web-1" (while its placement record still exists,
so Milestone 9's sticky re-apply keeps it on the same node) provisions a
fresh rootfs and jail, sees the volume dataset already exists, skips
`create_volume`, and remounts it — the app sees its old data immediately →
`keelctl delete-volume web-data --node <id>` (or `--socket` against a bare
node) destroys the dataset for good.

## Error Handling

- `spec.volumes` validation (name format, duplicate names, quota grammar)
  happens at the same `apply`-time validation step as every other spec
  field, before anything is persisted or provisioned — a malformed volume
  never reaches `provision`.
- Changing `spec.volumes` on an already-applied jail is rejected via the
  new `SpecError::ImmutableField("spec.volumes")`, the identical
  "identity is immutable, delete and re-apply instead" precedent `image`/
  `network.address` already carry.
- `create_dir_all` failing inside `provision` (e.g. a `mountPath` that
  collides with an existing file from the base image) surfaces as a new
  `ReconcileError::Io` variant and fails provisioning the same way any
  other `provision` step failure already does — cleaned up by the existing
  best-effort teardown path `provision`'s caller already runs on failure.
- `DELETE /volumes/<name>` on a still-mounted (i.e. still-referenced-by-a-
  running-jail) volume surfaces `zfs destroy`'s own "dataset is busy"
  failure as a `409`-class error (mirroring how `status_for_error` already
  maps `ImmutableField` to `409`) rather than force-unmounting anything —
  an operator must delete the jail first, an explicit two-step teardown
  rather than an implicit, surprising one.
- `DELETE /volumes/<name>` on a volume that was never created (typo'd name,
  or never provisioned) returns the existing `ZfsError::NotFound` mapped to
  `404`, the same mapping every other not-found case in this project
  already gets.
- A volume's `mount_nullfs`/`unmount` failing for a reason other than
  "already (un)mounted" propagates as a normal `ReconcileError`, failing
  that reconcile pass the same way a `JailError`/`ZfsError` already does —
  no special-cased silent tolerance beyond the two idempotency checks
  (`is_mounted`, dataset-already-exists) this document already calls out.

## Testing Strategy

- **`keel-spec`:** unit tests for `VolumeMount` (de)serialization
  (including the `#[serde(default)]` empty-list backward-compatibility
  case for existing YAML with no `volumes` key), `parse_zfs_quota`'s
  K/M/G grammar (mirroring the existing `parse_memory_bytes` test shapes),
  duplicate-name rejection, and a `validate_transition` test confirming a
  changed `volumes` list is rejected via `ImmutableField("spec.volumes")`,
  mirroring the existing `image`/`network.address` immutability tests.
- **`keel-zfs`:** `FakeZfsManager`-backed tests for `create_volume`
  (idempotent on an already-existing dataset); real, FreeBSD-VM-verified
  test for the actual `zfs create -o quota=...` invocation, matching every
  prior milestone's "verify the one genuinely OS-level part for real"
  discipline.
- **`keel-jail`:** `FakeMountManager`-backed unit tests for `mount_nullfs`/
  `unmount`/`is_mounted`'s in-memory bookkeeping; real VM-verified test for
  `CliMountManager` against an actual nullfs mount.
- **`keel-agentd`:** fake-backed reconciler tests: provisioning a jail with
  a volume creates the dataset and mounts it exactly once; re-provisioning
  after a simulated restart (dataset and mount already present) is a
  no-op, not a duplicate `create_volume`/`mount_nullfs` call; deleting the
  jail unmounts but leaves `FakeZfsManager`'s dataset present; re-applying
  and re-provisioning the same jail name afterward finds the dataset still
  there and remounts it without recreating it; an HTTP-layer test for
  `GET`/`DELETE /volumes/<name>` including the "still mounted" and
  "never existed" error-mapping cases above.
- **`keel-controlplane`:** an HTTP-layer test confirming `DELETE
  /nodes/{id}/volumes/{name}` forwards to the right node exactly like the
  existing `/nodes/{id}/jails/{name}` forwarding test does.
- **VM verification (single real node is sufficient; run against the
  existing 3-node fleet regardless, per this project's standing
  discipline):** apply a jail with a volume, write a file into the mounted
  path, delete the jail, confirm `zfs list` still shows the volume dataset,
  re-apply the same jail, confirm the file is still there; confirm
  `keelctl delete-volume` actually frees the dataset and fails cleanly
  while the jail is still using it; confirm a plain `kind: Jail` with no
  `volumes` is entirely unaffected (no new mounts, no behavior change).

## Open Questions / Deferred Decisions

- **Delete-then-recreate onto a different node silently starts empty.** If
  a jail with volumes is deleted and later re-applied under the same name
  after its placement record no longer exists, the scheduler may place it
  on a node other than the one holding the old data (Milestone 9's
  stickiness only holds while a placement record exists). That node has no
  way to know data exists elsewhere — there is no cluster-wide volume
  registry in this milestone — so it silently creates a fresh, empty
  volume rather than erroring. Detecting or preventing this needs exactly
  the cluster-wide volume tracking this milestone deliberately defers.
- **Stateful services (`kind: Service` + volumes) remain fully
  undesigned.** Whether the eventual mechanism is scheduler node-pinning
  (simple, but reintroduces a single point of failure per replica) or
  `zfs send`/`receive` replication (correct, but a project-sized effort of
  its own with its own consistency/lag story) is left for a later
  milestone in this sub-project to decide, once this foundation has been
  used in practice.
- **No quota-exceeded story beyond ZFS's own enforcement.** `size` sets a
  hard ZFS quota; what happens when an application fills it (ENOSPC
  surfaced directly to the jailed process) is not this milestone's
  concern, matching how `resources.cpu`/`memory` already rely entirely on
  `rctl`'s own enforcement rather than any Keel-level pre-check.
- **Whether a future `keelctl get-volume`/bare-collection verb is needed**
  is deferred the same way Milestone 16 deferred a `GET /services`
  `keelctl` verb — this milestone's operator surface is `delete-volume`
  only, matching exactly what Goals lists.
