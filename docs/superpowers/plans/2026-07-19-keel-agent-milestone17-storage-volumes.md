# Milestone 17: Persistent Volumes on a Single Node Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `kind: Jail` can declare one or more named volumes; a volume's ZFS dataset is created the first time a jail referencing it is provisioned, survives that jail being deleted, and is destroyed only by an explicit, separate `DELETE /volumes/<name>` (forwarded through `keel-controlplane`, or direct against a bare `keel-agentd`).

**Architecture:** `keel-spec`'s `Spec` gains `volumes: Vec<VolumeMount>` (`#[serde(default)]`), validated by a new `validate_volumes` and rejected from changing via `validate_transition`. `keel-zfs` gains `create_volume` (plain `zfs create -o quota=...`, no cloning) and a `ZfsError::Busy` variant so an exhausted-retries "dataset is busy" `destroy_dataset` failure is distinguishable from other failures. `keel-jail` gains a new `MountManager` trait (`CliMountManager`/`FakeMountManager`) for `mount -t nullfs`/`umount`/`mount -p`, independent of `JailRuntime`. `keel-agentd`'s `Reconciler` gains a fourth generic type parameter `M: MountManager`; `provision` creates+mounts each declared volume before starting the jail, `delete` unmounts (never destroys) each one before destroying the rootfs dataset. Two new HTTP verbs, `GET`/`DELETE /volumes/<name>`, act purely against ZFS keyed by dataset path, never consulting jail records. `keel-controlplane` gains two forwarding-only route arms reusing `handle_forward` verbatim; `keelctl` gains a `delete-volume <name>` verb.

**Tech Stack:** Rust (2021 edition), `serde`/`serde_yaml` (wire types), plain `std::process::Command` (shelling `zfs`/`mount`/`umount`), no new dependencies anywhere in this milestone.

## Global Constraints

- `kind: Service`/`JailTemplate` never gain `volumes` — see the design spec's Non-Goals. This milestone touches `Spec` (the `kind: Jail` body) only, never `ServiceSpecBody`/`JailTemplate`.
- A volume's dataset is never destroyed by anything other than the new explicit `DELETE /volumes/<name>` — not by `Reconciler::delete`, not by a re-provision.
- `spec.volumes` is immutable once a jail is created: a re-apply with a changed `volumes` list is rejected with `SpecError::ImmutableField("spec.volumes")`, never diffed/partially remounted.
- `create_volume` never creates its parent dataset (`{pool}/keel/volumes`) — that is a one-time, out-of-band per-node prerequisite, exactly like `{pool}/keel/jails` already is for `clone_from_base`.
- Design reference: `docs/superpowers/specs/2026-07-19-keel-agent-milestone17-storage-volumes-design.md` (Approved). Follow it exactly; every place this plan makes an implementation decision the spec left open is called out inline with its rationale.

---

## Facts about the current codebase this plan relies on

Gathered by reading the actual current source, not assumed from the design spec:

- `keel-spec/src/types.rs`'s `Spec` (the `kind: Jail` body) has no `volumes` field today. `ServiceSpecBody`/`JailTemplate` are separate types and are out of scope (Non-Goals).
- `keel-spec/src/validate.rs`'s `validate_transition(old: &JailSpec, new: &JailSpec)` currently checks only `spec.image` and `spec.network.address`, each via `SpecError::ImmutableField(&'static str)`. `validate_name`/`validate_address` live here too.
- `keel-spec/src/resources.rs`'s `parse_memory_bytes(s: &str) -> Result<u64, SpecError>` already implements the exact K/M/G-suffix grammar the design spec wants `parse_zfs_quota` to reuse — reuse it directly (a thin wrapper), no new grammar.
- `keel-spec/src/error.rs`'s `SpecError` has `Yaml`, `InvalidName`, `InvalidAddress`, `InvalidCpu`, `InvalidMemory`, `InvalidPort`, `ImmutableField(&'static str)`. No duplicate-name variant exists yet for anything.
- `keel-zfs/src/lib.rs`'s `ZfsManager` trait has `dataset_exists`, `clone_from_base`, `destroy_dataset` only — no `create_volume`. `keel-zfs/src/error.rs`'s `ZfsError` has `Spawn`, `CommandFailed`, `NotFound` only — no `Busy`.
- `keel-zfs/src/cli.rs`'s `CliZfsManager::destroy_dataset` already retries up to 10 times on a "dataset is busy" stderr match (100ms sleep between attempts), and separately detects "dataset does not exist" stderr as `ZfsError::NotFound` — but once retries are exhausted on a still-busy dataset, it falls through to `Err(last_err.unwrap())`, which is a plain `CommandFailed`, indistinguishable at the type level from any other failure. This is exactly what `ZfsError::Busy` needs to fix.
- `keel-zfs/src/fake.rs`'s `FakeZfsManager` holds one `Mutex<HashSet<String>>` of dataset names; `destroy_dataset` removes-or-`NotFound`. It has no notion of "busy" today — this plan adds a second `Mutex<HashSet<String>>` plus a `mark_busy` test helper, the same idiom `FakeJailRuntime::fail_start_command`/`mark_exited` already establish for injecting a specific failure mode into an otherwise-successful fake.
- `keel-jail/src/lib.rs`'s `JailRuntime` trait/`CliJailRuntime`(named `ProcessJailRuntime`)/`FakeJailRuntime` split is the exact real/fake precedent `MountManager` follows. `keel-jail/src/error.rs`'s `JailError` has `Spawn`, `CommandFailed`, `NotFound` — `MountError`'s shape (`Spawn`, `CommandFailed`, `NotMounted(PathBuf)`) mirrors it.
- `keel-agentd/src/record.rs` has `jail_name`, `base_dataset_path`, `jail_dataset_path`, `jail_rootfs_path`, `epair_base_name` — all plain string-formatting helpers with dense unit tests. `volume_dataset_path`/`volume_mountpoint` follow the exact same shape.
- `keel-agentd/src/reconciler.rs`'s `Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager>` has fields `jails, zfs, net, pool, state_dir, records, backoff, next_epair_ordinal`; `provision` runs `clone_from_base` → `jails.create` → `configure_networking_and_limits` → `start_command`; `delete` runs `detach_jail` → `jails.destroy` (tolerating `NotFound`) → `zfs.destroy_dataset` (tolerating `NotFound`) → `remove_resource_limits` (tolerating `NotFound`) → `store::remove`. `ReconcileError` wraps `SpecError`/`StoreError`/`JailError`/`ZfsError`/`NetError` via `#[from]`, plus `NotFound(String)` and `BaseImageNotFound(String)` — no `Io` variant yet.
- `Reconciler::new(jails: J, zfs: Z, net: N, pool: String, state_dir: PathBuf) -> Result<Self, ReconcileError>` is called at **13 sites** across the crate: `keel-agentd/src/reconciler.rs:297` (own test helper, edited directly in Task 5), `worker.rs:127,244`, `registration.rs:442,475,539,610,640,717,777` (all via fully-qualified `keel_jail::FakeJailRuntime::new()`/`keel_zfs::FakeZfsManager::new()`/`keel_net::FakeNetManager::new()`, never an imported bare name), `http.rs:349,371,541,738,783`, `main.rs:84` (real `ProcessJailRuntime`/`CliZfsManager`/`ProcessNetManager`), plus **2 more** in `keelctl/tests/cli.rs:21,71` (a dev-dependency integration test, fully-qualified the same way). Every one of these 15 call sites needs a `MountManager` argument inserted between the `NetManager` argument and the `pool` string argument once `Reconciler::new` gains a fourth parameter (Task 6).
- `worker::spawn<J: JailRuntime, Z: ZfsManager, N: NetManager>(reconciler: Reconciler<J, Z, N>)` (`worker.rs:23`) needs a fourth `M: MountManager` bound; every one of its own callers already just passes a `Reconciler` value through, so no call site here needs an *extra argument* (only the two explicit `Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager>` type annotations at `reconciler.rs:296` and `proxy.rs:164` need a fourth type argument).
- `keel-agentd/src/worker.rs`'s `Command` enum already has `AddServiceAlias`/`RemoveServiceAlias(String, String, Sender<Result<(), keel_net::NetError>>)` as the precedent for a command that bypasses `self.records` entirely and calls straight through to a `Reconciler` passthrough method (`reconciler.add_alias`/`remove_alias`) — `GetVolume`/`DeleteVolume` follow the identical shape.
- `keel-agentd/src/http.rs`'s `route()` dispatches on `(method, path_segments)`; `status_for_error` today has arms only for `ReconcileError::InvalidSpec(SpecError::ImmutableField(_)) => 409`, `InvalidSpec(_) => 400`, `NotFound(_) => 404`, `_ => 500` — no arm for `ReconcileError::Zfs(_)` at all, so both new cases would incorrectly fall through to `500` without this plan's Task 8 additions.
- `keel-agentd/src/wire.rs` has `JailStatus`, `BackoffStatus`, `ErrorBody` — no `VolumeStatus` yet.
- `keel-controlplane/src/http.rs`'s `route()` has `("GET", ["nodes", id, "jails", name])`/`("DELETE", ["nodes", id, "jails", name])` arms that call `handle_forward(id, "GET"/"DELETE", &format!("/jails/{name}"), &[], commands, client_config)` verbatim — the exact structural precedent Task 10's two new arms copy.
- `keelctl/src/main.rs` has no per-kind CLI verbs beyond `apply -f FILE`/`get [name]`/`delete NAME` (which try `/jails/<name>` then fall back to `/services/<name>` on a 404). `delete-volume <name>` is a new, separate top-level verb (Goals: `keelctl` gains `delete-volume <name>`, not a fallback branch of `delete`), dispatched via the same `Target`/`dispatch`/`jails_path`-shaped helper this file already has for `/jails`, but hitting `/volumes/<name>` — needs its own tiny path helper since `jails_path`'s name is jail-specific in name only (it already works for any suffix, including `/services/...` in `run_apply`, so it's reused as-is here too, not renamed).

---

### Task 1: `keel-spec` — `VolumeMount`, `validate_volumes`, `parse_zfs_quota`

**Files:**
- Modify: `keel-spec/src/types.rs`
- Modify: `keel-spec/src/error.rs`
- Modify: `keel-spec/src/resources.rs`
- Modify: `keel-spec/src/validate.rs`
- Modify: `keel-spec/src/lib.rs`
- Test: same files' `#[cfg(test)]` modules

**Interfaces:**
- Consumes: nothing new.
- Produces: `keel_spec::types::VolumeMount { name: String, mount_path: String, size: String }` (re-exported from `keel_spec::VolumeMount`); `Spec.volumes: Vec<VolumeMount>` (`#[serde(default)]`); `SpecError::DuplicateVolumeName(String)`; `resources::parse_zfs_quota(s: &str) -> Result<u64, SpecError>`; `validate::validate_volumes(volumes: &[VolumeMount]) -> Result<(), SpecError>`; `validate_transition` rejects a changed `volumes` list via `SpecError::ImmutableField("spec.volumes")`.

- [ ] **Step 1: Write the failing tests**

In `keel-spec/src/types.rs`'s `#[cfg(test)] mod tests`, add a volume-bearing fixture and tests after `parses_the_design_spec_example_yaml`:

```rust
    const EXAMPLE_YAML_WITH_VOLUME: &str = r#"
apiVersion: keel/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.0.5/24
  resources:
    cpu: "2"
    memory: "512M"
  restartPolicy: Always
  volumes:
    - name: web-data
      mountPath: /data
      size: 1G
"#;

    #[test]
    fn parses_a_jail_with_one_volume() {
        let spec: JailSpec = serde_yaml::from_str(EXAMPLE_YAML_WITH_VOLUME).unwrap();
        assert_eq!(spec.spec.volumes.len(), 1);
        assert_eq!(spec.spec.volumes[0].name, "web-data");
        assert_eq!(spec.spec.volumes[0].mount_path, "/data");
        assert_eq!(spec.spec.volumes[0].size, "1G");
    }

    #[test]
    fn a_jail_with_no_volumes_key_parses_with_an_empty_list() {
        let spec: JailSpec = serde_yaml::from_str(EXAMPLE_YAML).unwrap();
        assert_eq!(spec.spec.volumes, vec![]);
    }
```

In `keel-spec/src/resources.rs`'s `#[cfg(test)] mod tests`, add after `rejects_invalid_memory_values`:

```rust
    #[test]
    fn parse_zfs_quota_accepts_the_same_grammar_as_memory() {
        assert_eq!(parse_zfs_quota("1G"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_zfs_quota("512M"), Ok(512 * 1024 * 1024));
    }

    #[test]
    fn parse_zfs_quota_rejects_the_same_malformed_input_as_memory() {
        assert!(parse_zfs_quota("0G").is_err());
        assert!(parse_zfs_quota("abc").is_err());
    }
```

In `keel-spec/src/validate.rs`'s `#[cfg(test)] mod tests`, add after `rejects_malformed_addresses`:

```rust
    fn volume(name: &str, mount_path: &str, size: &str) -> VolumeMount {
        VolumeMount { name: name.to_string(), mount_path: mount_path.to_string(), size: size.to_string() }
    }

    #[test]
    fn validate_volumes_accepts_an_empty_list() {
        assert!(validate_volumes(&[]).is_ok());
    }

    #[test]
    fn validate_volumes_accepts_well_formed_distinct_volumes() {
        let volumes = vec![volume("web-data", "/data", "1G"), volume("web-cache", "/cache", "512M")];
        assert!(validate_volumes(&volumes).is_ok());
    }

    #[test]
    fn validate_volumes_rejects_a_malformed_name() {
        let volumes = vec![volume("Invalid_Name", "/data", "1G")];
        assert!(matches!(validate_volumes(&volumes), Err(SpecError::InvalidName(_))));
    }

    #[test]
    fn validate_volumes_rejects_a_duplicate_name() {
        let volumes = vec![volume("web-data", "/data", "1G"), volume("web-data", "/other", "2G")];
        assert_eq!(validate_volumes(&volumes), Err(SpecError::DuplicateVolumeName("web-data".to_string())));
    }

    #[test]
    fn validate_volumes_rejects_a_malformed_size() {
        let volumes = vec![volume("web-data", "/data", "not-a-size")];
        assert!(matches!(validate_volumes(&volumes), Err(SpecError::InvalidMemory(_))));
    }
```

And after `rejects_changing_network_address`:

```rust
    #[test]
    fn rejects_changing_volumes() {
        let old = sample_spec();
        let mut new = sample_spec();
        new.spec.volumes = vec![volume("web-data", "/data", "1G")];
        assert_eq!(validate_transition(&old, &new), Err(SpecError::ImmutableField("spec.volumes")));
    }

    #[test]
    fn allows_reapplying_with_the_same_volumes() {
        let mut old = sample_spec();
        old.spec.volumes = vec![volume("web-data", "/data", "1G")];
        let new = old.clone();
        assert!(validate_transition(&old, &new).is_ok());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-spec`
Expected: FAIL to compile — `volumes` is not a field of `Spec`, `VolumeMount`/`DuplicateVolumeName`/`validate_volumes`/`parse_zfs_quota` don't exist.

- [ ] **Step 3: Implement**

In `keel-spec/src/types.rs`, add the new type and extend `Spec`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeMount {
    pub name: String,
    #[serde(rename = "mountPath")]
    pub mount_path: String,
    pub size: String,
}
```

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub image: String,
    pub command: Vec<String>,
    pub network: NetworkSpec,
    pub resources: ResourcesSpec,
    #[serde(rename = "restartPolicy")]
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
}
```

In `keel-spec/src/error.rs`, add after `InvalidPort`:

```rust
    #[error("duplicate volume name '{0}' in spec.volumes")]
    DuplicateVolumeName(String),
```

In `keel-spec/src/resources.rs`, add after `parse_memory_bytes`:

```rust
/// A ZFS quota and a memory size are the same kind of quantity (a plain
/// byte count with an optional K/M/G suffix) — reuses `parse_memory_bytes`'s
/// grammar directly rather than inventing a new one.
pub fn parse_zfs_quota(s: &str) -> Result<u64, SpecError> {
    parse_memory_bytes(s)
}
```

In `keel-spec/src/validate.rs`, add after `validate_address`:

```rust
pub fn validate_volumes(volumes: &[crate::types::VolumeMount]) -> Result<(), SpecError> {
    let mut seen = std::collections::HashSet::new();
    for volume in volumes {
        validate_name(&volume.name)?;
        if !seen.insert(volume.name.clone()) {
            return Err(SpecError::DuplicateVolumeName(volume.name.clone()));
        }
        crate::resources::parse_zfs_quota(&volume.size)?;
    }
    Ok(())
}
```

And extend `validate_transition`:

```rust
pub fn validate_transition(old: &crate::types::JailSpec, new: &crate::types::JailSpec) -> Result<(), SpecError> {
    if old.spec.image != new.spec.image {
        return Err(SpecError::ImmutableField("spec.image"));
    }
    if old.spec.network.address != new.spec.network.address {
        return Err(SpecError::ImmutableField("spec.network.address"));
    }
    if old.spec.volumes != new.spec.volumes {
        return Err(SpecError::ImmutableField("spec.volumes"));
    }
    Ok(())
}
```

In `keel-spec/src/lib.rs`, wire `validate_volumes` into `parse_and_validate` and re-export `VolumeMount`:

```rust
pub use types::{
    JailSpec, JailTemplate, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, ServiceSpec,
    ServiceSpecBody, Spec, TemplateNetworkSpec, VolumeMount,
};
pub use validate::{validate_address, validate_name, validate_transition, validate_volumes};

pub fn parse_and_validate(yaml: &str) -> Result<JailSpec, SpecError> {
    let spec: JailSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    validate::validate_address(&spec.spec.network.address)?;
    resources::parse_cpu_cores(&spec.spec.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.resources.memory)?;
    validate::validate_volumes(&spec.spec.volumes)?;
    Ok(spec)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-spec`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-spec/src/types.rs keel-spec/src/error.rs keel-spec/src/resources.rs keel-spec/src/validate.rs keel-spec/src/lib.rs
git commit -m "Add spec.volumes to kind: Jail, with validation and immutability"
```

---

### Task 2: `keel-zfs` — `ZfsError::Busy`, `create_volume`

**Files:**
- Modify: `keel-zfs/src/error.rs`
- Modify: `keel-zfs/src/lib.rs`
- Modify: `keel-zfs/src/cli.rs`
- Modify: `keel-zfs/src/fake.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `ZfsError::Busy(String)`; `ZfsManager::create_volume(&self, dataset: &str, quota: &str) -> Result<(), ZfsError>`; `FakeZfsManager::mark_busy(&self, dataset: &str)` (test helper); `CliZfsManager::destroy_dataset`/`FakeZfsManager::destroy_dataset` both return `Busy` instead of `CommandFailed`/silently succeeding when the dataset is busy.

- [ ] **Step 1: Write the failing tests**

In `keel-zfs/src/fake.rs`'s `#[cfg(test)] mod tests`, add after `destroy_dataset_on_unknown_dataset_returns_not_found`:

```rust
    #[test]
    fn create_volume_creates_the_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.create_volume("zroot/keel/volumes/web-data", "1G").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), true);
    }

    #[test]
    fn create_volume_is_idempotent_on_an_already_existing_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.create_volume("zroot/keel/volumes/web-data", "1G").unwrap();
        zfs.create_volume("zroot/keel/volumes/web-data", "1G").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), true);
    }

    #[test]
    fn destroy_dataset_on_a_busy_dataset_returns_busy_and_leaves_it_present() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.mark_busy("zroot/keel/volumes/web-data");
        assert!(matches!(zfs.destroy_dataset("zroot/keel/volumes/web-data"), Err(ZfsError::Busy(_))));
        assert_eq!(zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), true);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-zfs`
Expected: FAIL to compile — `create_volume`/`mark_busy`/`ZfsError::Busy` don't exist.

- [ ] **Step 3: Implement**

In `keel-zfs/src/error.rs`, add after `NotFound`:

```rust
    #[error("dataset '{0}' is busy")]
    Busy(String),
```

In `keel-zfs/src/lib.rs`, extend the trait:

```rust
pub trait ZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError>;

    /// The `target_dataset`'s parent must already exist — this method does
    /// not create parent datasets (no `-p`).
    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError>;

    /// A plain, independent dataset with a hard quota and no base image —
    /// distinct from `clone_from_base`, which always clones from a shared
    /// base snapshot. Idempotent: a dataset that already exists is left
    /// untouched (its quota is not re-applied).  Like `clone_from_base`,
    /// this does not create `target_dataset`'s parent (no `-p`).
    fn create_volume(&self, dataset: &str, quota: &str) -> Result<(), ZfsError>;

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError>;
}
```

In `keel-zfs/src/cli.rs`, add `create_volume` and fix `destroy_dataset`'s exhausted-retries case:

```rust
    fn create_volume(&self, dataset: &str, quota: &str) -> Result<(), ZfsError> {
        if self.dataset_exists(dataset)? {
            return Ok(());
        }
        Self::run_checked(&["create", "-o", &format!("quota={quota}"), dataset])
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        let mut last_err = None;
        let mut last_was_busy = false;
        for _ in 0..10 {
            match Self::run_checked(&["destroy", dataset]) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if matches!(&e, ZfsError::CommandFailed(_, _, stderr) if stderr.contains("dataset does not exist"))
                    {
                        return Err(ZfsError::NotFound(dataset.to_string()));
                    }
                    let is_busy =
                        matches!(&e, ZfsError::CommandFailed(_, _, stderr) if stderr.contains("dataset is busy"));
                    last_was_busy = is_busy;
                    last_err = Some(e);
                    if !is_busy {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        if last_was_busy {
            return Err(ZfsError::Busy(dataset.to_string()));
        }
        Err(last_err.unwrap())
    }
```

(`clone_from_base` is unchanged.)

In `keel-zfs/src/fake.rs`, add a second tracked set and wire it in:

```rust
#[derive(Default)]
pub struct FakeZfsManager {
    datasets: Mutex<HashSet<String>>,
    busy: Mutex<HashSet<String>>,
}
```

```rust
    /// Test helper: makes `destroy_dataset` return `ZfsError::Busy` for
    /// this dataset instead of removing it — simulates a volume still
    /// nullfs-mounted by a running jail, since this in-memory fake has no
    /// real mount awareness of its own.
    pub fn mark_busy(&self, dataset: &str) {
        self.busy.lock().unwrap().insert(dataset.to_string());
    }
```

```rust
    fn create_volume(&self, dataset: &str, _quota: &str) -> Result<(), ZfsError> {
        self.datasets.lock().unwrap().insert(dataset.to_string());
        Ok(())
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        if self.busy.lock().unwrap().contains(dataset) {
            return Err(ZfsError::Busy(dataset.to_string()));
        }
        if self.datasets.lock().unwrap().remove(dataset) {
            Ok(())
        } else {
            Err(ZfsError::NotFound(dataset.to_string()))
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-zfs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-zfs/src/error.rs keel-zfs/src/lib.rs keel-zfs/src/cli.rs keel-zfs/src/fake.rs
git commit -m "Add ZfsManager::create_volume and a distinguishable ZfsError::Busy"
```

---

### Task 3: `keel-jail` — `MountManager` trait, `CliMountManager`, `FakeMountManager`

**Files:**
- Create: `keel-jail/src/mount_error.rs`
- Create: `keel-jail/src/mount_cli.rs`
- Create: `keel-jail/src/mount_fake.rs`
- Modify: `keel-jail/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `keel_jail::MountError` (`Spawn`, `CommandFailed`, `NotMounted(PathBuf)`); `keel_jail::MountManager` trait (`mount_nullfs`, `unmount`, `is_mounted`); `keel_jail::CliMountManager`; `keel_jail::FakeMountManager`.

- [ ] **Step 1: Write the failing tests**

Create `keel-jail/src/mount_fake.rs`:

```rust
use crate::MountError;
use crate::MountManager;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeMountManager {
    mounted: Mutex<HashSet<PathBuf>>,
}

impl FakeMountManager {
    pub fn new() -> Self {
        Self::default()
    }
}

impl MountManager for FakeMountManager {
    fn mount_nullfs(&self, _source: &Path, target: &Path) -> Result<(), MountError> {
        self.mounted.lock().unwrap().insert(target.to_path_buf());
        Ok(())
    }

    fn unmount(&self, target: &Path) -> Result<(), MountError> {
        if self.mounted.lock().unwrap().remove(target) {
            Ok(())
        } else {
            Err(MountError::NotMounted(target.to_path_buf()))
        }
    }

    fn is_mounted(&self, target: &Path) -> Result<bool, MountError> {
        Ok(self.mounted.lock().unwrap().contains(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_mounted_is_false_until_mount_nullfs() {
        let mounts = FakeMountManager::new();
        let target = Path::new("/zroot/keel/jails/web-1/data");
        assert_eq!(mounts.is_mounted(target).unwrap(), false);
        mounts.mount_nullfs(Path::new("/zroot/keel/volumes/web-data"), target).unwrap();
        assert_eq!(mounts.is_mounted(target).unwrap(), true);
    }

    #[test]
    fn unmount_makes_is_mounted_false() {
        let mounts = FakeMountManager::new();
        let target = Path::new("/zroot/keel/jails/web-1/data");
        mounts.mount_nullfs(Path::new("/zroot/keel/volumes/web-data"), target).unwrap();
        mounts.unmount(target).unwrap();
        assert_eq!(mounts.is_mounted(target).unwrap(), false);
    }

    #[test]
    fn unmount_on_a_never_mounted_target_returns_not_mounted() {
        let mounts = FakeMountManager::new();
        let target = Path::new("/zroot/keel/jails/web-1/data");
        assert!(matches!(mounts.unmount(target), Err(MountError::NotMounted(_))));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-jail`
Expected: FAIL to compile — `MountManager`/`MountError` don't exist yet, `mod mount_fake` isn't declared.

- [ ] **Step 3: Implement `MountError` and the trait, then `CliMountManager`**

Create `keel-jail/src/mount_error.rs`:

```rust
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MountError {
    #[error("failed to spawn `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed with exit status {1}: {2}")]
    CommandFailed(String, std::process::ExitStatus, String),
    #[error("'{0}' is not currently mounted")]
    NotMounted(PathBuf),
}
```

Create `keel-jail/src/mount_cli.rs`:

```rust
use crate::MountError;
use crate::MountManager;
use std::path::Path;
use std::process::{Command, Output};

pub struct CliMountManager;

impl CliMountManager {
    pub fn new() -> Self {
        Self
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, MountError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| MountError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), MountError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(MountError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

impl Default for CliMountManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MountManager for CliMountManager {
    fn mount_nullfs(&self, source: &Path, target: &Path) -> Result<(), MountError> {
        Self::run_checked("mount", &["-t", "nullfs", &source.to_string_lossy(), &target.to_string_lossy()])
    }

    fn unmount(&self, target: &Path) -> Result<(), MountError> {
        let output = Self::run("umount", &[&target.to_string_lossy()])?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // FreeBSD's `umount` prints `umount: <path>: not currently mounted`
        // and exits non-zero for a target that isn't mounted — the same
        // "already in the desired state" tolerance `Reconciler::delete`
        // needs for volumes it never got as far as mounting.
        if stderr.contains("not currently mounted") {
            return Err(MountError::NotMounted(target.to_path_buf()));
        }
        Err(MountError::CommandFailed(
            format!("umount {}", target.display()),
            output.status,
            stderr.into_owned(),
        ))
    }

    fn is_mounted(&self, target: &Path) -> Result<bool, MountError> {
        let output = Self::run("mount", &["-p"])?;
        if !output.status.success() {
            return Err(MountError::CommandFailed(
                "mount -p".to_string(),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let target_str = target.to_string_lossy();
        // `mount -p`'s output is tab-separated: `device  mountpoint  fstype ...`.
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split('\t').nth(1) == Some(target_str.as_ref())))
    }
}
```

In `keel-jail/src/lib.rs`, add the new modules and re-exports:

```rust
pub mod error;
pub mod fake;
pub mod mount_cli;
pub mod mount_error;
pub mod mount_fake;
pub mod process;

pub use error::JailError;
pub use fake::FakeJailRuntime;
pub use mount_cli::CliMountManager;
pub use mount_error::MountError;
pub use mount_fake::FakeMountManager;
pub use process::ProcessJailRuntime;

use std::path::Path;

pub trait JailRuntime {
    // ... unchanged ...
}

pub trait MountManager {
    fn mount_nullfs(&self, source: &Path, target: &Path) -> Result<(), MountError>;
    fn unmount(&self, target: &Path) -> Result<(), MountError>;
    fn is_mounted(&self, target: &Path) -> Result<bool, MountError>;
}
```

(Only the `pub mod`/`pub use` lines and the new `pub trait MountManager` block are additions — `JailRuntime`'s body is untouched.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-jail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-jail/src/mount_error.rs keel-jail/src/mount_cli.rs keel-jail/src/mount_fake.rs keel-jail/src/lib.rs
git commit -m "Add MountManager trait plus CliMountManager/FakeMountManager"
```

---

### Task 4: `keel-agentd` — `record::volume_dataset_path`/`volume_mountpoint`

**Files:**
- Modify: `keel-agentd/src/record.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `record::volume_dataset_path(pool: &str, name: &str) -> String`; `record::volume_mountpoint(pool: &str, name: &str) -> PathBuf`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/record.rs`'s `#[cfg(test)] mod tests`, after `jail_rootfs_path_is_leading_slash_plus_dataset_path`:

```rust
    #[test]
    fn volume_dataset_path_uses_volumes_subdirectory() {
        assert_eq!(volume_dataset_path("zroot", "web-data"), "zroot/keel/volumes/web-data");
    }

    #[test]
    fn volume_mountpoint_is_leading_slash_plus_dataset_path() {
        assert_eq!(volume_mountpoint("zroot", "web-data"), PathBuf::from("/zroot/keel/volumes/web-data"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd record::tests`
Expected: FAIL to compile — `volume_dataset_path`/`volume_mountpoint` don't exist.

- [ ] **Step 3: Implement**

Add to `keel-agentd/src/record.rs`, after `jail_rootfs_path`:

```rust
pub fn volume_dataset_path(pool: &str, name: &str) -> String {
    format!("{pool}/keel/volumes/{name}")
}

pub fn volume_mountpoint(pool: &str, name: &str) -> PathBuf {
    PathBuf::from(format!("/{}", volume_dataset_path(pool, name)))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd record::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/record.rs
git commit -m "Add volume_dataset_path/volume_mountpoint record helpers"
```

---

### Task 5: `keel-agentd` — `Reconciler` gains `MountManager`, mounts/unmounts volumes in `provision`/`delete`

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: `keel_jail::MountManager` (Task 3), `keel_zfs::ZfsManager::create_volume` (Task 2), `record::volume_dataset_path`/`volume_mountpoint` (Task 4), `keel_spec::Spec.volumes` (Task 1).
- Produces: `Reconciler<J, Z, N, M: MountManager>::new(jails: J, zfs: Z, net: N, mounts: M, pool: String, state_dir: PathBuf)`; `ReconcileError::Io(#[from] std::io::Error)`; `Reconciler::get_volume(&self, name: &str) -> Result<(), ReconcileError>`; `Reconciler::delete_volume(&mut self, name: &str) -> Result<(), ReconcileError>`.

- [ ] **Step 1: Write the failing tests**

Update `keel-agentd/src/reconciler.rs`'s test module: `new_reconciler` gains a `FakeMountManager`, and `sample_spec` gets a sibling that carries a volume. Add near the top of `#[cfg(test)] mod tests`:

```rust
    use keel_jail::FakeMountManager;
```

Modify `new_reconciler`:

```rust
    fn new_reconciler(state_dir: PathBuf) -> Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager, FakeMountManager> {
        Reconciler::new(
            FakeJailRuntime::new(),
            FakeZfsManager::new(),
            FakeNetManager::new(),
            FakeMountManager::new(),
            "zroot".to_string(),
            state_dir,
        )
        .unwrap()
    }
```

Add after `sample_spec`:

```rust
    fn sample_spec_with_volume(name: &str, volume_name: &str, mount_path: &str, size: &str) -> JailSpec {
        let mut spec = sample_spec(name);
        spec.spec.volumes = vec![keel_spec::VolumeMount {
            name: volume_name.to_string(),
            mount_path: mount_path.to_string(),
            size: size.to_string(),
        }];
        spec
    }
```

Add new tests after `provision_fails_clearly_when_base_image_missing`:

```rust
    #[test]
    fn provision_creates_and_mounts_a_declared_volume() {
        let dir = test_state_dir("provision_creates_and_mounts_a_declared_volume");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();

        reconciler.provision("web-1", &record).unwrap();

        assert!(reconciler.zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap());
        let target = record::jail_rootfs_path("zroot", "web-1").join("data");
        assert!(reconciler.mounts.is_mounted(&target).unwrap());
    }

    #[test]
    fn reprovisioning_after_a_restart_does_not_recreate_or_remount_the_volume() {
        let dir = test_state_dir("reprovisioning_after_a_restart_does_not_recreate_or_remount_the_volume");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();
        reconciler.provision("web-1", &record).unwrap();

        // Simulate an agentd restart with the jail already provisioned:
        // re-running provision must be a no-op for the volume (still
        // exactly one dataset, still mounted, no error).
        reconciler.jails.destroy(&record::jail_name("web-1")).unwrap();
        reconciler.provision("web-1", &record).unwrap();

        assert!(reconciler.zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap());
        let target = record::jail_rootfs_path("zroot", "web-1").join("data");
        assert!(reconciler.mounts.is_mounted(&target).unwrap());
    }

    #[test]
    fn delete_unmounts_the_volume_but_leaves_its_dataset_present() {
        let dir = test_state_dir("delete_unmounts_the_volume_but_leaves_its_dataset_present");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();
        reconciler.provision("web-1", &record).unwrap();

        reconciler.delete("web-1").unwrap();

        assert!(reconciler.zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), "volume dataset must survive jail deletion");
        let target = record::jail_rootfs_path("zroot", "web-1").join("data");
        assert!(!reconciler.mounts.is_mounted(&target).unwrap());
    }

    #[test]
    fn reapplying_and_reprovisioning_the_same_name_finds_the_dataset_and_remounts_it() {
        let dir = test_state_dir("reapplying_and_reprovisioning_the_same_name_finds_the_dataset_and_remounts_it");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();
        reconciler.provision("web-1", &record).unwrap();
        reconciler.delete("web-1").unwrap();

        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();
        reconciler.provision("web-1", &record).unwrap();

        assert!(reconciler.zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap());
        let target = record::jail_rootfs_path("zroot", "web-1").join("data");
        assert!(reconciler.mounts.is_mounted(&target).unwrap());
    }

    #[test]
    fn get_volume_reports_existence() {
        let dir = test_state_dir("get_volume_reports_existence");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();
        reconciler.provision("web-1", &record).unwrap();

        assert!(reconciler.get_volume("web-data").is_ok());
        assert!(matches!(reconciler.get_volume("missing"), Err(ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(_)))));
    }

    #[test]
    fn delete_volume_destroys_the_dataset_once_the_jail_is_gone() {
        let dir = test_state_dir("delete_volume_destroys_the_dataset_once_the_jail_is_gone");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("web-1", "web-data", "/data", "1G")).unwrap();
        let record = reconciler.records["web-1"].clone();
        reconciler.provision("web-1", &record).unwrap();
        reconciler.delete("web-1").unwrap();

        reconciler.delete_volume("web-data").unwrap();

        assert!(matches!(reconciler.get_volume("web-data"), Err(ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(_)))));
    }

    #[test]
    fn delete_volume_on_a_never_created_name_is_not_found() {
        let dir = test_state_dir("delete_volume_on_a_never_created_name_is_not_found");
        let mut reconciler = new_reconciler(dir);
        assert!(matches!(reconciler.delete_volume("missing"), Err(ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(_)))));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd reconciler::tests`
Expected: FAIL to compile — `Reconciler::new` has the wrong arity, `get_volume`/`delete_volume` don't exist, `reconciler.mounts` isn't a field.

- [ ] **Step 3: Implement**

Modify the struct, imports, and constructor:

```rust
use keel_jail::{JailRuntime, MountManager};
```

```rust
pub struct Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager, M: MountManager> {
    jails: J,
    zfs: Z,
    net: N,
    mounts: M,
    pool: String,
    state_dir: PathBuf,
    records: HashMap<String, JailRecord>,
    backoff: HashMap<String, BackoffState>,
    next_epair_ordinal: u32,
}

impl<J: JailRuntime, Z: ZfsManager, N: NetManager, M: MountManager> Reconciler<J, Z, N, M> {
    pub fn new(jails: J, zfs: Z, net: N, mounts: M, pool: String, state_dir: PathBuf) -> Result<Self, ReconcileError> {
        let loaded = store::load_all(&state_dir)?;
        let next_epair_ordinal = loaded.iter().map(|r| r.epair_ordinal).max().map(|m| m + 1).unwrap_or(1);
        let records = loaded.into_iter().map(|r| (r.spec.metadata.name.clone(), r)).collect();
        Ok(Self {
            jails,
            zfs,
            net,
            mounts,
            pool,
            state_dir,
            records,
            backoff: HashMap::new(),
            next_epair_ordinal,
        })
    }
```

Add `Io` to `ReconcileError`:

```rust
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
```

Extend `provision` (after `configure_networking_and_limits`, before `start_command` — mounts must exist before the jail's command runs but the design spec places it right after clone/create; ordering vs. `configure_networking_and_limits` doesn't matter since neither touches the other, this plan places it directly after the rootfs/jail steps to match the design spec's own code block):

```rust
    fn provision(&mut self, name: &str, record: &JailRecord) -> Result<(), ReconcileError> {
        let jail_name = record::jail_name(name);
        let base_dataset = record::base_dataset_path(&self.pool, &record.spec.spec.image);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);
        let rootfs = record::jail_rootfs_path(&self.pool, name);

        if !self.zfs.dataset_exists(&base_dataset)? {
            return Err(ReconcileError::BaseImageNotFound(base_dataset));
        }
        self.zfs.clone_from_base(&base_dataset, &jail_dataset)?;
        self.jails.create(&jail_name, &rootfs, record.spec.spec.network.vnet)?;
        for volume in &record.spec.spec.volumes {
            let dataset = record::volume_dataset_path(&self.pool, &volume.name);
            let target = rootfs.join(volume.mount_path.trim_start_matches('/'));
            std::fs::create_dir_all(&target)?;
            if !self.zfs.dataset_exists(&dataset)? {
                self.zfs.create_volume(&dataset, &volume.size)?;
            }
            if !self.mounts.is_mounted(&target)? {
                self.mounts.mount_nullfs(&record::volume_mountpoint(&self.pool, &volume.name), &target)?;
            }
        }
        self.configure_networking_and_limits(name, record)?;
        self.jails.start_command(&jail_name, &record.spec.spec.command)?;
        Ok(())
    }
```

Extend `delete` (unmount every declared volume before destroying the rootfs dataset):

```rust
    pub fn delete(&mut self, name: &str) -> Result<(), ReconcileError> {
        let record = self.records.get(name).ok_or_else(|| ReconcileError::NotFound(name.to_string()))?.clone();
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);
        let rootfs = record::jail_rootfs_path(&self.pool, name);

        self.net.detach_jail(&epair_base)?;
        match self.jails.destroy(&jail_name) {
            Ok(()) | Err(keel_jail::JailError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        for volume in &record.spec.spec.volumes {
            let target = rootfs.join(volume.mount_path.trim_start_matches('/'));
            match self.mounts.unmount(&target) {
                Ok(()) | Err(keel_jail::MountError::NotMounted(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        match self.zfs.destroy_dataset(&jail_dataset) {
            Ok(()) | Err(keel_zfs::ZfsError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        match self.jails.remove_resource_limits(&jail_name) {
            Ok(()) | Err(keel_jail::JailError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }

        store::remove(&self.state_dir, name)?;
        self.records.remove(name);
        self.backoff.remove(name);
        Ok(())
    }
```

Add `ReconcileError::Jail(#[from] keel_jail::JailError)` already exists — `MountError` needs its own `#[from]` arm:

```rust
    #[error("mount error: {0}")]
    Mount(#[from] keel_jail::MountError),
```

Add `get_volume`/`delete_volume`, plus the `add_alias`/`remove_alias`-style passthroughs stay as-is:

```rust
    /// Never consults `self.records` — a volume can outlive every jail
    /// record that ever referenced it, which is this milestone's whole
    /// point. `Ok(())` means the dataset exists; the caller (HTTP layer)
    /// maps that to a 200 with a minimal body.
    pub fn get_volume(&self, name: &str) -> Result<(), ReconcileError> {
        let dataset = record::volume_dataset_path(&self.pool, name);
        if self.zfs.dataset_exists(&dataset)? {
            Ok(())
        } else {
            Err(ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(dataset)))
        }
    }

    pub fn delete_volume(&mut self, name: &str) -> Result<(), ReconcileError> {
        let dataset = record::volume_dataset_path(&self.pool, name);
        self.zfs.destroy_dataset(&dataset)?;
        Ok(())
    }
```

Update every other `Reconciler<...>`/`Reconciler::new` reference inside this file's own test module for the new arity (the `new_reconciler` helper from Step 1 already covers all of them, since every test calls through it).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd reconciler::tests`
Expected: PASS.

- [ ] **Step 5: Full-crate build check (expect failures elsewhere — fixed in Task 6)**

Run: `cargo build -p keel-agentd 2>&1 | head -60`
Expected: errors in `worker.rs`, `registration.rs`, `http.rs`, `main.rs`, `proxy.rs` — every other `Reconciler::new(...)` call site now has the wrong arity. This is expected; Task 6 fixes all of them. Do not commit yet.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/reconciler.rs
git commit -m "Reconciler gains a MountManager: mounts/unmounts declared volumes, get_volume/delete_volume"
```

(This commit intentionally leaves the workspace non-compiling outside `keel-agentd`'s `reconciler.rs` unit tests — Task 6 is the very next task and fixes every remaining call site. If your workflow requires every commit to build clean, squash Tasks 5 and 6 into one commit instead; this plan keeps them separate only because they're independently reviewable.)

---

### Task 6: `keel-agentd` — thread `MountManager` through `worker.rs` and every remaining `Reconciler::new` call site

**Files:**
- Modify: `keel-agentd/src/worker.rs`
- Modify: `keel-agentd/src/registration.rs`
- Modify: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/proxy.rs`
- Modify: `keel-agentd/src/main.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `Reconciler<J, Z, N, M>::new(.., mounts, ..)` (Task 5).
- Produces: `worker::spawn<J, Z, N, M>(reconciler: Reconciler<J, Z, N, M>)`; every construction site in this crate and in `keelctl/tests/cli.rs` passes a `keel_jail::FakeMountManager::new()` (tests) or `keel_jail::CliMountManager::new()` (`main.rs`) as the fourth `Reconciler::new` argument, immediately after the `NetManager` argument and before the `pool` string.

- [ ] **Step 1: `worker.rs` — generic bound, the two explicit `Reconciler::new` sites, `Command`**

Add the import and widen `spawn`'s bound:

```rust
use keel_jail::{JailRuntime, MountManager};
```

```rust
pub fn spawn<J, Z, N, M>(mut reconciler: Reconciler<J, Z, N, M>) -> (JoinHandle<()>, Sender<Command>)
where
    J: JailRuntime + Send + 'static,
    Z: ZfsManager + Send + 'static,
    N: NetManager + Send + 'static,
    M: MountManager + Send + 'static,
{
```

```rust
fn handle_command<J: JailRuntime, Z: ZfsManager, N: NetManager, M: MountManager>(
    reconciler: &mut Reconciler<J, Z, N, M>,
    command: Command,
) {
```

At `worker.rs:127` (`spawn_test_worker`'s own `Reconciler::new` call), add `FakeMountManager::new(),` between `FakeNetManager::new(),` and `test_state_dir(name),`, and add `use keel_jail::FakeMountManager;` to the test module's imports (alongside the existing `use keel_jail::FakeJailRuntime;`).

At `worker.rs:244` (`add_service_alias_command_round_trips`'s own `crate::Reconciler::new` call), same edit: insert `FakeMountManager::new(),` between `FakeNetManager::new(),` and `"zroot".to_string(),`.

Add the two new `Command` variants after `RemoveServiceAlias`:

```rust
    GetVolume(String, Sender<Result<(), ReconcileError>>),
    DeleteVolume(String, Sender<Result<(), ReconcileError>>),
```

Add the two new match arms in `handle_command`, after `RemoveServiceAlias`:

```rust
        Command::GetVolume(name, reply) => {
            let _ = reply.send(reconciler.get_volume(&name));
        }
        Command::DeleteVolume(name, reply) => {
            let _ = reply.send(reconciler.delete_volume(&name));
        }
```

Add tests after `add_service_alias_command_round_trips`:

```rust
    #[test]
    fn get_volume_and_delete_volume_commands_round_trip() {
        let commands = spawn_test_worker("get_volume_and_delete_volume_commands_round_trip");

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::GetVolume("web-data".to_string(), get_tx)).unwrap();
        assert!(matches!(get_rx.recv().unwrap(), Err(ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(_)))));

        let (del_tx, del_rx) = mpsc::channel();
        commands.send(Command::DeleteVolume("web-data".to_string(), del_tx)).unwrap();
        assert!(matches!(del_rx.recv().unwrap(), Err(ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(_)))));
    }
```

- [ ] **Step 2: `registration.rs` — 7 `Reconciler::new` call sites**

At each of lines 442, 475, 539, 610, 640, 717, 777, insert `keel_jail::FakeMountManager::new(),` immediately after the `keel_net::FakeNetManager::new(),` argument (each site already spells every argument fully-qualified — no new `use` needed).

- [ ] **Step 3: `http.rs` — 5 `Reconciler::new` call sites**

At each of lines 349, 371, 541, 738, 783, insert `keel_jail::FakeMountManager::new(),`/`FakeMountManager::new(),` (matching whichever qualification style that particular call site already uses for its other fakes) immediately after the `FakeNetManager::new()` argument.

- [ ] **Step 4: `proxy.rs` — 1 `Reconciler::new` call site and its type annotation**

Update `test_reconciler`'s return type and body:

```rust
    fn test_reconciler(name: &str) -> crate::Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager, keel_jail::FakeMountManager> {
        crate::Reconciler::new(
            FakeJailRuntime::new(),
            FakeZfsManager::new(),
            FakeNetManager::new(),
            keel_jail::FakeMountManager::new(),
            "zroot".to_string(),
            test_state_dir(name),
        )
        .unwrap()
    }
```

(Adjust to this file's actual existing argument list/formatting — read it first; only the new `keel_jail::FakeMountManager::new(),` argument and the type parameter are additions.)

- [ ] **Step 5: `main.rs` — the one real `Reconciler::new` call**

```rust
    let reconciler = Reconciler::new(
        ProcessJailRuntime::new(),
        CliZfsManager::new(),
        ProcessNetManager::new(),
        keel_jail::CliMountManager::new(),
        config.pool.clone(),
        config.state_dir.clone(),
    )
    .expect("failed to initialize reconciler from on-disk state");
```

- [ ] **Step 6: `keelctl/tests/cli.rs` — 2 `Reconciler::new` call sites**

At lines 21 and 71, insert `keel_jail::FakeMountManager::new(),` immediately after the `FakeNetManager::new(),` argument (this file's `dev-dependencies` already include `keel-jail`, so no `Cargo.toml` change is needed).

- [ ] **Step 7: Run the full workspace build and test suite**

Run: `cargo build --workspace 2>&1 | tail -60`
Expected: clean build.

Run: `cargo test --workspace 2>&1 | tail -100`
Expected: PASS across every crate.

- [ ] **Step 8: Commit**

```bash
git add keel-agentd/src/worker.rs keel-agentd/src/registration.rs keel-agentd/src/http.rs keel-agentd/src/proxy.rs keel-agentd/src/main.rs keelctl/tests/cli.rs
git commit -m "Thread MountManager through every Reconciler construction site, add GetVolume/DeleteVolume commands"
```

---

### Task 7: `keel-agentd` — `wire::VolumeStatus`, `GET`/`DELETE /volumes/<name>` HTTP routes

**Files:**
- Modify: `keel-agentd/src/wire.rs`
- Modify: `keel-agentd/src/http.rs`

**Interfaces:**
- Consumes: `Command::GetVolume`/`Command::DeleteVolume` (Task 6).
- Produces: `wire::VolumeStatus { name: String }`; `route()` dispatches `GET`/`DELETE /volumes/<name>`; `status_for_error` maps `ReconcileError::Zfs(ZfsError::NotFound(_))` to `404` and `ReconcileError::Zfs(ZfsError::Busy(_))` to `409`.

- [ ] **Step 1: Write the failing tests**

In `keel-agentd/src/wire.rs`'s `#[cfg(test)] mod tests`, add after `error_body_round_trips_through_yaml`:

```rust
    #[test]
    fn volume_status_round_trips_through_yaml() {
        let status = VolumeStatus { name: "web-data".to_string() };
        let yaml = serde_yaml::to_string(&status).unwrap();
        let parsed: VolumeStatus = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, status);
    }
```

In `keel-agentd/src/http.rs`'s `#[cfg(test)] mod tests`, add after `get_jails_lists_all_applied_jails`:

```rust
    fn sample_spec_yaml_with_volume(name: &str, volume_name: &str) -> String {
        format!(
            "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: {name}\nspec:\n  image: base/14.2-web\n  command: [\"/usr/local/bin/myapp\"]\n  network:\n    vnet: true\n    bridge: keel0\n    address: 10.0.0.5/24\n  resources:\n    cpu: \"2\"\n    memory: 512M\n  restartPolicy: Always\n  volumes:\n    - name: {volume_name}\n      mountPath: /data\n      size: 1G\n"
        )
    }

    #[test]
    fn get_volume_on_a_provisioned_volume_returns_200() {
        let socket_path = start_test_server("get_volume_on_a_provisioned_volume_returns_200");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml_with_volume("web-1", "web-data"));

        let (status, body) = send_request(&socket_path, "GET", "/volumes/web-data", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-data"));
    }

    #[test]
    fn get_volume_on_an_unknown_name_returns_404() {
        let socket_path = start_test_server("get_volume_on_an_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "GET", "/volumes/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn delete_volume_on_an_unknown_name_returns_404() {
        let socket_path = start_test_server("delete_volume_on_an_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "DELETE", "/volumes/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn delete_volume_survives_the_owning_jails_deletion_then_succeeds() {
        let socket_path = start_test_server("delete_volume_survives_the_owning_jails_deletion_then_succeeds");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml_with_volume("web-1", "web-data"));
        send_request(&socket_path, "DELETE", "/jails/web-1", "");

        let (status, _) = send_request(&socket_path, "GET", "/volumes/web-data", "");
        assert_eq!(status, 200, "the volume dataset must survive the jail's deletion");

        let (status, _) = send_request(&socket_path, "DELETE", "/volumes/web-data", "");
        assert_eq!(status, 200);

        let (status, _) = send_request(&socket_path, "GET", "/volumes/web-data", "");
        assert_eq!(status, 404, "the volume should be gone for good now");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd`
Expected: FAIL to compile — `VolumeStatus` doesn't exist, no route for `/volumes/<name>`.

- [ ] **Step 3: Implement**

In `keel-agentd/src/wire.rs`, add after `ErrorBody`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub name: String,
}
```

In `keel-agentd/src/http.rs`, add the two route arms to `route()`:

```rust
        ("GET", ["volumes", name]) => handle_get_volume(name, commands),
        ("DELETE", ["volumes", name]) => handle_delete_volume(name, commands),
```

Add the two handlers, near `handle_delete`:

```rust
fn handle_get_volume(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::GetVolume(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => yaml_response(200, &crate::wire::VolumeStatus { name: name.to_string() }),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}

fn handle_delete_volume(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::DeleteVolume(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}
```

Extend `status_for_error`:

```rust
fn status_for_error(error: &ReconcileError) -> u16 {
    match error {
        ReconcileError::InvalidSpec(keel_spec::SpecError::ImmutableField(_)) => 409,
        ReconcileError::InvalidSpec(_) => 400,
        ReconcileError::NotFound(_) => 404,
        ReconcileError::Zfs(keel_zfs::ZfsError::NotFound(_)) => 404,
        ReconcileError::Zfs(keel_zfs::ZfsError::Busy(_)) => 409,
        _ => 500,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/wire.rs keel-agentd/src/http.rs
git commit -m "Add GET/DELETE /volumes/<name>, mapping ZfsError::NotFound/Busy to 404/409"
```

---

### Task 8: `keel-controlplane` — forwarding routes for `/nodes/{id}/volumes/{name}`

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `handle_forward` (existing, unchanged).
- Produces: two new route arms forwarding `GET`/`DELETE /nodes/{id}/volumes/{name}` to `/volumes/{name}` on the target node.

- [ ] **Step 1: Write the failing test**

Add to `keel-controlplane/src/http.rs`'s `#[cfg(test)] mod tests`, near the existing `/nodes/{id}/jails/{name}` forwarding tests:

```rust
    #[test]
    fn get_node_volume_forwards_to_the_right_node() {
        let cp_addr = start_test_server();
        send_request(&cp_addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1:7621\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n");

        let (status, _) = send_request(&cp_addr, "GET", "/nodes/node-1/volumes/web-data", "");
        // No real node-1 is listening in this test, so the forward attempt
        // itself must succeed in *reaching the right code path* — a
        // connection failure surfaces as 500, which is still proof the
        // request was routed to node-1's address rather than 404'd by
        // route() for lack of a matching arm.
        assert_ne!(status, 404, "expected the route to exist and attempt a forward, not 404 for lack of a route");
    }

    #[test]
    fn delete_node_volume_on_an_unregistered_node_returns_404() {
        let cp_addr = start_test_server();
        let (status, _) = send_request(&cp_addr, "DELETE", "/nodes/missing/volumes/web-data", "");
        assert_eq!(status, 404);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane http::tests`
Expected: FAIL — no route for `/nodes/{id}/volumes/{name}` (falls through to the catch-all 404, so both assertions fail: the first because `status == 404` when it shouldn't, the second passing already but for the wrong reason until the route exists to test against).

- [ ] **Step 3: Implement**

Add to `route()`'s match, after the existing `("DELETE", ["nodes", id, "jails", name])` arm:

```rust
        ("GET", ["nodes", id, "volumes", name]) => {
            handle_forward(id, "GET", &format!("/volumes/{name}"), &[], commands, client_config)
        }
        ("DELETE", ["nodes", id, "volumes", name]) => {
            handle_forward(id, "DELETE", &format!("/volumes/{name}"), &[], commands, client_config)
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane http::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Forward GET/DELETE /nodes/{id}/volumes/{name} to the target node"
```

---

### Task 9: `keelctl` — `delete-volume <name>` verb

**Files:**
- Modify: `keelctl/src/main.rs`

**Interfaces:**
- Consumes: `dispatch`/`jails_path`/`success_body`/`Target` (existing, unchanged).
- Produces: `keelctl delete-volume <name> [--socket PATH|--control-plane-addr ADDR --node ID ...]`, sending `DELETE /volumes/<name>` (bare `--socket`) or `DELETE /nodes/{node}/volumes/<name>` (routed).

- [ ] **Step 1: Write the failing test**

Add to `keelctl/tests/cli.rs` (read the file first for its existing test server/dispatch helpers — it already has an end-to-end `apply`/`get`/`delete` test against a real spawned `keel-agentd` worker + Unix socket; follow that exact shape):

```rust
#[test]
fn delete_volume_against_a_bare_socket_hits_the_volumes_route() {
    // Uses this file's existing test-server helper (spawn a Reconciler +
    // worker::spawn + http::run over a Unix socket, exactly like the
    // existing apply/get/delete tests in this file already do).
    let socket = start_test_agentd("delete_volume_against_a_bare_socket_hits_the_volumes_route");

    let output = run_keelctl(&["--socket", socket.to_str().unwrap(), "delete-volume", "missing"]);
    assert!(!output.status.success(), "deleting a never-created volume should fail");
    assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
}
```

(Adapt `start_test_agentd`/`run_keelctl` to this file's actual existing helper names — read `keelctl/tests/cli.rs` in full before writing this step for real, since its exact test-server bootstrap shape wasn't re-derived here.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keelctl --test cli delete_volume`
Expected: FAIL — `delete-volume` is not a recognized subcommand (falls into the usage-error branch).

- [ ] **Step 3: Implement**

In `keelctl/src/main.rs`'s `main()`, add a branch to the `result` match:

```rust
    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "apply" => run_apply(&target, rest),
        Some((cmd, rest)) if cmd == "get" => run_get(&target, rest),
        Some((cmd, rest)) if cmd == "delete" => run_delete(&target, rest),
        Some((cmd, rest)) if cmd == "delete-volume" => run_delete_volume(&target, rest),
        _ => {
            eprintln!(
                "usage: keelctl <apply -f FILE|get [name]|delete NAME|delete-volume NAME> [--socket PATH|--control-plane-addr ADDR --node ID]"
            );
            return ExitCode::FAILURE;
        }
    };
```

Add the function after `run_delete`:

```rust
fn run_delete_volume(target: &Target, args: &[String]) -> Result<String, String> {
    let name = args.first().ok_or("delete-volume requires a volume name")?;
    let path = jails_path(target, &format!("/volumes/{name}"));
    success_body(dispatch(target, "DELETE", &path, "")).map(|_| String::new())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keelctl --test cli delete_volume`
Expected: PASS.

- [ ] **Step 5: Full workspace check**

Run: `cargo test --workspace`
Expected: PASS everywhere.

- [ ] **Step 6: Commit**

```bash
git add keelctl/src/main.rs keelctl/tests/cli.rs
git commit -m "Add keelctl delete-volume verb"
```

---

### Task 10: VM verification (manual, on the real FreeBSD fleet)

This milestone's single genuinely OS-level behavior (`zfs create -o quota=...`, `mount -t nullfs`, `umount`, and `destroy_dataset`'s busy-detection against a real still-mounted dataset) cannot be verified from this development environment — it needs the project's real FreeBSD VM fleet, per this project's standing "verify the one genuinely OS-level part for real" discipline (every prior milestone's README documents this same split between unit-tested-here and VM-verified-separately).

- [ ] **Step 1:** One-time per node: `zfs create zroot/keel/volumes` (the parent dataset `create_volume` deliberately never creates itself).
- [ ] **Step 2:** Apply a `kind: Jail` spec with one `spec.volumes` entry; confirm the jail starts and the mount point is writable.
- [ ] **Step 3:** Write a file into the mounted path; `keelctl delete` the jail; confirm `zfs list` still shows the volume dataset.
- [ ] **Step 4:** Re-apply the same jail; confirm the file written in Step 3 is still present at the mount point.
- [ ] **Step 5:** Confirm `keelctl delete-volume <name>` frees the dataset once the jail is gone, and fails cleanly (409) while the jail is still using it.
- [ ] **Step 6:** Confirm a plain `kind: Jail` with no `volumes` is entirely unaffected — no new mounts, no behavior change.
- [ ] **Step 7:** Update `README.md` with the VM verification result (pass/pending), matching every prior milestone's own README entry.

---

## Self-Review Notes

- Every Goals bullet in the design spec maps to a task above: `keel-spec` (Task 1), `keel-zfs` (Task 2), `keel-jail` (Task 3), `Reconciler::provision`/`delete` (Task 5), the `keel-agentd` HTTP+Command layer (Tasks 6-7), `keel-controlplane` (Task 8), `keelctl` (Task 9).
- Non-Goals are respected throughout: no changes to `ServiceSpecBody`/`JailTemplate`, no cross-node volume logic, no `kind: Volume` resource, no live-remount-on-reapply logic (only outright rejection via `ImmutableField`).
- Type/name consistency check: `VolumeMount` (Task 1) → `spec.spec.volumes` (Task 5's `provision`/`delete`) → `record::volume_dataset_path`/`volume_mountpoint` (Task 4) all agree on `name`/`mount_path`/`size` field names throughout.
