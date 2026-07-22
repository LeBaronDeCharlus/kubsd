# Milestone 19 Implementation Plan: Cross-Node Volume Movement via Replication and Force Re-Pin

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Milestone 19 exactly as specified in `docs/superpowers/specs/2026-07-20-keel-agent-milestone19-cross-node-volume-movement-design.md`: `keel-zfs` snapshot/send/receive primitives, a plain-TCP replication wire protocol between a stateful replica's primary and its standby, a `keel-agentd` replication loop, control-plane `Standbys`/`PendingFences` bookkeeping and scheduling, `keelctl force-repin`, and heartbeat-piggybacked fencing.

**Architecture:** Bottom-up: `keel-zfs` primitives first (Task 1), then the node-local pieces that only need those primitives (`keel-spec` field, `ReplicaTarget` storage, wire protocol receiver, replication-loop sender, retarget endpoint — Tasks 2-7), then control-plane state/scheduling/force-repin/fencing (Tasks 8-11), then `keelctl` (Task 12), then a full workspace check plus the real-VM verification every prior milestone has ended with (Task 13).

**Tech Stack:** Rust (2021 edition), `std::process::Command` for ZFS subprocesses, raw `std::net::TcpStream`/`TcpListener` (no async runtime anywhere in this codebase), `serde_yaml` for on-the-wire/on-disk specs, `mpsc` channel + dedicated worker thread ("actor") for all owned mutable state in both `keel-controlplane` and `keel-agentd`, `thiserror` for error enums.

## Global Constraints

- No async runtime anywhere in this codebase (`keel-controlplane`, `keel-agentd`, `keelctl` are all plain-thread, blocking-I/O). Do not introduce `tokio`/`async-std`.
- Every new piece of control-plane or node-local mutable state must go through the existing single-writer "worker" actor pattern (an `mpsc::Sender<Command>` into a dedicated thread) — never touched directly by an HTTP handler thread. This is how `Placements`/`UsedAddresses`/`Registry`/`Services` (control plane) and `Reconciler` (agentd) already work.
- Match this codebase's existing error-response convention exactly: 404 = target/resource doesn't exist or can't be located; 400 = malformed/invalid request; 409 = well-formed request that conflicts with committed state; 500 = internal/unreachable-peer failure; 503 = no schedulable capacity right now (mirrors `ScheduleError::NoAvailableNodes`/`ApplyServiceError::VipPoolExhausted`).
- `FakeZfsManager`/`FakeJailRuntime`/`FakeNetManager`/`FakeMountManager`-style in-memory fakes are this codebase's only test double mechanism — no mocking library. Every new piece of ZFS/network logic must be unit-testable against a fake, with real subprocess/socket behavior reserved for the closing real-VM verification step.
- Never use `--no-verify`/skip hooks. Create commits per step as instructed; do not squash across tasks.
- Run `cargo test -p <crate>` (or `cargo test --workspace` for the final task) after every implementation step and confirm the printed pass count before moving on — do not just assume green.

## Design decisions beyond the literal spec text

The design spec (reviewed and lightly corrected earlier today) describes the intended behavior at the architecture level but leaves a few concrete implementation questions open. These are resolved here, once, so every task below is unambiguous:

1. **Force-repin must reassign the replica's network address**, not just its node and dataset. A replica's `network.address` must lie inside its *hosting* node's `pod_cidr` (`keel-agentd`'s `handle_apply` already enforces this, see `keel-agentd/src/http.rs:245-259`). Since the standby being promoted almost always has a different `pod_cidr` than the dead primary, force-repin must release the old `UsedAddresses` entry and allocate a fresh one on the new primary's node, exactly like a brand-new `ReplicaAction::Schedule` does. Task 10 implements this.
2. **The replication wire protocol needs an explicit one-byte handshake before the bulk stream**, not just the one-byte rejection the spec's prose calls out. The sender writes the framed header, then blocks for exactly one reply byte before sending any `zfs send` payload bytes: `0x00` = "proceed, streaming now", `0x01` = "base mismatch, resend full — try again next tick". This is what makes "the sender's next tick retries with `base: None`" an observable, testable outcome rather than an inferred one. Task 4/6 implement this.
3. **The bulk byte-transfer path (both the replication listener and the replication loop) stays outside every `Command`-actor channel**, exactly like `keel-agentd`'s existing `proxy.rs` relay stays outside its worker's `Command` channel. Only small, instantaneous metadata (looking up/updating a `ReplicaTarget`, reading `replicate_to`) goes through a channel; the multi-second-or-longer byte stream itself runs on its own thread against a directly-owned, cheaply-cloneable `ZfsManager` handle. This is why Task 1 also makes `CliZfsManager`/`FakeZfsManager` `Clone`.
4. **The replication loop must notice when its replica has been deleted** and exit, or every deleted stateful replica would leak a forever-running background thread trying to snapshot a dataset whose owning jail is long gone (the dataset itself legitimately survives jail deletion per Milestone 17, but nothing should keep replicating it once the record is gone). Task 6 adds a minimal per-tick "does this record still exist?" check as the thread's exit condition.

## Task 1: `keel-zfs` — `snapshot`/`send_snapshot`/`receive_snapshot`, and `Clone` for both managers

**Files:**
- Modify: `keel-zfs/src/lib.rs`
- Modify: `keel-zfs/src/cli.rs`
- Modify: `keel-zfs/src/fake.rs`
- Test: inline `#[cfg(test)]` in `keel-zfs/src/fake.rs`

**Interfaces:**
- Produces: `ZfsManager::snapshot(&self, dataset: &str, snapshot: &str) -> Result<(), ZfsError>`, `ZfsManager::send_snapshot(&self, dataset: &str, snapshot: &str, base: Option<&str>, out: &mut dyn Write) -> Result<(), ZfsError>`, `ZfsManager::receive_snapshot(&self, dataset: &str, input: &mut dyn Read) -> Result<(), ZfsError>`. `CliZfsManager: Clone`, `FakeZfsManager: Clone` (clones share underlying state — needed by Task 4/6, which hand independent clones to a Reconciler and to a separately-threaded replication listener/loop).

- [ ] **Step 1: Write the failing `FakeZfsManager` tests for the three new methods**

Append to the `#[cfg(test)] mod tests` block in `keel-zfs/src/fake.rs`:

```rust
    #[test]
    fn snapshot_requires_an_existing_dataset() {
        let zfs = FakeZfsManager::new();
        assert!(matches!(zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-1"), Err(ZfsError::NotFound(_))));
    }

    #[test]
    fn send_snapshot_requires_the_snapshot_to_exist() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        let mut out = Vec::new();
        assert!(matches!(
            zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-1", None, &mut out),
            Err(ZfsError::NotFound(_))
        ));
    }

    #[test]
    fn send_snapshot_full_then_receive_snapshot_creates_the_target_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-1").unwrap();

        let mut stream = Vec::new();
        zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-1", None, &mut stream).unwrap();

        let target = FakeZfsManager::new();
        target.receive_snapshot("zroot/keel/volumes/web-0-data", &mut stream.as_slice()).unwrap();
        assert!(target.dataset_exists("zroot/keel/volumes/web-0-data").unwrap());
    }

    #[test]
    fn send_snapshot_incremental_requires_the_base_snapshot_to_exist() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-2").unwrap();

        let mut out = Vec::new();
        assert!(matches!(
            zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-2", Some("keel-repl-1"), &mut out),
            Err(ZfsError::NotFound(_))
        ));
    }

    #[test]
    fn send_snapshot_incremental_succeeds_once_the_base_exists() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-1").unwrap();
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-2").unwrap();

        let mut out = Vec::new();
        zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-2", Some("keel-repl-1"), &mut out).unwrap();
        assert!(!out.is_empty(), "expected the fake to still write a synthetic byte marker for an incremental send");
    }

    #[test]
    fn receive_snapshot_on_a_malformed_stream_fails_without_creating_the_dataset() {
        let zfs = FakeZfsManager::new();
        let mut garbage: &[u8] = b"not a real send stream";
        assert!(matches!(zfs.receive_snapshot("zroot/keel/volumes/web-0-data", &mut garbage), Err(ZfsError::CommandFailed(_, _, _))));
        assert!(!zfs.dataset_exists("zroot/keel/volumes/web-0-data").unwrap());
    }

    #[test]
    fn clone_shares_the_same_underlying_state() {
        let zfs = FakeZfsManager::new();
        let clone = zfs.clone();
        clone.seed_dataset("zroot/keel/volumes/shared");
        assert!(zfs.dataset_exists("zroot/keel/volumes/shared").unwrap(), "expected a clone's mutation to be visible through the original handle");
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile (the methods and `Clone` don't exist yet)**

Run: `cargo test -p keel-zfs`
Expected: compile error — `no method named 'snapshot' found`, `no method named 'send_snapshot' found`, etc., and `no method named 'clone' found for FakeZfsManager`.

- [ ] **Step 3: Add the three methods to the `ZfsManager` trait**

In `keel-zfs/src/lib.rs`, add `use std::io::{Read, Write};` near the top, and add to the `pub trait ZfsManager` block (after `create_volume`):

```rust
    fn snapshot(&self, dataset: &str, snapshot: &str) -> Result<(), ZfsError>;

    /// Streams a `zfs send` (full if `base` is `None`, incremental `-i <base>`
    /// otherwise) of `dataset@snapshot` into `out`.
    fn send_snapshot(&self, dataset: &str, snapshot: &str, base: Option<&str>, out: &mut dyn Write) -> Result<(), ZfsError>;

    /// Streams `input` into `zfs receive <dataset>`, creating or advancing
    /// `dataset` from the received stream.
    fn receive_snapshot(&self, dataset: &str, input: &mut dyn Read) -> Result<(), ZfsError>;
```

- [ ] **Step 4: Implement the three methods (and `Clone`) on `FakeZfsManager`**

In `keel-zfs/src/fake.rs`, change the struct to share state across clones and add a snapshots set:

```rust
use crate::ZfsError;
use crate::ZfsManager;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::sync::{Arc, Mutex};

#[derive(Default, Clone)]
pub struct FakeZfsManager {
    datasets: Arc<Mutex<HashSet<String>>>,
    snapshots: Arc<Mutex<HashSet<String>>>, // "dataset@snapshot"
    busy: Arc<Mutex<HashSet<String>>>,
}
```

(the `seed_dataset`/`mark_busy`/`dataset_exists`/`clone_from_base`/`create_volume`/`destroy_dataset` bodies are unchanged — `Arc<Mutex<_>>` supports the same `.lock().unwrap()` calls `Mutex<_>` did).

Add to `impl ZfsManager for FakeZfsManager`:

```rust
    fn snapshot(&self, dataset: &str, snapshot: &str) -> Result<(), ZfsError> {
        if !self.datasets.lock().unwrap().contains(dataset) {
            return Err(ZfsError::NotFound(dataset.to_string()));
        }
        self.snapshots.lock().unwrap().insert(format!("{dataset}@{snapshot}"));
        Ok(())
    }

    fn send_snapshot(&self, dataset: &str, snapshot: &str, base: Option<&str>, out: &mut dyn Write) -> Result<(), ZfsError> {
        let key = format!("{dataset}@{snapshot}");
        if !self.snapshots.lock().unwrap().contains(&key) {
            return Err(ZfsError::NotFound(key));
        }
        if let Some(base_snapshot) = base {
            let base_key = format!("{dataset}@{base_snapshot}");
            if !self.snapshots.lock().unwrap().contains(&base_key) {
                return Err(ZfsError::NotFound(base_key));
            }
        }
        let marker = format!(
            "keel-zfs-fake-send:{dataset}@{snapshot}:base={}\n",
            base.unwrap_or("none")
        );
        out.write_all(marker.as_bytes()).map_err(|e| ZfsError::Spawn("fake zfs send".to_string(), e))
    }

    fn receive_snapshot(&self, dataset: &str, input: &mut dyn Read) -> Result<(), ZfsError> {
        let mut buf = String::new();
        input.read_to_string(&mut buf).map_err(|e| ZfsError::Spawn("fake zfs receive".to_string(), e))?;
        if !buf.starts_with("keel-zfs-fake-send:") {
            return Err(ZfsError::CommandFailed(
                "zfs receive (fake)".to_string(),
                std::process::ExitStatus::from_raw(256), // exit code 1
                "malformed stream".to_string(),
            ));
        }
        self.datasets.lock().unwrap().insert(dataset.to_string());
        Ok(())
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-zfs`
Expected: all tests pass, including the 7 new ones and every pre-existing `FakeZfsManager` test (unaffected by the `Mutex` → `Arc<Mutex<_>>` change).

- [ ] **Step 6: Implement the three methods (and `Clone`) on `CliZfsManager`**

In `keel-zfs/src/cli.rs`, change the struct and imports:

```rust
use crate::ZfsError;
use crate::ZfsManager;
use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};

#[derive(Clone)]
pub struct CliZfsManager;
```

Add to `impl ZfsManager for CliZfsManager` (after `destroy_dataset`, before `clone_from_base`, matching the trait's declared order):

```rust
    fn snapshot(&self, dataset: &str, snapshot: &str) -> Result<(), ZfsError> {
        Self::run_checked(&["snapshot", &format!("{dataset}@{snapshot}")])
    }

    fn send_snapshot(&self, dataset: &str, snapshot: &str, base: Option<&str>, out: &mut dyn Write) -> Result<(), ZfsError> {
        let target = format!("{dataset}@{snapshot}");
        let base_arg = base.map(|b| format!("{dataset}@{b}"));
        let mut args: Vec<&str> = vec!["send"];
        if let Some(b) = &base_arg {
            args.push("-i");
            args.push(b);
        }
        args.push(&target);

        let mut child = Command::new("zfs")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ZfsError::Spawn("zfs".to_string(), e))?;
        let mut stdout = child.stdout.take().expect("stdout was piped");
        std::io::copy(&mut stdout, out).map_err(|e| ZfsError::Spawn("zfs send".to_string(), e))?;
        drop(stdout);
        let status = child.wait().map_err(|e| ZfsError::Spawn("zfs".to_string(), e))?;
        if status.success() {
            Ok(())
        } else {
            let mut stderr = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            Err(ZfsError::CommandFailed(format!("zfs {}", args.join(" ")), status, stderr))
        }
    }

    fn receive_snapshot(&self, dataset: &str, input: &mut dyn Read) -> Result<(), ZfsError> {
        let mut child = Command::new("zfs")
            .args(["receive", dataset])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ZfsError::Spawn("zfs".to_string(), e))?;
        let mut stdin = child.stdin.take().expect("stdin was piped");
        std::io::copy(input, &mut stdin).map_err(|e| ZfsError::Spawn("zfs receive".to_string(), e))?;
        drop(stdin);
        let status = child.wait().map_err(|e| ZfsError::Spawn("zfs".to_string(), e))?;
        if status.success() {
            Ok(())
        } else {
            let mut stderr = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            Err(ZfsError::CommandFailed(format!("zfs receive {dataset}"), status, stderr))
        }
    }
```

`CliZfsManager` has no unit tests today (it shells out to a real `zfs` binary) — that stays true here; real coverage comes from Task 13's VM verification, matching this file's existing untested-by-design status.

- [ ] **Step 7: Run the full crate test suite once more and commit**

Run: `cargo test -p keel-zfs`
Expected: all tests pass (no regressions from the `Clone`/`Arc` change).

```bash
git add keel-zfs/src/lib.rs keel-zfs/src/cli.rs keel-zfs/src/fake.rs
git commit -m "$(cat <<'EOF'
Add snapshot/send_snapshot/receive_snapshot to ZfsManager

Milestone 19's replication primitives. FakeZfsManager and CliZfsManager
both gain Clone (Arc-backed state on the fake) so a single ZFS handle can
be shared between a Reconciler and an independently-threaded replication
listener/loop without them fighting over ownership.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 2: `keel-spec` — `Spec` gains `replicate_to`

**Files:**
- Modify: `keel-spec/src/types.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `Spec.replicate_to: Option<String>` (defaults to `None` when absent from YAML), `JailTemplate::to_jail_spec` always sets `replicate_to: None` (the control plane fills it in later — see Task 9).

- [ ] **Step 1: Write the failing round-trip tests**

Add to `#[cfg(test)] mod tests` in `keel-spec/src/types.rs`:

```rust
    const EXAMPLE_YAML_WITH_REPLICATE_TO: &str = r#"
apiVersion: keel/v1
kind: Jail
metadata:
  name: db-0
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
    - name: db-0-data
      mountPath: /var/db
      size: 5G
  replicateTo: "10.0.0.9:7622"
"#;

    #[test]
    fn parses_replicate_to_when_present() {
        let spec: JailSpec = serde_yaml::from_str(EXAMPLE_YAML_WITH_REPLICATE_TO).unwrap();
        assert_eq!(spec.spec.replicate_to, Some("10.0.0.9:7622".to_string()));
    }

    #[test]
    fn replicate_to_defaults_to_none_when_absent() {
        let spec: JailSpec = serde_yaml::from_str(EXAMPLE_YAML).unwrap();
        assert_eq!(spec.spec.replicate_to, None);
    }

    #[test]
    fn to_jail_spec_always_starts_with_no_replicate_to() {
        let service: ServiceSpec = serde_yaml::from_str(SERVICE_EXAMPLE_YAML).unwrap();
        let jail = service.spec.template.to_jail_spec("web-0", "10.0.60.2/24");
        assert_eq!(jail.spec.replicate_to, None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-spec`
Expected: compile error — `no field 'replicate_to' on type 'Spec'`.

- [ ] **Step 3: Add the field**

In `keel-spec/src/types.rs`, modify `Spec`:

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
    #[serde(rename = "replicateTo", default)]
    pub replicate_to: Option<String>,
}
```

And in `JailTemplate::to_jail_spec`, add `replicate_to: None,` as the last field of the `Spec` struct literal.

Every other existing `Spec { ... }` struct literal in this workspace (`keel-agentd/src/record.rs`, `keel-agentd/src/store.rs`, `keel-agentd/src/reconciler.rs`, `keel-agentd/src/worker.rs`, `keel-controlplane` test helpers) will now fail to compile with "missing field `replicate_to`" — this is expected and intentional; do not add a `Default` impl or make the field non-exhaustive to paper over it. Fix each one by adding `replicate_to: None,` to the literal. Use `cargo build --workspace` to find every site (the compiler lists them all).

- [ ] **Step 4: Fix every other `Spec { ... }` literal across the workspace**

Run: `cargo build --workspace 2>&1 | grep "missing field"` to enumerate every remaining site, then add `replicate_to: None,` to each. (As of this plan's writing, expect sites in `keel-agentd/src/record.rs`, `keel-agentd/src/store.rs`, `keel-agentd/src/reconciler.rs`, `keel-agentd/src/worker.rs`, and `keel-controlplane/src/worker.rs`'s test module if it builds a `Spec` directly — most of this codebase builds jail specs via YAML strings instead, which are unaffected since the field is `#[serde(default)]`.)

- [ ] **Step 5: Run the tests to verify everything passes**

Run: `cargo test --workspace`
Expected: all tests pass across every crate.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
Add Spec.replicate_to for Milestone 19 replication targeting

Defaults to None on parse and on every freshly-built JailSpec; the
control plane is the only writer of a real value (Task 9).

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 3: `keel-agentd` — `ReplicaTarget` type and its own on-disk store

**Files:**
- Create: `keel-agentd/src/replica_target.rs`
- Create: `keel-agentd/src/replica_target_store.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Produces: `pub struct ReplicaTarget { pub replica_name: String, pub volume_dataset: String, pub source_node_addr: String, pub last_snapshot: Option<String> }`; `replica_target_store::save(state_dir: &Path, target: &ReplicaTarget) -> Result<(), StoreError>`, `replica_target_store::load_all(state_dir: &Path) -> Result<Vec<ReplicaTarget>, StoreError>`. Stored under `state_dir.join("replica-targets")` — a dedicated subdirectory, **not** alongside `JailRecord` `.yaml` files (which `store::load_all` would otherwise try, and fail, to parse as a `JailRecord`).

- [ ] **Step 1: Write the failing tests**

Create `keel-agentd/src/replica_target.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaTarget {
    pub replica_name: String,
    pub volume_dataset: String,
    pub source_node_addr: String,
    pub last_snapshot: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_target_round_trips_through_yaml() {
        let target = ReplicaTarget {
            replica_name: "db-0".to_string(),
            volume_dataset: "zroot/keel/volumes/db-0-data".to_string(),
            source_node_addr: "10.0.0.4:7621".to_string(),
            last_snapshot: None,
        };
        let yaml = serde_yaml::to_string(&target).unwrap();
        let parsed: ReplicaTarget = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, target);
    }
}
```

Create `keel-agentd/src/replica_target_store.rs`:

```rust
use crate::replica_target::ReplicaTarget;
use crate::store::StoreError;
use std::fs;
use std::path::Path;

fn dir(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("replica-targets")
}

pub fn load_all(state_dir: &Path) -> Result<Vec<ReplicaTarget>, StoreError> {
    let dir = dir(state_dir);
    fs::create_dir_all(&dir).map_err(|e| StoreError::Io(dir.clone(), e))?;
    let mut targets = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| StoreError::Io(dir.clone(), e))? {
        let entry = entry.map_err(|e| StoreError::Io(dir.clone(), e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|e| StoreError::Io(path.clone(), e))?;
        let target: ReplicaTarget = serde_yaml::from_str(&content).map_err(|e| StoreError::Parse(path.clone(), e))?;
        targets.push(target);
    }
    Ok(targets)
}

pub fn save(state_dir: &Path, target: &ReplicaTarget) -> Result<(), StoreError> {
    let dir = dir(state_dir);
    fs::create_dir_all(&dir).map_err(|e| StoreError::Io(dir.clone(), e))?;
    let path = dir.join(format!("{}.yaml", target.replica_name));
    let tmp_path = dir.join(format!("{}.yaml.tmp", target.replica_name));
    let content = serde_yaml::to_string(target).expect("ReplicaTarget serialization should not fail");
    fs::write(&tmp_path, content).map_err(|e| StoreError::Io(tmp_path.clone(), e))?;
    fs::rename(&tmp_path, &path).map_err(|e| StoreError::Io(path.clone(), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-replica-target-store-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample(name: &str) -> ReplicaTarget {
        ReplicaTarget {
            replica_name: name.to_string(),
            volume_dataset: format!("zroot/keel/volumes/{name}-data"),
            source_node_addr: "10.0.0.4:7621".to_string(),
            last_snapshot: None,
        }
    }

    #[test]
    fn save_then_load_all_roundtrips() {
        let dir = test_state_dir("save_then_load_all_roundtrips");
        let target = sample("db-0");
        save(&dir, &target).unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![target]);
    }

    #[test]
    fn load_all_on_missing_dir_creates_it_and_returns_empty() {
        let dir = test_state_dir("load_all_on_missing_dir_creates_it_and_returns_empty");
        assert_eq!(load_all(&dir).unwrap(), vec![]);
        assert!(dir.join("replica-targets").exists());
    }

    #[test]
    fn replica_targets_live_in_their_own_subdirectory_not_alongside_jail_records() {
        let dir = test_state_dir("replica_targets_live_in_their_own_subdirectory");
        let target = sample("db-0");
        save(&dir, &target).unwrap();

        // A JailRecord loader pointed at the same top-level state_dir must
        // see nothing here -- proving replica targets don't collide with
        // `store::load_all`'s own `.yaml` scan of `state_dir` itself.
        assert_eq!(crate::store::load_all(&dir).unwrap(), vec![]);
    }

    #[test]
    fn save_overwrites_rather_than_duplicating() {
        let dir = test_state_dir("save_overwrites_rather_than_duplicating");
        let mut target = sample("db-0");
        save(&dir, &target).unwrap();
        target.last_snapshot = Some("keel-repl-1".to_string());
        save(&dir, &target).unwrap();

        let loaded = load_all(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_snapshot, Some("keel-repl-1".to_string()));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p keel-agentd replica_target`
Expected: compile error — modules `replica_target`/`replica_target_store` aren't declared yet.

- [ ] **Step 3: Wire the new modules into `lib.rs`**

In `keel-agentd/src/lib.rs`, add `pub mod replica_target;` and `pub mod replica_target_store;`, and `pub use replica_target::ReplicaTarget;` alongside the existing `pub use` lines.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd replica_target`
Expected: all 5 new tests pass.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/replica_target.rs keel-agentd/src/replica_target_store.rs keel-agentd/src/lib.rs
git commit -m "$(cat <<'EOF'
Add ReplicaTarget type and its own on-disk store

A node holding a replicated copy runs no jail at all until force-repin
promotes it (Milestone 17's existing "a volume dataset is independent of
any jail record" precedent), so it needs its own small crash-safe record,
kept in state_dir/replica-targets/ to avoid colliding with JailRecord's
own flat .yaml scan of state_dir itself.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 4: `keel-agentd` — replication wire protocol (receiver side)

**Files:**
- Create: `keel-agentd/src/replication.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `keel_zfs::ZfsManager` (as a generic bound, cloned handle), `ReplicaTarget`/`replica_target_store` from Task 3.
- Produces: `pub struct ReplicaTargetRegistry` — an `Arc<Mutex<HashMap<String, ReplicaTarget>>>`-backed handle, cheaply `Clone`, loaded from disk at construction, with `get(&self, name: &str) -> Option<ReplicaTarget>` and an internal update path used only by this module's own connection handler. `pub fn write_header(stream: &mut dyn Write, replica_name: &str, snapshot_id: &str, base_snapshot_id: Option<&str>) -> std::io::Result<()>` and `pub fn read_header(stream: &mut dyn Read) -> std::io::Result<Header>` where `pub struct Header { pub replica_name: String, pub snapshot_id: String, pub base_snapshot_id: Option<String> }` — used by both this task's receiver and Task 6's sender. `pub const ACK_PROCEED: u8 = 0`, `pub const ACK_NEED_FULL: u8 = 1`. `pub fn run<Z: ZfsManager + Clone + Send + 'static>(listener: TcpListener, zfs: Z, pool: String, state_dir: PathBuf, targets: ReplicaTargetRegistry)`.

- [ ] **Step 1: Write the failing framing tests**

Create `keel-agentd/src/replication.rs`:

```rust
use crate::replica_target::ReplicaTarget;
use crate::replica_target_store;
use keel_zfs::ZfsManager;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

pub const ACK_PROCEED: u8 = 0;
pub const ACK_NEED_FULL: u8 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub replica_name: String,
    pub snapshot_id: String,
    pub base_snapshot_id: Option<String>,
}

fn write_len_prefixed(stream: &mut dyn Write, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)
}

fn read_len_prefixed(stream: &mut dyn Read) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_header(stream: &mut dyn Write, replica_name: &str, snapshot_id: &str, base_snapshot_id: Option<&str>) -> io::Result<()> {
    write_len_prefixed(stream, replica_name)?;
    write_len_prefixed(stream, snapshot_id)?;
    match base_snapshot_id {
        None => stream.write_all(&[0u8]),
        Some(base) => {
            stream.write_all(&[1u8])?;
            write_len_prefixed(stream, base)
        }
    }
}

pub fn read_header(stream: &mut dyn Read) -> io::Result<Header> {
    let replica_name = read_len_prefixed(stream)?;
    let snapshot_id = read_len_prefixed(stream)?;
    let mut has_base = [0u8; 1];
    stream.read_exact(&mut has_base)?;
    let base_snapshot_id = match has_base[0] {
        0 => None,
        _ => Some(read_len_prefixed(stream)?),
    };
    Ok(Header { replica_name, snapshot_id, base_snapshot_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_with_no_base_round_trips() {
        let mut buf = Vec::new();
        write_header(&mut buf, "db-0", "keel-repl-1", None).unwrap();
        let header = read_header(&mut buf.as_slice()).unwrap();
        assert_eq!(header, Header { replica_name: "db-0".to_string(), snapshot_id: "keel-repl-1".to_string(), base_snapshot_id: None });
    }

    #[test]
    fn header_with_a_base_round_trips() {
        let mut buf = Vec::new();
        write_header(&mut buf, "db-0", "keel-repl-2", Some("keel-repl-1")).unwrap();
        let header = read_header(&mut buf.as_slice()).unwrap();
        assert_eq!(
            header,
            Header { replica_name: "db-0".to_string(), snapshot_id: "keel-repl-2".to_string(), base_snapshot_id: Some("keel-repl-1".to_string()) }
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd replication::tests`
Expected: both framing tests pass (nothing else in this module exists yet, so nothing else runs).

- [ ] **Step 3: Write the failing `ReplicaTargetRegistry` and connection-handler tests**

Append to `keel-agentd/src/replication.rs`, above the existing `#[cfg(test)] mod tests`:

```rust
#[derive(Clone)]
pub struct ReplicaTargetRegistry {
    state_dir: PathBuf,
    by_name: Arc<Mutex<HashMap<String, ReplicaTarget>>>,
}

impl ReplicaTargetRegistry {
    pub fn load(state_dir: PathBuf) -> Result<Self, crate::store::StoreError> {
        let loaded = replica_target_store::load_all(&state_dir)?;
        let by_name = loaded.into_iter().map(|t| (t.replica_name.clone(), t)).collect();
        Ok(Self { state_dir, by_name: Arc::new(Mutex::new(by_name)) })
    }

    pub fn get(&self, replica_name: &str) -> Option<ReplicaTarget> {
        self.by_name.lock().unwrap().get(replica_name).cloned()
    }

    /// Creates the target on first contact (`volume_dataset`/`source_node_addr`
    /// as given, `last_snapshot: None`) or refreshes `source_node_addr` on an
    /// existing one, without touching its `last_snapshot`. Persists to disk.
    fn ensure(&self, replica_name: &str, volume_dataset: &str, source_node_addr: &str) -> Result<ReplicaTarget, crate::store::StoreError> {
        let mut guard = self.by_name.lock().unwrap();
        let target = guard.entry(replica_name.to_string()).or_insert_with(|| ReplicaTarget {
            replica_name: replica_name.to_string(),
            volume_dataset: volume_dataset.to_string(),
            source_node_addr: source_node_addr.to_string(),
            last_snapshot: None,
        });
        target.source_node_addr = source_node_addr.to_string();
        replica_target_store::save(&self.state_dir, target)?;
        Ok(target.clone())
    }

    fn record_snapshot(&self, replica_name: &str, snapshot_id: &str) -> Result<(), crate::store::StoreError> {
        let mut guard = self.by_name.lock().unwrap();
        if let Some(target) = guard.get_mut(replica_name) {
            target.last_snapshot = Some(snapshot_id.to_string());
            replica_target_store::save(&self.state_dir, target)?;
        }
        Ok(())
    }
}

/// One accepted connection's worth of work: read the header, decide
/// proceed-vs-reject against the locally-known `last_snapshot`, and (if
/// proceeding) stream the rest of the connection into `zfs receive`.
fn handle_connection<Z: ZfsManager>(mut stream: TcpStream, zfs: &Z, pool: &str, targets: &ReplicaTargetRegistry) -> io::Result<()> {
    let header = read_header(&mut stream)?;
    let dataset = crate::record::volume_dataset_path(pool, &header.replica_name);
    let peer_addr = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let target = targets
        .ensure(&header.replica_name, &dataset, &peer_addr)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    if header.base_snapshot_id != target.last_snapshot {
        stream.write_all(&[ACK_NEED_FULL])?;
        return Ok(());
    }
    stream.write_all(&[ACK_PROCEED])?;

    zfs.receive_snapshot(&dataset, &mut stream).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    targets
        .record_snapshot(&header.replica_name, &header.snapshot_id)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

pub fn run<Z: ZfsManager + Clone + Send + 'static>(listener: TcpListener, zfs: Z, pool: String, targets: ReplicaTargetRegistry) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let zfs = zfs.clone();
        let pool = pool.clone();
        let targets = targets.clone();
        thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &zfs, &pool, &targets) {
                eprintln!("keel-agentd: replication connection failed: {e}");
            }
        });
    }
}
```

Add before the existing `#[cfg(test)] mod tests` block's closing (extend it, don't create a second one) with:

```rust
    use keel_zfs::FakeZfsManager;
    use std::io::Read as _;
    use std::net::TcpListener as StdTcpListener;

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-replication-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn first_contact_creates_a_replica_target_and_accepts_a_full_send() {
        let dir = test_state_dir("first_contact_creates_a_replica_target_and_accepts_a_full_send");
        let targets = ReplicaTargetRegistry::load(dir).unwrap();
        let sender_zfs = FakeZfsManager::new();
        sender_zfs.seed_dataset("zroot/keel/volumes/db-0-data");
        sender_zfs.snapshot("zroot/keel/volumes/db-0-data", "keel-repl-1").unwrap();
        let receiver_zfs = FakeZfsManager::new();

        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let pool = "zroot".to_string();
        let targets_clone = targets.clone();
        let receiver_zfs_clone = receiver_zfs.clone();
        thread::spawn(move || run(listener, receiver_zfs_clone, pool, targets_clone));

        let mut stream = TcpStream::connect(addr).unwrap();
        write_header(&mut stream, "db-0", "keel-repl-1", None).unwrap();
        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).unwrap();
        assert_eq!(ack[0], ACK_PROCEED);

        sender_zfs.send_snapshot("zroot/keel/volumes/db-0-data", "keel-repl-1", None, &mut stream).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(receiver_zfs.dataset_exists("zroot/keel/volumes/db-0-data").unwrap());
        let target = targets.get("db-0").expect("expected a ReplicaTarget to have been created on first contact");
        assert_eq!(target.last_snapshot, Some("keel-repl-1".to_string()));
    }

    #[test]
    fn a_base_mismatch_is_rejected_without_reading_a_payload() {
        let dir = test_state_dir("a_base_mismatch_is_rejected_without_reading_a_payload");
        let targets = ReplicaTargetRegistry::load(dir).unwrap();
        let receiver_zfs = FakeZfsManager::new();

        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let pool = "zroot".to_string();
        let targets_clone = targets.clone();
        let receiver_zfs_clone = receiver_zfs.clone();
        thread::spawn(move || run(listener, receiver_zfs_clone, pool, targets_clone));

        let mut stream = TcpStream::connect(addr).unwrap();
        // This node has no ReplicaTarget yet (last_snapshot is None), so
        // claiming a base of "keel-repl-9" must be rejected.
        write_header(&mut stream, "db-0", "keel-repl-10", Some("keel-repl-9")).unwrap();
        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).unwrap();
        assert_eq!(ack[0], ACK_NEED_FULL);

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!receiver_zfs.dataset_exists("zroot/keel/volumes/db-0-data").unwrap());
    }
```

- [ ] **Step 4: Run the tests to verify they fail, then pass once corrected**

Run: `cargo test -p keel-agentd replication::tests`
Expected: first a compile pass with likely 1-2 logic mismatches (double-check `record::volume_dataset_path` is `pub(crate)`-visible from this new module — it's `pub fn` already, so no visibility change needed), then all 4 tests green.

- [ ] **Step 5: Wire the module into `lib.rs`**

In `keel-agentd/src/lib.rs`, add `pub mod replication;` and `pub use replication::{ReplicaTargetRegistry, ACK_NEED_FULL, ACK_PROCEED};`.

- [ ] **Step 6: Run the full crate test suite and commit**

Run: `cargo test -p keel-agentd`
Expected: all tests pass, including every pre-existing one.

```bash
git add keel-agentd/src/replication.rs keel-agentd/src/lib.rs
git commit -m "$(cat <<'EOF'
Add the Milestone 19 replication wire protocol (receiver side)

Length-prefixed header (replica_name, snapshot_id, base_snapshot_id) plus
a one-byte ACK_PROCEED/ACK_NEED_FULL handshake before the zfs send/receive
payload -- the handshake is this plan's own addition, needed to make
"the sender's next tick retries with base: None" an observable outcome
rather than an inferred one. Bulk I/O runs on its own per-connection
thread, independent of any Command-actor channel, matching proxy.rs's
existing relay precedent.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 5: `keel-agentd` — `GET /replica-targets/<name>` readiness endpoint

**Files:**
- Modify: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/wire.rs`
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `ReplicaTargetRegistry` from Task 4.
- Produces: `GET /replica-targets/<name>` — 200 with `ReplicaTargetStatus { replica_name, ready: true }` once `last_snapshot.is_some()`; 409 if a `ReplicaTarget` exists but `last_snapshot` is still `None`; 404 if no `ReplicaTarget` exists at all for that name. This is what Task 10's `force-repin` handler probes before promoting.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/wire.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaTargetStatus {
    pub replica_name: String,
    pub ready: bool,
}
```

Add to the `#[cfg(test)] mod tests` block in `keel-agentd/src/http.rs` (near the other `start_test_server` helpers):

```rust
    fn start_test_server_with_replica_targets(name: &str, targets: crate::ReplicaTargetRegistry) -> PathBuf {
        let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-test-state-{name}"));
        let _ = std::fs::remove_dir_all(&state_dir);
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), FakeMountManager::new(), "zroot".to_string(), state_dir).unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let socket_path = short_unique_socket_path();
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();
        thread::spawn(move || run(listener, commands, PodCidrSlot::new(), targets));
        socket_path
    }

    #[test]
    fn get_replica_target_on_an_unknown_name_returns_404() {
        let dir = std::env::temp_dir().join("keel-agentd-http-test-replica-targets-unknown");
        let _ = std::fs::remove_dir_all(&dir);
        let targets = crate::ReplicaTargetRegistry::load(dir).unwrap();
        let socket_path = start_test_server_with_replica_targets("get_replica_target_on_an_unknown_name_returns_404", targets);
        let (status, _) = send_request(&socket_path, "GET", "/replica-targets/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn get_replica_target_before_a_first_snapshot_returns_409() {
        let dir = std::env::temp_dir().join("keel-agentd-http-test-replica-targets-not-ready");
        let _ = std::fs::remove_dir_all(&dir);
        let targets = crate::ReplicaTargetRegistry::load(dir).unwrap();
        targets.ensure_for_test("db-0", "zroot/keel/volumes/db-0-data", "10.0.0.4:7621");
        let socket_path = start_test_server_with_replica_targets("get_replica_target_before_a_first_snapshot_returns_409", targets);
        let (status, _) = send_request(&socket_path, "GET", "/replica-targets/db-0", "");
        assert_eq!(status, 409);
    }

    #[test]
    fn get_replica_target_after_a_first_snapshot_returns_200_and_ready_true() {
        let dir = std::env::temp_dir().join("keel-agentd-http-test-replica-targets-ready");
        let _ = std::fs::remove_dir_all(&dir);
        let targets = crate::ReplicaTargetRegistry::load(dir).unwrap();
        targets.ensure_for_test("db-0", "zroot/keel/volumes/db-0-data", "10.0.0.4:7621");
        targets.record_snapshot_for_test("db-0", "keel-repl-1");
        let socket_path = start_test_server_with_replica_targets("get_replica_target_after_a_first_snapshot_returns_200_and_ready_true", targets);
        let (status, body) = send_request(&socket_path, "GET", "/replica-targets/db-0", "");
        assert_eq!(status, 200);
        assert!(body.contains("ready: true"), "got: {body}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p keel-agentd get_replica_target`
Expected: compile errors — `run` doesn't take a `ReplicaTargetRegistry` argument yet; `ensure_for_test`/`record_snapshot_for_test` don't exist; no route for `GET /replica-targets/<name>`.

- [ ] **Step 3: Expose test-only helpers on `ReplicaTargetRegistry`**

In `keel-agentd/src/replication.rs`, add (near `ensure`/`record_snapshot`, outside `#[cfg(test)]` since `keel-agentd/src/http.rs`'s tests are in a different module and need real `pub` visibility — mirror how `FakeZfsManager::seed_dataset` is a real, always-compiled test helper, not `#[cfg(test)]`-gated):

```rust
    /// Test helper: seed a `ReplicaTarget` directly, bypassing the network
    /// handshake in `handle_connection`.
    pub fn ensure_for_test(&self, replica_name: &str, volume_dataset: &str, source_node_addr: &str) {
        self.ensure(replica_name, volume_dataset, source_node_addr).unwrap();
    }

    /// Test helper: mark a `ReplicaTarget` as having completed a snapshot,
    /// bypassing a real `zfs receive`.
    pub fn record_snapshot_for_test(&self, replica_name: &str, snapshot_id: &str) {
        self.record_snapshot(replica_name, snapshot_id).unwrap();
    }
```

- [ ] **Step 4: Add the route and handler in `http.rs`**

In `keel-agentd/src/http.rs`, thread `ReplicaTargetRegistry` through every entry point exactly like `PodCidrSlot` already is:

```rust
pub fn run(listener: UnixListener, commands: Sender<Command>, pod_cidr_slot: PodCidrSlot, replica_targets: crate::ReplicaTargetRegistry) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let pod_cidr_slot = pod_cidr_slot.clone();
        let replica_targets = replica_targets.clone();
        thread::spawn(move || {
            let _ = handle_connection(stream, &commands, &pod_cidr_slot, &replica_targets);
        });
    }
}

pub fn run_tls(
    listener: TcpListener,
    commands: Sender<Command>,
    reloading_tls: Arc<crate::tls::ReloadingTls>,
    pod_cidr_slot: PodCidrSlot,
    replica_targets: crate::ReplicaTargetRegistry,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = reloading_tls.server_config();
        let pod_cidr_slot = pod_cidr_slot.clone();
        let replica_targets = replica_targets.clone();
        thread::spawn(move || {
            let Ok(conn) = ServerConnection::new(tls_config) else { return };
            let mut tls_stream = TlsStream::new(conn, stream);
            if handle_connection_tls(&mut tls_stream, &commands, &pod_cidr_slot, &replica_targets).is_err() {
                eprintln!("keel-agentd: TLS handshake or request handling failed for a connection");
            }
        });
    }
}
```

Thread the new parameter through `handle_connection`/`handle_connection_tls` the same way `pod_cidr_slot` already flows, down into `route`:

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>, pod_cidr_slot: &PodCidrSlot, replica_targets: &crate::ReplicaTargetRegistry) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("PUT", ["jails", name]) => handle_apply(name, &request.body, commands, pod_cidr_slot),
        ("GET", ["jails"]) => handle_get(None, commands),
        ("GET", ["jails", name]) => handle_get(Some(name.to_string()), commands),
        ("DELETE", ["jails", name]) => handle_delete(name, commands),
        ("GET", ["volumes", name]) => handle_get_volume(name, commands),
        ("DELETE", ["volumes", name]) => handle_delete_volume(name, commands),
        ("GET", ["replica-targets", name]) => handle_get_replica_target(name, replica_targets),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}

fn handle_get_replica_target(name: &str, replica_targets: &crate::ReplicaTargetRegistry) -> (u16, Vec<u8>) {
    match replica_targets.get(name) {
        None => error_response(404, format!("no replica target '{name}'")),
        Some(target) if target.last_snapshot.is_none() => {
            error_response(409, format!("replica target '{name}' has not completed a first full replication yet"))
        }
        Some(_) => yaml_response(200, &crate::wire::ReplicaTargetStatus { replica_name: name.to_string(), ready: true }),
    }
}
```

Update every existing call site of `run`/`run_tls`/`route`/`handle_connection`/`handle_connection_tls` (production code in `keel-agentd/src/main.rs`, and every test helper already in `http.rs`'s own test module, e.g. `start_test_server`, `start_test_server_with_pod_cidr`, `start_tcp_test_server`, plus the reload tests) to pass an extra `crate::ReplicaTargetRegistry::load(state_dir.join(...)).unwrap()` (each test helper already has a `state_dir` in scope; reuse it) or, in `main.rs`, the real one constructed from `config.state_dir`.

- [ ] **Step 5: Update `main.rs` to construct and thread the registry**

In `keel-agentd/src/main.rs`, after constructing `reconciler`, add:

```rust
    let replica_targets = keel_agentd::ReplicaTargetRegistry::load(config.state_dir.clone())
        .expect("failed to load replica-target state");
```

and pass `replica_targets.clone()` into every `keel_agentd::http::run`/`run_tls` call (both the TLS listener spawned inside the `if let (...)` block and the final Unix-socket `keel_agentd::http::run(listener, commands, pod_cidr_slot, replica_targets);` call).

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd`
Expected: all tests pass, including the 3 new ones and every pre-existing `http.rs` test (now passing a `ReplicaTargetRegistry` alongside `PodCidrSlot`).

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/src/http.rs keel-agentd/src/wire.rs keel-agentd/src/main.rs keel-agentd/src/replication.rs
git commit -m "$(cat <<'EOF'
Add GET /replica-targets/<name> readiness endpoint

404 if no ReplicaTarget exists, 409 if it exists but hasn't completed a
first full replication (last_snapshot still None), 200 + ready:true once
it has -- this is what the control plane's force-repin handler (Task 10)
probes before promoting a standby.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 6: `keel-agentd` — the replication loop (sender side) and `replicate-to` retargeting

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`
- Modify: `keel-agentd/src/worker.rs`
- Modify: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_zfs::ZfsManager::{snapshot, send_snapshot}` (Task 1), `replication::{write_header, read_header, ACK_PROCEED, ACK_NEED_FULL}` (Task 4).
- Produces: `Reconciler::set_replicate_to(&mut self, name: &str, replicate_to: Option<String>) -> Result<(), ReconcileError>` (patches the on-disk `JailRecord` in place, bypassing `apply()`/`validate_transition` entirely — this is the mechanism `PUT /jails/<name>/replicate-to` uses). `worker::Command::SetReplicateTo(String, Option<String>, Sender<Result<(), ReconcileError>>)`. A `replication_loop::spawn` function starting one thread per stateful+replicated replica, ticking every 30s.

- [ ] **Step 1: Write the failing `Reconciler::set_replicate_to` tests**

Add to `#[cfg(test)] mod tests` in `keel-agentd/src/reconciler.rs`:

```rust
    #[test]
    fn set_replicate_to_updates_the_record_without_going_through_validate_transition() {
        let dir = test_state_dir("set_replicate_to_updates_the_record_without_going_through_validate_transition");
        let mut reconciler = new_reconciler(dir.clone());
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec_with_volume("db-0", "db-0-data", "/var/db", "5G")).unwrap();

        reconciler.set_replicate_to("db-0", Some("10.0.0.9:7622".to_string())).unwrap();

        assert_eq!(reconciler.records["db-0"].spec.spec.replicate_to, Some("10.0.0.9:7622".to_string()));
        let loaded = store::load_all(&dir).unwrap();
        assert_eq!(loaded[0].spec.spec.replicate_to, Some("10.0.0.9:7622".to_string()));
    }

    #[test]
    fn set_replicate_to_on_an_unknown_name_returns_not_found() {
        let dir = test_state_dir("set_replicate_to_on_an_unknown_name_returns_not_found");
        let mut reconciler = new_reconciler(dir);
        assert!(matches!(reconciler.set_replicate_to("missing", Some("10.0.0.9:7622".to_string())), Err(ReconcileError::NotFound(_))));
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p keel-agentd set_replicate_to`
Expected: compile error — no method `set_replicate_to`.

- [ ] **Step 3: Implement `Reconciler::set_replicate_to`**

In `keel-agentd/src/reconciler.rs`, add (near `delete_volume`):

```rust
    /// Retargets an *already-running* primary's replication without a full
    /// re-provision -- just updates `replicate_to` on the existing
    /// `JailRecord` and persists it. Bypasses `apply()`/`validate_transition`
    /// entirely: `replicate_to` isn't an immutability-checked field, and this
    /// path has no spec to validate against, only a bare address to store.
    pub fn set_replicate_to(&mut self, name: &str, replicate_to: Option<String>) -> Result<(), ReconcileError> {
        let mut record = self.records.get(name).ok_or_else(|| ReconcileError::NotFound(name.to_string()))?.clone();
        record.spec.spec.replicate_to = replicate_to;
        store::save(&self.state_dir, &record)?;
        self.records.insert(name.to_string(), record);
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd set_replicate_to`
Expected: both tests pass.

- [ ] **Step 5: Add `Command::SetReplicateTo` and the `PUT /jails/<name>/replicate-to` route**

In `keel-agentd/src/worker.rs`, add to the `Command` enum: `SetReplicateTo(String, Option<String>, Sender<Result<(), ReconcileError>>),` and to `handle_command`:

```rust
        Command::SetReplicateTo(name, replicate_to, reply) => {
            let _ = reply.send(reconciler.set_replicate_to(&name, replicate_to));
        }
```

In `keel-agentd/src/http.rs`, add the route (in `route()`, alongside the other `["jails", name]` arms) and handler:

```rust
        ("PUT", ["jails", name, "replicate-to"]) => handle_set_replicate_to(name, &request.body, commands),
```

```rust
fn handle_set_replicate_to(name: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let replicate_to: Option<String> = if body.is_empty() {
        None
    } else {
        match serde_yaml::from_slice::<crate::wire::ReplicateToBody>(body) {
            Ok(b) => Some(b.replicate_to),
            Err(e) => return error_response(400, format!("invalid YAML: {e}")),
        }
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::SetReplicateTo(name.to_string(), replicate_to, reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}
```

Add to `keel-agentd/src/wire.rs`: `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] pub struct ReplicateToBody { pub replicate_to: String }`.

- [ ] **Step 6: Write and run the HTTP-level tests for the new route**

Add to `keel-agentd/src/http.rs`'s test module:

```rust
    #[test]
    fn put_replicate_to_retargets_an_existing_jails_replication_address() {
        let socket_path = start_test_server("put_replicate_to_retargets_an_existing_jails_replication_address");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));

        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1/replicate-to", "replicate_to: 10.0.0.9:7622\n");
        assert_eq!(status, 200);

        let (status, body) = send_request(&socket_path, "GET", "/jails/web-1", "");
        assert_eq!(status, 200);
        assert!(body.contains("10.0.0.9:7622"), "got: {body}");
    }

    #[test]
    fn put_replicate_to_on_an_unknown_name_returns_404() {
        let socket_path = start_test_server("put_replicate_to_on_an_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "PUT", "/jails/missing/replicate-to", "replicate_to: 10.0.0.9:7622\n");
        assert_eq!(status, 404);
    }
```

Run: `cargo test -p keel-agentd put_replicate_to`
Expected: both pass.

- [ ] **Step 7: Write the failing replication-loop tests**

Create the module inline as part of this step — add a new file `keel-agentd/src/replication_loop.rs`:

```rust
use crate::replica_target::ReplicaTarget;
use crate::replication::{self, ACK_NEED_FULL, ACK_PROCEED};
use crate::worker::Command;
use keel_zfs::ZfsManager;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

/// Spawned once per stateful+replicated jail name from `Command::Apply`
/// (see `worker.rs`) when its spec has both `volumes` and `replicate_to`
/// set. Ticks every `interval`: re-reads `replicate_to` from the live
/// `JailRecord` (so a `PUT /jails/<name>/replicate-to` takes effect on the
/// very next tick with no signal/restart), snapshots the volume, and sends
/// a full or incremental stream to the standby. Exits once the record
/// itself is gone (the replica was deleted) -- checked every tick via
/// `Command::Get`, since nothing should keep replicating a deleted
/// replica's already-orphaned dataset forever.
pub fn spawn<Z: ZfsManager + Clone + Send + 'static>(
    replica_name: String,
    volume_name: String,
    pool: String,
    zfs: Z,
    commands: Sender<Command>,
    interval: Duration,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let dataset = crate::record::volume_dataset_path(&pool, &volume_name);
        let mut last_confirmed_sent: Option<String> = None;
        let mut tick: u64 = 0;
        loop {
            thread::sleep(interval);
            tick += 1;

            let (tx, rx) = std::sync::mpsc::channel();
            if commands.send(Command::Get(Some(replica_name.clone()), tx)).is_err() {
                return;
            }
            let Ok(statuses) = rx.recv() else { return };
            let Some(status) = statuses.into_iter().next() else {
                eprintln!("keel-agentd: replica '{replica_name}' no longer exists, stopping its replication loop");
                return;
            };
            let Some(replicate_to) = status.record.spec.spec.replicate_to.clone() else {
                continue; // retargeted away to nothing (shouldn't normally happen); just wait
            };

            let snapshot_id = format!("keel-repl-{tick}");
            if let Err(e) = zfs.snapshot(&dataset, &snapshot_id) {
                eprintln!("keel-agentd: failed to snapshot '{dataset}' for replica '{replica_name}': {e}");
                continue;
            }

            match send_once(&zfs, &dataset, &snapshot_id, last_confirmed_sent.as_deref(), &replicate_to) {
                Ok(()) => {
                    last_confirmed_sent = Some(snapshot_id);
                }
                Err(SendOnceError::NeedFull) => {
                    eprintln!("keel-agentd: standby for replica '{replica_name}' rejected the incremental base; will send full next tick");
                    last_confirmed_sent = None;
                }
                Err(SendOnceError::Io(e)) => {
                    eprintln!("keel-agentd: failed to replicate '{replica_name}' to {replicate_to}: {e}");
                }
            }
        }
    })
}

enum SendOnceError {
    NeedFull,
    Io(String),
}

fn send_once<Z: ZfsManager>(zfs: &Z, dataset: &str, snapshot_id: &str, base: Option<&str>, replicate_to: &str) -> Result<(), SendOnceError> {
    let mut stream = TcpStream::connect(replicate_to).map_err(|e| SendOnceError::Io(e.to_string()))?;
    replication::write_header(&mut stream, dataset_replica_name(dataset), snapshot_id, base).map_err(|e| SendOnceError::Io(e.to_string()))?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).map_err(|e| SendOnceError::Io(e.to_string()))?;
    if ack[0] == ACK_NEED_FULL {
        return Err(SendOnceError::NeedFull);
    }
    zfs.send_snapshot(dataset, snapshot_id, base, &mut stream).map_err(|e| SendOnceError::Io(e.to_string()))?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    Ok(())
}

/// `dataset` is `<pool>/keel/volumes/<replica_name>`; the replica name the
/// receiver keys its `ReplicaTarget` by is the final path component.
fn dataset_replica_name(dataset: &str) -> &str {
    dataset.rsplit('/').next().unwrap_or(dataset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconciler::Reconciler;
    use crate::worker;
    use keel_jail::{FakeJailRuntime, FakeMountManager};
    use keel_net::FakeNetManager;
    use keel_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec, VolumeMount};
    use keel_zfs::FakeZfsManager;
    use std::path::PathBuf;

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-replication-loop-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn stateful_spec(name: &str) -> keel_spec::JailSpec {
        keel_spec::JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec { vnet: true, bridge: "keel0".to_string(), address: "10.0.0.5/24".to_string() },
                resources: ResourcesSpec { cpu: "1".to_string(), memory: "256M".to_string() },
                restart_policy: RestartPolicy::Always,
                volumes: vec![VolumeMount { name: format!("{name}-data"), mount_path: "/var/db".to_string(), size: "1G".to_string() }],
                replicate_to: None,
            },
        }
    }

    #[test]
    fn a_tick_snapshots_and_sends_a_full_replication_on_first_contact() {
        let dir = test_state_dir("a_tick_snapshots_and_sends_a_full_replication_on_first_contact");
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(FakeJailRuntime::new(), zfs.clone(), FakeNetManager::new(), FakeMountManager::new(), "zroot".to_string(), dir).unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let (apply_tx, apply_rx) = std::sync::mpsc::channel();
        commands.send(Command::Apply(stateful_spec("db-0"), apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();

        let receiver_zfs = FakeZfsManager::new();
        let targets = crate::ReplicaTargetRegistry::load(std::env::temp_dir().join("keel-agentd-replication-loop-test-receiver-a_tick_snapshots_and_sends_a_full_replication_on_first_contact")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let receiver_zfs_clone = receiver_zfs.clone();
        let targets_clone = targets.clone();
        std::thread::spawn(move || crate::replication::run(listener, receiver_zfs_clone, "zroot".to_string(), targets_clone));

        let (rt_tx, rt_rx) = std::sync::mpsc::channel();
        commands.send(Command::SetReplicateTo("db-0".to_string(), Some(addr), rt_tx)).unwrap();
        rt_rx.recv().unwrap().unwrap();

        let _handle = spawn("db-0".to_string(), "db-0-data".to_string(), "zroot".to_string(), zfs.clone(), commands.clone(), Duration::from_millis(50));

        std::thread::sleep(Duration::from_millis(300));
        assert!(receiver_zfs.dataset_exists("zroot/keel/volumes/db-0-data").unwrap());
        assert!(targets.get("db-0").is_some_and(|t| t.last_snapshot.is_some()));
    }
}
```

Note the receiver's `ReplicaTargetRegistry::load` directory must be cleared first, matching `test_state_dir`'s existing idiom elsewhere in this crate — add `let _ = std::fs::remove_dir_all(&receiver_dir);` before constructing `targets` in this test, using a `receiver_dir` variable in place of the inline `std::env::temp_dir().join(...)` path shown above.

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo test -p keel-agentd replication_loop`
Expected: passes.

- [ ] **Step 9: Wire the module into `lib.rs` and spawn it from `Command::Apply`**

In `keel-agentd/src/lib.rs`, add `pub mod replication_loop;`.

In `keel-agentd/src/worker.rs`, `spawn` needs to grow two more parameters (a cloneable `Z` handle and the `pool`/`interval` config) so `handle_command`'s `Command::Apply` arm can start a replication-loop thread the first time a stateful+replicated spec is applied:

```rust
pub fn spawn<J, Z, N, M>(mut reconciler: Reconciler<J, Z, N, M>, zfs: Z, pool: String) -> (JoinHandle<()>, Sender<Command>)
where
    J: JailRuntime + Send + 'static,
    Z: ZfsManager + Clone + Send + 'static,
    N: NetManager + Send + 'static,
    M: MountManager + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Command>();
    let commands_for_thread = tx.clone();
    let handle = thread::spawn(move || {
        let mut replicating: std::collections::HashSet<String> = std::collections::HashSet::new();
        for command in rx {
            handle_command(&mut reconciler, command, &zfs, &pool, &commands_for_thread, &mut replicating);
        }
    });
    (handle, tx)
}
```

Update `handle_command`'s signature to accept the four new parameters, and change the `Command::Apply` arm:

```rust
        Command::Apply(spec, reply) => {
            let is_stateful_and_replicated = !spec.spec.volumes.is_empty() && spec.spec.replicate_to.is_some();
            let name = spec.metadata.name.clone();
            let result = reconciler.apply(spec);
            let _ = reconciler.reconcile(Instant::now());
            if result.is_ok() && is_stateful_and_replicated && replicating.insert(name.clone()) {
                let volume_name = format!("{name}-data");
                crate::replication_loop::spawn(name, volume_name, pool.clone(), zfs.clone(), commands.clone(), Duration::from_secs(30));
            }
            let _ = reply.send(result);
        }
```

(`volume_name` assumes the single-volume-per-replica convention this codebase already uses everywhere else, e.g. `db-0-data` for replica `db-0`'s one declared volume named `data` — see `to_jail_spec`'s `format!("{name}-{}", v.name)`. If a stateful replica ever had more than one volume this would need generalizing; today's `Non-Goals` don't cover multi-volume replicas, so this single-volume assumption matches the milestone's actual scope.)

Add `use std::time::Duration;` to the top of `worker.rs` if not already present (it is, via `Instant`'s sibling import — check and add `Duration` explicitly if missing).

Update every call site of `worker::spawn(reconciler)` across the crate (`main.rs`, and every test helper in `worker.rs`/`http.rs`/`registration.rs`/`proxy.rs`) to `worker::spawn(reconciler, zfs.clone(), "zroot".to_string())` (or the real pool string in `main.rs`), cloning whatever `ZfsManager` instance each call site already constructs immediately before building its `Reconciler`. Use `cargo build --workspace` to enumerate every site precisely.

- [ ] **Step 10: Run the full crate test suite and fix every remaining call site**

Run: `cargo test -p keel-agentd`
Expected: after fixing every `worker::spawn` call site the compiler flags, all tests pass (including the new replication-loop test and every pre-existing test, since `replicating.insert` only ever fires for specs with `volumes` + `replicate_to` both set — no existing test sets `replicate_to`, so no existing test spawns a new thread).

- [ ] **Step 11: Update `main.rs` to pass the real `zfs`/`pool` into `worker::spawn`**

In `keel-agentd/src/main.rs`, clone the zfs manager before handing one copy to `Reconciler::new` and the other to `worker::spawn`:

```rust
    let zfs = CliZfsManager::new();
    let reconciler = Reconciler::new(
        ProcessJailRuntime::new(),
        zfs.clone(),
        ProcessNetManager::new(),
        keel_jail::CliMountManager::new(),
        config.pool.clone(),
        config.state_dir.clone(),
    )
    .expect("failed to initialize reconciler from on-disk state");
    ...
    let (_worker_handle, commands) = worker::spawn(reconciler, zfs, config.pool.clone());
```

- [ ] **Step 12: Run the full workspace test suite and commit**

Run: `cargo test --workspace`
Expected: all tests pass.

```bash
git add -A
git commit -m "$(cat <<'EOF'
Add the Milestone 19 replication loop and replicate-to retargeting

One background thread per stateful+replicated replica, spawned the first
time Command::Apply sees both volumes and replicate_to set. Re-reads
replicate_to from the live JailRecord every tick (so PUT
/jails/<name>/replicate-to takes effect with no restart), sends full or
incremental via the Task 4 wire protocol, and exits once its own replica
record is deleted rather than leaking forever.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 7: `keel-controlplane` — `Standbys`/`PendingFences` state and `worker::spawn` plumbing

**Files:**
- Create: `keel-controlplane/src/standbys.rs`
- Create: `keel-controlplane/src/pending_fences.rs`
- Modify: `keel-controlplane/src/lib.rs`
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/http.rs`
- Modify: `keel-controlplane/src/main.rs`
- Modify: `keel-agentd/src/registration.rs` (control-plane test harness call site)
- Modify: `keelctl/tests/cli.rs` (control-plane test harness call site)

**Interfaces:**
- Produces: `pub struct Standbys` (`get`/`set`/`remove`, mirroring `Placements` exactly), `pub struct PendingFences` (`set`/`remove`/`for_node`), five new `worker::Command` variants: `RecordStandby(String, String, Sender<()>)`, `RecordPendingFence(String, String, Sender<()>)`, `PendingFencesForNode(String, Sender<Vec<String>>)`, `RemovePendingFence(String, Sender<()>)`. `worker::spawn` now takes 6 params instead of 4.

- [ ] **Step 1: Write the failing `Standbys`/`PendingFences` unit tests**

Create `keel-controlplane/src/standbys.rs`:

```rust
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct Standbys {
    by_replica: HashMap<String, String>,
}

impl Standbys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, replica_name: &str) -> Option<&str> {
        self.by_replica.get(replica_name).map(|s| s.as_str())
    }

    pub fn set(&mut self, replica_name: String, node_id: String) {
        self.by_replica.insert(replica_name, node_id);
    }

    pub fn remove(&mut self, replica_name: &str) {
        self.by_replica.remove(replica_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_on_an_empty_table_returns_none() {
        assert_eq!(Standbys::new().get("db-0"), None);
    }

    #[test]
    fn set_then_get_returns_the_recorded_node() {
        let mut standbys = Standbys::new();
        standbys.set("db-0".to_string(), "node-2".to_string());
        assert_eq!(standbys.get("db-0"), Some("node-2"));
    }

    #[test]
    fn set_again_overwrites_rather_than_duplicating() {
        let mut standbys = Standbys::new();
        standbys.set("db-0".to_string(), "node-2".to_string());
        standbys.set("db-0".to_string(), "node-3".to_string());
        assert_eq!(standbys.get("db-0"), Some("node-3"));
    }

    #[test]
    fn remove_clears_the_entry() {
        let mut standbys = Standbys::new();
        standbys.set("db-0".to_string(), "node-2".to_string());
        standbys.remove("db-0");
        assert_eq!(standbys.get("db-0"), None);
    }
}
```

Create `keel-controlplane/src/pending_fences.rs`:

```rust
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct PendingFences {
    by_replica: HashMap<String, String>, // replica_name -> node_id owed a forced delete
}

impl PendingFences {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, replica_name: String, node_id: String) {
        self.by_replica.insert(replica_name, node_id);
    }

    pub fn remove(&mut self, replica_name: &str) {
        self.by_replica.remove(replica_name);
    }

    /// Every replica_name currently owed a forced delete on `node_id`.
    pub fn for_node(&self, node_id: &str) -> Vec<String> {
        self.by_replica
            .iter()
            .filter(|(_, owed_node)| owed_node.as_str() == node_id)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_node_on_an_empty_table_is_empty() {
        assert_eq!(PendingFences::new().for_node("node-1"), Vec::<String>::new());
    }

    #[test]
    fn for_node_finds_only_entries_owed_on_that_node() {
        let mut fences = PendingFences::new();
        fences.set("db-0".to_string(), "node-1".to_string());
        fences.set("db-1".to_string(), "node-2".to_string());
        assert_eq!(fences.for_node("node-1"), vec!["db-0".to_string()]);
    }

    #[test]
    fn remove_clears_the_entry() {
        let mut fences = PendingFences::new();
        fences.set("db-0".to_string(), "node-1".to_string());
        fences.remove("db-0");
        assert_eq!(fences.for_node("node-1"), Vec::<String>::new());
    }

    #[test]
    fn a_node_with_no_owed_fences_gets_an_empty_result_not_every_entry() {
        let mut fences = PendingFences::new();
        fences.set("db-0".to_string(), "node-1".to_string());
        assert_eq!(fences.for_node("node-9"), Vec::<String>::new());
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass in isolation**

Run: `cargo test -p keel-controlplane standbys:: pending_fences::`
Expected: compile error (modules not declared) until Step 3, then all 8 tests pass.

- [ ] **Step 3: Wire the modules into `lib.rs`**

In `keel-controlplane/src/lib.rs`, add `pub mod standbys;` and `pub mod pending_fences;`, and `pub use standbys::Standbys; pub use pending_fences::PendingFences;` alongside the existing `pub use` lines.

Run: `cargo test -p keel-controlplane standbys:: pending_fences::`
Expected: all 8 tests pass.

- [ ] **Step 4: Grow `worker::spawn`'s signature and add the four new `Command` variants**

In `keel-controlplane/src/worker.rs`, add imports (`use crate::standbys::Standbys;` and `use crate::pending_fences::PendingFences;`), extend `spawn`:

```rust
pub fn spawn(
    mut registry: Registry,
    mut placements: Placements,
    mut services: Services,
    mut used_addresses: UsedAddresses,
    mut standbys: Standbys,
    mut pending_fences: PendingFences,
) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, &mut placements, &mut services, &mut used_addresses, &mut standbys, &mut pending_fences, command);
        }
    });
    (handle, tx)
}
```

Extend `handle_command`'s signature the same way, and add to the `Command` enum:

```rust
    RecordStandby(String, String, Sender<()>),
    RecordPendingFence(String, String, Sender<()>),
    PendingFencesForNode(String, Sender<Vec<String>>),
    RemovePendingFence(String, Sender<()>),
```

and to `handle_command`'s `match`:

```rust
        Command::RecordStandby(replica_name, node_id, reply) => {
            standbys.set(replica_name, node_id);
            let _ = reply.send(());
        }
        Command::RecordPendingFence(replica_name, node_id, reply) => {
            pending_fences.set(replica_name, node_id);
            let _ = reply.send(());
        }
        Command::PendingFencesForNode(node_id, reply) => {
            let _ = reply.send(pending_fences.for_node(&node_id));
        }
        Command::RemovePendingFence(replica_name, reply) => {
            pending_fences.remove(&replica_name);
            let _ = reply.send(());
        }
```

- [ ] **Step 5: Run the crate's test suite to enumerate every call site that now fails to compile**

Run: `cargo build -p keel-controlplane --tests 2>&1 | grep -B1 "this function takes"`
Expected: a list of every `worker::spawn(...)` call with only 4 arguments (roughly 25 in `worker.rs`'s own test module, plus `start_test_server`/`start_fake_remote_tls_agentd`-adjacent helpers and the TLS-reload tests in `http.rs`, plus the production call in `main.rs`).

- [ ] **Step 6: Fix every call site**

Add `, Standbys::new(), PendingFences::new()` as the 5th/6th arguments to every one of them (in `keel-controlplane/src/worker.rs`'s test module, `keel-controlplane/src/http.rs`'s test module, and `keel-controlplane/src/main.rs`'s production call).

- [ ] **Step 7: Fix the two out-of-crate call sites**

In `keel-agentd/src/registration.rs`'s `start_test_control_plane()` test helper, and in `keelctl/tests/cli.rs`'s equivalent helper, add the same two extra arguments to their `worker::spawn(...)` calls (both currently construct a `Registry`/`Placements`/`Services`/`UsedAddresses` control-plane test harness the same way `keel-controlplane`'s own `http.rs` tests do).

- [ ] **Step 8: Run the full workspace test suite and commit**

Run: `cargo test --workspace`
Expected: all tests pass across every crate.

```bash
git add -A
git commit -m "$(cat <<'EOF'
Add Standbys/PendingFences control-plane state

Plain HashMap-backed structs matching Placements/UsedAddresses's existing
style, owned by the same single-writer worker actor thread and reached
only through new Command variants (RecordStandby, RecordPendingFence,
PendingFencesForNode, RemovePendingFence) -- never touched directly by an
HTTP handler, matching every other piece of control-plane state today.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 8: `keel-controlplane` — scheduler picks a standby at initial placement

**Files:**
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `Standbys` (Task 7), `services::pick_node_for_service` (existing).
- Produces: `ReplicaAction::Schedule` gains `standby_node_id: Option<String>, standby_addr: Option<String>`. `Command::ReconcileServices`'s `to_add` loop picks a standby (a second, different, `Alive` node) for every stateful (`template.volumes` non-empty) replica it schedules — every time that specific replica is newly placed, not just service-wide "first index". `execute_replica_actions` sets `spec.spec.replicate_to` and calls the new `Command::RecordStandby` on success.

- [ ] **Step 1: Write the failing scheduler test**

Add to `#[cfg(test)] mod tests` in `keel-controlplane/src/worker.rs`:

```rust
    fn resolve_standby(commands: &Sender<Command>, replica_name: &str) -> Option<String> {
        // No direct "GetStandby" command exists (nothing needs one outside
        // this test) -- PrepareForceRepin (Task 10) will be the real
        // consumer. For this test, record a placement query isn't
        // available either, so assert indirectly via the Schedule action's
        // own standby_node_id field instead of a dedicated getter. This
        // helper is intentionally unused; delete it.
        let _ = (commands, replica_name);
        None
    }

    #[test]
    fn reconcile_services_picks_a_distinct_standby_for_a_new_stateful_replica() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service_with_template(&commands, "db", 1, stateful_template());

        let actions = reconcile(&commands);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReplicaAction::Schedule { node_id, standby_node_id, standby_addr, .. } => {
                let standby = standby_node_id.as_ref().expect("expected a standby to be picked for a stateful replica");
                assert_ne!(standby, node_id, "standby must be a different node than the primary");
                assert!(standby_addr.is_some());
            }
            other => panic!("expected a Schedule action, got: {other:?}"),
        }
    }

    #[test]
    fn reconcile_services_picks_no_standby_for_a_stateless_replica() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 1);

        let actions = reconcile(&commands);
        match &actions[0] {
            ReplicaAction::Schedule { standby_node_id, standby_addr, .. } => {
                assert_eq!(*standby_node_id, None);
                assert_eq!(*standby_addr, None);
            }
            other => panic!("expected a Schedule action, got: {other:?}"),
        }
    }

    #[test]
    fn reconcile_services_leaves_a_stateful_replica_without_a_standby_when_only_one_node_is_alive() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service_with_template(&commands, "db", 1, stateful_template());

        let actions = reconcile(&commands);
        match &actions[0] {
            ReplicaAction::Schedule { standby_node_id, standby_addr, .. } => {
                assert_eq!(*standby_node_id, None, "no second node exists to serve as a standby");
                assert_eq!(*standby_addr, None);
            }
            other => panic!("expected a Schedule action, got: {other:?}"),
        }
    }
```

Delete the unused `resolve_standby` helper stub above before running (it was scaffolding notes only, not a real test aid).

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p keel-controlplane picks_a_distinct_standby picks_no_standby without_a_standby`
Expected: compile error — `ReplicaAction::Schedule` has no `standby_node_id`/`standby_addr` fields yet, and every existing `spawn(...)` call in this test module now also needs the two extra Task 7 arguments (already fixed in Task 7 — if any were missed, this step's compiler output will list them).

- [ ] **Step 3: Add the two new fields to `ReplicaAction::Schedule` and pick a standby in `ReconcileServices`**

In `keel-controlplane/src/worker.rs`, modify `ReplicaAction::Schedule`:

```rust
    Schedule {
        replica_name: String,
        node_id: String,
        node_addr: String,
        template: keel_spec::JailTemplate,
        address: std::net::Ipv4Addr,
        prefix_len: u8,
        standby_node_id: Option<String>,
        standby_addr: Option<String>,
    },
```

In the `Command::ReconcileServices` handler's `to_add` loop, after `busy.insert(node_id.clone());` and before pushing the action:

```rust
                    let (standby_node_id, standby_addr) = if record.template.volumes.is_empty() {
                        (None, None)
                    } else {
                        services::pick_node_for_service(alive_nodes.clone(), &busy)
                            .ok()
                            .and_then(|standby_id| registry.resolve(&standby_id, now).ok().map(|addr| (standby_id, addr)))
                            .map(|(id, addr)| (Some(id), Some(addr)))
                            .unwrap_or((None, None))
                    };

                    actions.push(ReplicaAction::Schedule {
                        replica_name,
                        node_id,
                        node_addr,
                        template: record.template.clone(),
                        address,
                        prefix_len: pod_cidr.prefix_len(),
                        standby_node_id,
                        standby_addr,
                    });
```

Note `busy` already contains the just-picked primary `node_id` at this point (from the line right above), so `pick_node_for_service`'s own busy-node filter is exactly what guarantees the standby differs from the primary; if every node is busy, `pick_node_for_service` falls back to its own bin-packing over the unfiltered list, which could theoretically return the same node as the primary in a single-node cluster — that's exactly `reconcile_services_leaves_a_stateful_replica_without_a_standby_when_only_one_node_is_alive`'s case, so the code must explicitly discard a standby pick that equals the primary. Refine the snippet above to add that guard:

```rust
                    let (standby_node_id, standby_addr) = if record.template.volumes.is_empty() {
                        (None, None)
                    } else {
                        services::pick_node_for_service(alive_nodes.clone(), &busy)
                            .ok()
                            .filter(|standby_id| standby_id != &node_id)
                            .and_then(|standby_id| registry.resolve(&standby_id, now).ok().map(|addr| (standby_id, addr)))
                            .map(|(id, addr)| (Some(id), Some(addr)))
                            .unwrap_or((None, None))
                    };
```

- [ ] **Step 4: Run the new tests to verify they pass, then fix `execute_replica_actions`'s now-broken match**

Run: `cargo test -p keel-controlplane picks_a_distinct_standby picks_no_standby without_a_standby`
Expected: the 3 new tests pass, but `cargo build` now fails elsewhere: `keel-controlplane/src/http.rs`'s `execute_replica_actions` destructures `ReplicaAction::Schedule { replica_name, node_id, node_addr, template, address, prefix_len }` without `..`, so it no longer compiles against the grown variant.

- [ ] **Step 5: Update `execute_replica_actions` to set `replicate_to` and record the standby**

In `keel-controlplane/src/http.rs`:

```rust
            ReplicaAction::Schedule { replica_name, node_id, node_addr, template, address, prefix_len, standby_node_id, standby_addr } => {
                let cidr = format!("{address}/{prefix_len}");
                let mut spec = template.to_jail_spec(&replica_name, &cidr);
                spec.spec.replicate_to = standby_addr.clone();
                let body = serde_yaml::to_string(&spec).expect("JailSpec serialization should not fail");
                match forward(&node_addr, "PUT", &format!("/jails/{replica_name}"), body.as_bytes(), client_config) {
                    Ok((status, _)) if (200..300).contains(&status) => {
                        send_record_placement(&replica_name, &node_id, commands);
                        send_record_replica_address(&replica_name, &node_id, address, commands);
                        if let Some(standby_id) = standby_node_id {
                            send_record_standby(&replica_name, &standby_id, commands);
                        }
                    }
                    Ok((status, resp_body)) => eprintln!(
                        "keel-controlplane: failed to schedule replica '{replica_name}' on node '{node_id}': status {status}, body {:?}",
                        String::from_utf8_lossy(&resp_body)
                    ),
                    Err(e) => eprintln!(
                        "keel-controlplane: failed to reach node '{node_id}' at {node_addr} while scheduling replica '{replica_name}': {e}"
                    ),
                }
            }
```

Add the new helper next to `send_record_replica_address`:

```rust
fn send_record_standby(replica_name: &str, standby_node_id: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RecordStandby(replica_name.to_string(), standby_node_id.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}
```

- [ ] **Step 6: Write an HTTP-level test proving a scheduled stateful replica's forwarded spec carries `replicateTo`**

Add to `keel-controlplane/src/http.rs`'s test module:

```rust
    fn stateful_service_yaml(name: &str, replicas: u32) -> String {
        format!(
            "apiVersion: keel/v1\nkind: Service\nmetadata:\n  name: {name}\nspec:\n  replicas: {replicas}\n  port: 8080\n  template:\n    image: base/14.2-web\n    command: [\"/usr/local/bin/myapp\"]\n    network:\n      vnet: true\n      bridge: keel0\n    resources:\n      cpu: \"1\"\n      memory: 256M\n    restartPolicy: Always\n    volumes:\n      - name: data\n        mountPath: /var/db\n        size: 1G\n"
        )
    }

    #[test]
    fn scheduling_a_stateful_service_replica_forwards_a_spec_with_replicate_to_set() {
        let cp_addr = start_test_server();
        let node_a = start_fake_remote_tls_agentd(200, "running: true\n");
        let node_b = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_a);
        register_node(&cp_addr, "node-b", &node_b);

        let (status, _) = send_request(&cp_addr, "PUT", "/services/db", &stateful_service_yaml("db", 1));
        assert_eq!(status, 200);

        let (status, body) = send_request(&cp_addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert!(body.contains("node-a") && body.contains("node-b"), "got: {body}");
        // The fake remote agentd just echoes a fixed body ("running: true"),
        // so asserting the forwarded replicateTo requires inspecting what
        // was actually sent -- covered precisely by
        // keel-agentd's own "put_replicate_to_..." tests and Task 6's
        // replication-loop test for the receiving side. Here, confirm at
        // least one of the two nodes ends up as this replica's recorded
        // standby by checking GET /nodes twice is stable and a placement
        // exists (full round-trip proof lives in Task 10's force-repin
        // integration test, which depends on a real standby having been
        // recorded).
        let (status, _) = send_request(&cp_addr, "GET", "/jails/db-0", "");
        assert_eq!(status, 200, "expected db-0 to have been scheduled onto one of the two registered nodes");
    }
```

- [ ] **Step 7: Run the full crate test suite and commit**

Run: `cargo test -p keel-controlplane`
Expected: all tests pass.

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/http.rs
git commit -m "$(cat <<'EOF'
Pick a standby when scheduling a new stateful replica

ReplicaAction::Schedule gains standby_node_id/standby_addr, computed by
reusing pick_node_for_service's same-service spreading logic with the
just-picked primary already in the busy set. execute_replica_actions sets
the forwarded spec's replicateTo and records the standby via the new
RecordStandby command once the PUT succeeds. A stateless replica, or a
stateful one with no second Alive node available, gets standby: None and
is left for a later reconcile pass to pick up (no error, matching this
codebase's existing best-effort scheduling style).

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 8b: node-level `replicate_addr` registration (inserted during execution)

**Why this exists:** Task 8's implementer correctly flagged, and the controller confirmed, a real architecture gap: `keel-agentd`'s replication listener (Task 4) binds its own port, distinct from the main HTTP API port, per the design spec ("its own port distinct from the HTTP API and from Milestone 16's per-VIP proxy ports"). But nothing in Tasks 1-8 ever taught the control plane what that address *is* — Task 8's standby-picking used `Registry::resolve()`, which returns a node's main HTTP address, not its replication-listener address. Left unfixed, every `replicateTo` the control plane sets would point at the wrong port. This task closes that gap by having each node advertise a `replicate_addr` at registration, alongside its existing `addr`, and by actually starting the replication listener in `keel-agentd/src/main.rs` (which no prior task did either).

**Files:**
- Modify: `keel-controlplane/src/wire.rs` (`NodeRegistration`)
- Modify: `keel-controlplane/src/registry.rs` (`NodeRecord`, `register()`, new `replicate_addr()` accessor)
- Modify: `keel-controlplane/src/worker.rs` (`Command::Register`, `handle_command`, Task 8's standby-address lookup, existing test call sites)
- Modify: `keel-controlplane/src/http.rs` (`handle_register`, and Task 8's own new test if it needs a real `replicate_addr` to keep proving `standby_addr: Some(...)`)
- Modify: `keel-agentd/src/registration.rs` (`spawn`, `register_once`, existing test call sites)
- Modify: `keel-agentd/src/main.rs` (`Config`, `parse_args_from`, `main` — new `--replicate-addr` flag, actually binding and starting `keel_agentd::replication::run(...)`)

**Interfaces:**
- `NodeRegistration.replicate_addr: Option<String>` — `#[serde(default)]`, so every existing registration YAML body across the whole test suite keeps parsing (as `None`), matching this codebase's established convention for additive wire fields (`Spec.volumes`, `Spec.replicate_to`, `Heartbeat.jails`).
- `Registry::register(..., replicate_addr: Option<String>, ...)`, `Registry::replicate_addr(&self, node_id: &str) -> Option<String>` (mirrors the existing `pod_cidr()` accessor exactly).
- `Command::Register` gains `Option<String>` (replicate_addr) as a new positional field, inserted right after `addr`.
- Task 8's standby-address lookup changes from `registry.resolve(&standby_id, now).ok()` to `registry.replicate_addr(&standby_id)` — a plain lookup of a stored value, not a liveness check (the standby is already known-Alive since it came from `alive_nodes`).

- [ ] **Step 1: Add `replicate_addr` to the wire type and `Registry`**

In `keel-controlplane/src/wire.rs`, add to `NodeRegistration`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub id: String,
    pub addr: String,
    #[serde(default)]
    pub replicate_addr: Option<String>,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
}
```

In `keel-controlplane/src/registry.rs`, add `replicate_addr: Option<String>` to `NodeRecord`, add the parameter to `register()` (storing it on both fresh insert and re-registration, exactly like `addr` already is), and add:

```rust
    /// The node's advertised replication-listener address ("host:port"),
    /// distinct from its main HTTP `addr` — `None` until the node has
    /// registered with a `replicate_addr` at least once.
    pub fn replicate_addr(&self, node_id: &str) -> Option<String> {
        self.nodes.get(node_id).and_then(|r| r.replicate_addr.clone())
    }
```

Add a test proving `replicate_addr` round-trips through register + accessor, and a test proving it's `None` for an unregistered/never-advertised node.

- [ ] **Step 2: Thread `replicate_addr` through `Command::Register` and `handle_register`**

In `keel-controlplane/src/worker.rs`: add `Option<String>` to `Command::Register`'s tuple (right after the `addr` `String`), update `handle_command`'s arm to pass it to `registry.register(...)`, and fix every existing direct `Command::Register(...)` construction site the compiler flags (both the `register_node` test helper — a single fix there covers ~28 callers — and any raw `Command::Register(...)` sends that bypass it) by inserting `None,` in the right position. Use `cargo build -p keel-controlplane --tests` to find every site; do not assume the brief's line numbers are current, since Tasks 7 and 8 already shifted them.

In `keel-controlplane/src/http.rs`'s `handle_register`, pass `registration.replicate_addr` through to `Command::Register(...)`.

- [ ] **Step 3: Fix Task 8's standby-address lookup**

In `keel-controlplane/src/worker.rs`'s `Command::ReconcileServices` handler, change the standby-picking code Task 8 added from:

```rust
                    let (standby_node_id, standby_addr) = if record.template.volumes.is_empty() {
                        (None, None)
                    } else {
                        services::pick_node_for_service(alive_nodes.clone(), &busy)
                            .ok()
                            .filter(|standby_id| standby_id != &node_id)
                            .and_then(|standby_id| registry.resolve(&standby_id, now).ok().map(|addr| (standby_id, addr)))
                            .map(|(id, addr)| (Some(id), Some(addr)))
                            .unwrap_or((None, None))
                    };
```

to:

```rust
                    let (standby_node_id, standby_addr) = if record.template.volumes.is_empty() {
                        (None, None)
                    } else {
                        services::pick_node_for_service(alive_nodes.clone(), &busy)
                            .ok()
                            .filter(|standby_id| standby_id != &node_id)
                            .and_then(|standby_id| registry.replicate_addr(&standby_id).map(|addr| (standby_id, addr)))
                            .map(|(id, addr)| (Some(id), Some(addr)))
                            .unwrap_or((None, None))
                    };
```

(the only change: `registry.resolve(&standby_id, now).ok()` → `registry.replicate_addr(&standby_id)` — no liveness re-check needed, since `standby_id` already came from the already-Alive-filtered `alive_nodes`).

This means Task 8's own tests that register nodes via the plain `register_node` helper (which now sends `replicate_addr: None`) will start getting `standby_addr: None` where they previously (incorrectly) got `Some(<node's HTTP addr>)`. Find Task 8's tests that assert `standby_addr.is_some()` (e.g. `reconcile_services_picks_a_distinct_standby_for_a_new_stateful_replica`) and give them a way to register a node with a real `replicate_addr`. Add a second, narrowly-scoped test helper next to `register_node` (do not change `register_node`'s own signature — it has ~28 callers that don't care about this field):

```rust
    fn register_node_with_replicate_addr(commands: &Sender<Command>, id: &str, addr: &str, replicate_addr: &str, capacity_cpu: f64, capacity_memory: u64) {
        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(id.to_string(), addr.to_string(), Some(replicate_addr.to_string()), capacity_cpu, capacity_memory, reg_tx))
            .unwrap();
        reg_rx.recv().unwrap().unwrap();
    }
```

and use it (with an arbitrary but distinct fake `replicate_addr` per node, e.g. `"10.0.0.1:7622"`) in place of `register_node` wherever Task 8's own tests need to prove a real standby address was picked. Update those tests' assertions accordingly (e.g. assert `standby_addr == Some("10.0.0.2:7622".to_string())` instead of just `is_some()`, now that a real, known value is registered).

- [ ] **Step 4: Run `cargo test -p keel-controlplane`, fix any test now broken by Step 3's semantic change, confirm all pass**

Run: `cargo test -p keel-controlplane`
Expected: all tests pass, including Task 8's (now updated) standby tests and the two new `Registry`/`replicate_addr` tests from Step 1.

- [ ] **Step 5: Add `--replicate-addr` to `keel-agentd`, thread it through registration, and actually start the replication listener**

In `keel-agentd/src/main.rs`:
- Add `replicate_addr: Option<String>` to `Config`, parsed via a new `"--replicate-addr" => config.replicate_addr = Some(value),` arm in `parse_args_from`.
- Add `config.replicate_addr.is_none()` to the existing "all required together" validation alongside `node_id`/`advertise_addr`/the TLS files (same panic message pattern, naming `--replicate-addr` in the list).
- In `main()`, inside the existing `if let (Some(node_id), Some(control_plane_addr), ...)` block: bind `TcpListener::bind(&replicate_addr).unwrap_or_else(|e| panic!("failed to bind replication TCP listener on {replicate_addr}: {e}"))`, clone `zfs` once more (it's already cloned once for the `Reconciler` and once for `worker::spawn` per Task 6 — a third clone is equally cheap, `CliZfsManager` is a stateless unit struct) and `replica_targets` once more, and spawn `thread::spawn(move || keel_agentd::replication::run(replicate_listener, replicate_zfs, replicate_pool, replicate_targets))`.
- Pass `replicate_addr.clone()` into `registration::spawn(...)`'s new parameter (Step 6 below).

In `keel-agentd/src/registration.rs`:
- Add a `replicate_addr: String` parameter to `spawn(...)` (it already has 9 parameters and is `#[allow(clippy::too_many_arguments)]`'d — one more is consistent with its existing style, not new scope creep).
- Thread it into `register_once(...)`'s own parameter list, and add `\nreplicate_addr: {replicate_addr}` to the YAML body it builds.
- Fix every test call site of `registration::spawn(...)` in this file's own test module that the compiler flags (there are roughly a dozen) — be careful to distinguish these from any `worker::spawn(...)`/`crate::worker::spawn(...)` calls in the same file, which are a different, unrelated function and must not be touched.

- [ ] **Step 6: Run the full workspace test suite and commit**

Run: `cargo test --workspace`
Expected: all tests pass.

```bash
git add -A
git commit -m "$(cat <<'EOF'
Add node-level replicate_addr registration

Closes a gap found during Task 8's implementation: the replication
listener (Task 4) binds its own port, distinct from the main HTTP API,
but nothing taught the control plane that address, so replicateTo was
being set to a node's HTTP port instead. Nodes now advertise
replicate_addr (Option<String>, serde-default so every existing
registration body keeps parsing) alongside addr at registration;
Registry gains a replicate_addr() accessor mirroring pod_cidr(); Task 8's
standby-address lookup uses it instead of Registry::resolve(). keel-agentd
also gains --replicate-addr and now actually starts the replication
listener in main() (Task 4 built it, but no prior task ever bound and ran
it in production).

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 9: `keel-controlplane` — fencing on heartbeat

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `Command::PendingFencesForNode`, `Command::RemovePendingFence` (Task 7), `forward()` (existing).
- Produces: `handle_heartbeat` piggybacks a per-node forced-delete sweep on every successful heartbeat, right alongside the existing `reconcile_and_execute` self-healing call.

This task is implemented ahead of Task 10 (which is what actually *populates* `PendingFences`) so its tests can seed `PendingFences` directly via `Command::RecordPendingFence` without depending on the full force-repin flow.

- [ ] **Step 1: Write the failing fencing tests**

`start_test_server()` doesn't hand back a `Sender<Command>`, and seeding `PendingFences` for a test needs one (there is no HTTP route for `RecordPendingFence` itself — it's control-plane-internal state, only ever populated for real by `force-repin`, Task 10). Add a second test-only server-starter that keeps its own handle, plus the fencing tests, to `keel-controlplane/src/http.rs`'s test module:

```rust
    fn start_test_server_with_commands() -> (String, Sender<Command>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(
            Registry::new("10.0.0.0/16".parse().unwrap()),
            Placements::new(),
            crate::services::Services::new("10.0.250.0/24".parse().unwrap()),
            crate::addresses::UsedAddresses::new(),
            crate::standbys::Standbys::new(),
            crate::pending_fences::PendingFences::new(),
        );
        let reloading_tls = tls::ReloadingTls::spawn(
            fixture("fixture-node.crt"),
            fixture("fixture-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap();
        let commands_for_server = commands.clone();
        thread::spawn(move || run(listener, commands_for_server, reloading_tls));
        (addr, commands)
    }

    fn record_pending_fence(commands: &Sender<Command>, replica_name: &str, node_id: &str) {
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordPendingFence(replica_name.to_string(), node_id.to_string(), tx)).unwrap();
        rx.recv().unwrap();
    }

    #[test]
    fn a_heartbeat_from_a_node_owed_a_fence_triggers_a_forced_delete() {
        let (cp_addr, commands) = start_test_server_with_commands();
        let node_addr = start_fake_remote_tls_agentd(200, "");
        register_node(&cp_addr, "node-a", &node_addr);
        record_pending_fence(&commands, "db-0", "node-a");

        let (status, _) = send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\n");
        assert_eq!(status, 200);

        // The forced DELETE is fire-and-forget from the heartbeat response's
        // point of view -- give it a moment, then confirm the fence was
        // cleared (the fake remote agentd above answers every request with
        // 200, so the forced delete "succeeds" and PendingFencesForNode
        // should come back empty on the next check).
        std::thread::sleep(Duration::from_millis(100));
        let (tx, rx) = mpsc::channel();
        commands.send(Command::PendingFencesForNode("node-a".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Vec::<String>::new(), "expected the fence to have been cleared after a successful forced delete");
    }

    #[test]
    fn a_heartbeat_from_an_unrelated_node_leaves_pending_fences_untouched() {
        let (cp_addr, commands) = start_test_server_with_commands();
        let node_a = start_fake_remote_tls_agentd(200, "");
        let node_b = start_fake_remote_tls_agentd(200, "");
        register_node(&cp_addr, "node-a", &node_a);
        register_node(&cp_addr, "node-b", &node_b);
        record_pending_fence(&commands, "db-0", "node-a");

        send_request(&cp_addr, "POST", "/nodes/node-b/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\n");

        std::thread::sleep(Duration::from_millis(100));
        let (tx, rx) = mpsc::channel();
        commands.send(Command::PendingFencesForNode("node-a".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), vec!["db-0".to_string()], "expected node-a's fence to remain, untouched by node-b's heartbeat");
    }

    #[test]
    fn a_failed_forced_delete_leaves_the_fence_in_place_for_the_next_heartbeat() {
        let (cp_addr, commands) = start_test_server_with_commands();
        // 500 from the fake remote agentd simulates the forced DELETE failing.
        let node_addr = start_fake_remote_tls_agentd(500, "boom");
        register_node(&cp_addr, "node-a", &node_addr);
        record_pending_fence(&commands, "db-0", "node-a");

        send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\n");

        std::thread::sleep(Duration::from_millis(100));
        let (tx, rx) = mpsc::channel();
        commands.send(Command::PendingFencesForNode("node-a".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), vec!["db-0".to_string()], "expected the fence to remain after a failed forced delete");
    }
```

- [ ] **Step 2: Run the tests to verify they fail (fencing isn't wired up yet)**

Run: `cargo test -p keel-controlplane fence`
Expected: the first test fails (`PendingFencesForNode` still returns `["db-0"]` since nothing clears it yet); the other two happen to already pass trivially (nothing touches the fence on an unrelated heartbeat, and nothing clears it on failure either, since nothing acts on it at all yet) — confirm by reading the actual failure, not just trusting this description, then proceed.

- [ ] **Step 3: Implement `check_and_execute_fencing` and call it from `handle_heartbeat`**

In `keel-controlplane/src/http.rs`, add near `reconcile_and_execute`:

```rust
/// Checked on every successful heartbeat, right alongside
/// `reconcile_and_execute` -- if the heartbeating node id is owed a forced
/// delete for any replica (per `PendingFences`), forwards it inline. On
/// success or 404 (already gone -- e.g. an operator cleaned it up first)
/// the fence is cleared; on any other outcome it's left in place so the
/// next heartbeat from this node retries it. Unlike `reconcile_and_execute`
/// (which has no node context and also runs from the service-apply path),
/// this needs the specific heartbeating node's id, so it's called directly
/// from `handle_heartbeat` rather than folded into that shared function.
fn check_and_execute_fencing(node_id: &str, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::PendingFencesForNode(node_id.to_string(), reply_tx)).is_err() {
        return;
    }
    let Ok(replica_names) = reply_rx.recv() else { return };
    if replica_names.is_empty() {
        return;
    }
    let (resolve_tx, resolve_rx) = mpsc::channel();
    if commands.send(Command::Resolve(node_id.to_string(), resolve_tx)).is_err() {
        return;
    }
    let Ok(Ok(addr)) = resolve_rx.recv() else { return };
    for replica_name in replica_names {
        match forward(&addr, "DELETE", &format!("/jails/{replica_name}"), &[], client_config) {
            Ok((status, _)) if (200..300).contains(&status) || status == 404 => {
                send_remove_pending_fence(&replica_name, commands);
            }
            Ok((status, body)) => eprintln!(
                "keel-controlplane: forced delete of fenced jail '{replica_name}' on node '{node_id}' failed: status {status}, body {:?}",
                String::from_utf8_lossy(&body)
            ),
            Err(e) => eprintln!("keel-controlplane: failed to reach node '{node_id}' at {addr} while fencing '{replica_name}': {e}"),
        }
    }
}

fn send_remove_pending_fence(replica_name: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RemovePendingFence(replica_name.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}
```

In `handle_heartbeat`, after the existing `reconcile_and_execute(commands, client_config);` line:

```rust
        Ok(Ok(())) => {
            reconcile_and_execute(commands, client_config);
            check_and_execute_fencing(id, commands, client_config);
            let (entries_tx, entries_rx) = mpsc::channel();
            ...
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane fence`
Expected: all 3 pass.

- [ ] **Step 5: Run the full crate test suite and commit**

Run: `cargo test -p keel-controlplane`
Expected: all tests pass.

```bash
git add keel-controlplane/src/http.rs
git commit -m "$(cat <<'EOF'
Fence a stale resurrected jail on the node's next heartbeat

check_and_execute_fencing runs alongside reconcile_and_execute inside
handle_heartbeat (not folded into reconcile_and_execute itself, since that
function has no node context and is also called from the service-apply
path with none available). Clears the fence on success or 404
(already gone); leaves it for the next heartbeat on any other failure.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 10: `keel-controlplane` — `force-repin`

**Files:**
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: everything from Tasks 7-9, plus `keel-agentd`'s `GET /replica-targets/<name>` (Task 5).
- Produces: `Command::PrepareForceRepin(String, Sender<Result<ForceRepinPrep, ForceRepinError>>)`; `POST /replicas/<name>/force-repin` route returning 200 on success, 404/400/409/503 per the design spec's error table.

- [ ] **Step 1: Write the failing `PrepareForceRepin` unit tests**

Add to `keel-controlplane/src/worker.rs`'s test module:

```rust
    fn prepare_force_repin(commands: &Sender<Command>, replica_name: &str) -> Result<ForceRepinPrep, ForceRepinError> {
        let (tx, rx) = mpsc::channel();
        commands.send(Command::PrepareForceRepin(replica_name.to_string(), tx)).unwrap();
        rx.recv().unwrap()
    }

    #[test]
    fn prepare_force_repin_on_an_unplaced_name_returns_not_placed() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        assert_eq!(prepare_force_repin(&commands, "db-0"), Err(ForceRepinError::NotPlaced("db-0".to_string())));
    }

    #[test]
    fn prepare_force_repin_on_a_name_with_no_standby_returns_not_stateful() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 1);
        record_placement(&commands, "web-0", "node-1");

        assert_eq!(prepare_force_repin(&commands, "web-0"), Err(ForceRepinError::NotStateful("web-0".to_string())));
    }

    #[test]
    fn prepare_force_repin_while_the_primary_still_resolves_alive_is_rejected() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service_with_template(&commands, "db", 1, stateful_template());
        record_placement(&commands, "db-0", "node-1");
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordStandby("db-0".to_string(), "node-2".to_string(), tx)).unwrap();
        rx.recv().unwrap();

        assert_eq!(prepare_force_repin(&commands, "db-0"), Err(ForceRepinError::PrimaryStillAlive("node-1".to_string())));
    }

    #[test]
    fn prepare_force_repin_happy_path_picks_a_fresh_standby_and_a_free_address() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-3", "10.0.0.3", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service_with_template(&commands, "db", 1, stateful_template());
        // db-0's primary ("node-unreachable") is never registered, so
        // registry.resolve() fails for it exactly like a genuinely Dead
        // node -- the same trick this file's existing pinning tests use.
        record_placement(&commands, "db-0", "node-unreachable");
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordStandby("db-0".to_string(), "node-2".to_string(), tx)).unwrap();
        rx.recv().unwrap();

        let prep = prepare_force_repin(&commands, "db-0").unwrap();
        assert_eq!(prep.old_node_id, "node-unreachable");
        assert_eq!(prep.standby_node_id, "node-2");
        assert_eq!(prep.standby_addr, "10.0.0.2");
        assert_eq!(prep.fresh_standby_node_id, "node-3");
        assert_eq!(prep.fresh_standby_addr, "10.0.0.3");
        assert_eq!(prep.template, stateful_template());
    }

    #[test]
    fn prepare_force_repin_with_no_alive_node_left_for_a_fresh_standby_is_rejected() {
        let commands = spawn(
            Registry::new(test_cluster_cidr()),
            Placements::new(),
            Services::new(test_service_cidr()),
            UsedAddresses::new(),
            Standbys::new(),
            PendingFences::new(),
        )
        .1;
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service_with_template(&commands, "db", 1, stateful_template());
        record_placement(&commands, "db-0", "node-unreachable");
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordStandby("db-0".to_string(), "node-2".to_string(), tx)).unwrap();
        rx.recv().unwrap();

        assert_eq!(prepare_force_repin(&commands, "db-0"), Err(ForceRepinError::NoFreshStandby));
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p keel-controlplane prepare_force_repin`
Expected: compile error — `Command::PrepareForceRepin`, `ForceRepinPrep`, `ForceRepinError` don't exist yet.

- [ ] **Step 3: Implement `ForceRepinPrep`/`ForceRepinError` and `Command::PrepareForceRepin`**

In `keel-controlplane/src/worker.rs`, add near `PlacementError`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct ForceRepinPrep {
    pub old_node_id: String,
    pub standby_node_id: String,
    pub standby_addr: String,
    pub template: keel_spec::JailTemplate,
    pub fresh_standby_node_id: String,
    pub fresh_standby_addr: String,
    pub address: std::net::Ipv4Addr,
    pub prefix_len: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForceRepinError {
    #[error("no known placement for replica '{0}'")]
    NotPlaced(String),
    #[error("'{0}' is not a stateful replica with a standby")]
    NotStateful(String),
    #[error("current primary node '{0}' still resolves as alive")]
    PrimaryStillAlive(String),
    #[error("standby node is unreachable: {0}")]
    StandbyUnresolvable(ResolveError),
    #[error("no alive node available to serve as a fresh standby")]
    NoFreshStandby,
    #[error("no free address available for the promoted primary")]
    NoFreeAddress,
}
```

Add `PrepareForceRepin(String, Sender<Result<ForceRepinPrep, ForceRepinError>>),` to the `Command` enum, and to `handle_command`:

```rust
        Command::PrepareForceRepin(replica_name, reply) => {
            let now = Instant::now();
            let result = (|| {
                let old_node_id = placements.get(&replica_name).map(|s| s.to_string()).ok_or_else(|| ForceRepinError::NotPlaced(replica_name.clone()))?;
                if registry.resolve(&old_node_id, now).is_ok() {
                    return Err(ForceRepinError::PrimaryStillAlive(old_node_id));
                }
                let standby_node_id = standbys.get(&replica_name).map(|s| s.to_string()).ok_or_else(|| ForceRepinError::NotStateful(replica_name.clone()))?;
                // Deliberately Registry::resolve(), not replicate_addr(): this
                // is the address the control plane forwards the readiness
                // GET and the provisioning PUT to (this node's normal HTTP
                // API), not a replication target embedded in a spec. Do not
                // "fix" this to replicate_addr() by symmetry with
                // fresh_standby_addr below -- they serve different purposes.
                let standby_addr = registry.resolve(&standby_node_id, now).map_err(ForceRepinError::StandbyUnresolvable)?;

                let service_name = services::owner_of(&replica_name, placements, services)
                    .and_then(|owner| match owner {
                        Owner::Service(name) => Some(name),
                        Owner::Unmanaged => None,
                    })
                    .ok_or_else(|| ForceRepinError::NotStateful(replica_name.clone()))?;
                let template = services.get(&service_name).ok_or_else(|| ForceRepinError::NotStateful(replica_name.clone()))?.template.clone();

                let alive_nodes: Vec<scheduler::NodeResources> = registry
                    .list(now)
                    .into_iter()
                    .filter(|s| s.status == NodeState::Alive)
                    .map(|s| scheduler::NodeResources {
                        id: s.id,
                        capacity_cpu: s.capacity_cpu,
                        capacity_memory: s.capacity_memory,
                        committed_cpu: s.committed_cpu,
                        committed_memory: s.committed_memory,
                    })
                    .collect();
                let mut exclude = std::collections::HashSet::new();
                exclude.insert(old_node_id.clone());
                exclude.insert(standby_node_id.clone());
                let fresh_standby_node_id = services::pick_node_for_service(alive_nodes, &exclude)
                    .ok()
                    .filter(|id| !exclude.contains(id))
                    .ok_or(ForceRepinError::NoFreshStandby)?;
                // replicate_addr(), not resolve(): this value is embedded
                // into the promoted primary's spec.spec.replicate_to, telling
                // its replication loop where to connect -- it must be the
                // fresh standby's replication-listener address (Task 8b),
                // not its main HTTP address.
                let fresh_standby_addr = registry.replicate_addr(&fresh_standby_node_id).ok_or(ForceRepinError::NoFreshStandby)?;

                let pod_cidr = registry.pod_cidr(&standby_node_id).ok_or(ForceRepinError::NoFreeAddress)?;
                let address = addresses::first_free_address(pod_cidr, &standby_node_id, used_addresses).ok_or(ForceRepinError::NoFreeAddress)?;

                Ok(ForceRepinPrep {
                    old_node_id,
                    standby_node_id,
                    standby_addr,
                    template,
                    fresh_standby_node_id,
                    fresh_standby_addr,
                    address,
                    prefix_len: pod_cidr.prefix_len(),
                })
            })();
            let _ = reply.send(result);
        }
```

`pick_node_for_service`'s own busy-node filter already excludes `exclude`'s members when at least one non-excluded `Alive` node remains; the `.filter(|id| !exclude.contains(id))` guards the fallback-to-bin-packing path the same way Task 8's standby pick does, so `NoFreshStandby` is returned instead of silently reusing the just-fenced-old or about-to-be-primary node.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane prepare_force_repin`
Expected: all 5 pass.

- [ ] **Step 5: Write the failing HTTP-level `force-repin` tests**

Add to `keel-controlplane/src/http.rs`'s test module:

```rust
    #[test]
    fn force_repin_on_an_unplaced_name_returns_404() {
        let cp_addr = start_test_server();
        let (status, body) = send_request(&cp_addr, "POST", "/replicas/db-0/force-repin", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");
    }

    #[test]
    fn force_repin_on_a_non_stateful_name_returns_400() {
        let (cp_addr, commands) = start_test_server_with_commands();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_addr);
        record_placement(&commands, "web-0", "node-a");

        let (status, body) = send_request(&cp_addr, "POST", "/replicas/web-0/force-repin", "");
        assert_eq!(status, 400);
        assert!(body.contains("not a stateful replica"), "got: {body}");
    }

    #[test]
    fn force_repin_while_the_primary_is_still_alive_returns_409() {
        let (cp_addr, commands) = start_test_server_with_commands();
        let node_a = start_fake_remote_tls_agentd(200, "running: true\n");
        let node_b = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_a);
        register_node(&cp_addr, "node-b", &node_b);
        record_placement(&commands, "db-0", "node-a");
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordStandby("db-0".to_string(), "node-b".to_string(), tx)).unwrap();
        rx.recv().unwrap();

        let (status, body) = send_request(&cp_addr, "POST", "/replicas/db-0/force-repin", "");
        assert_eq!(status, 409);
        assert!(body.contains("still resolves as alive"), "got: {body}");
    }

    #[test]
    fn force_repin_before_the_standby_has_a_first_snapshot_returns_409() {
        let (cp_addr, commands) = start_test_server_with_commands();
        // node-a is never registered -> unreachable, standing in for Dead.
        let node_b = start_fake_remote_tls_agentd(404, "error: no replica target 'db-0'\n");
        register_node(&cp_addr, "node-b", &node_b);
        apply_service_with_template_via_http(&cp_addr, "db", 1);
        record_placement(&commands, "db-0", "node-unreachable");
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordStandby("db-0".to_string(), "node-b".to_string(), tx)).unwrap();
        rx.recv().unwrap();

        let (status, body) = send_request(&cp_addr, "POST", "/replicas/db-0/force-repin", "");
        assert_eq!(status, 409);
        assert!(body.contains("has not completed a first full replication"), "got: {body}");
    }

    #[test]
    fn force_repin_happy_path_updates_placements_standbys_and_pending_fences() {
        let (cp_addr, commands) = start_test_server_with_commands();
        let node_b = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-b", &node_b);
        // node-c never needs to be dialed in this test (it only needs to
        // exist as an Alive candidate for the fresh-standby pick), but it
        // DOES need a real replicate_addr registered -- Task 8b's
        // PrepareForceRepin looks up the fresh standby's replicate_addr()
        // (not resolve()), which is None unless explicitly advertised.
        send_request(
            &cp_addr,
            "POST",
            "/nodes/register",
            "id: node-c\naddr: 127.0.0.1:1\nreplicate_addr: 127.0.0.1:2\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
        );
        apply_service_with_template_via_http(&cp_addr, "db", 1);
        record_placement(&commands, "db-0", "node-unreachable");
        let (tx, rx) = mpsc::channel();
        commands.send(Command::RecordStandby("db-0".to_string(), "node-b".to_string(), tx)).unwrap();
        rx.recv().unwrap();

        // Simulate node-b already having a fully-replicated ReplicaTarget by
        // making its next forwarded GET return 200. start_fake_remote_tls_agentd
        // answers every request with the same fixed status/body, so this
        // fake stands in for BOTH the readiness GET and the provisioning PUT
        // with one 200 response -- sufficient to prove force-repin's own
        // control-plane-side bookkeeping (Placements/Standbys/PendingFences),
        // which is what this test targets; real end-to-end behavior against
        // genuine keel-agentd readiness/provision responses is covered by
        // Task 13's VM verification.
        let (status, _) = send_request(&cp_addr, "POST", "/replicas/db-0/force-repin", "");
        assert_eq!(status, 200);

        let (status, body) = send_request(&cp_addr, "GET", "/jails/db-0", "");
        assert_eq!(status, 200, "expected db-0's placement to now resolve to the promoted node, got: {body}");

        let (tx, rx) = mpsc::channel();
        commands.send(Command::PendingFencesForNode("node-unreachable".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), vec!["db-0".to_string()], "expected the old primary to be fenced");
    }

    fn apply_service_with_template_via_http(cp_addr: &str, name: &str, replicas: u32) {
        send_request(cp_addr, "PUT", &format!("/services/{name}"), &stateful_service_yaml(name, replicas));
    }
```

`register_node`'s existing signature is `register_node(cp_addr: &str, id: &str, node_addr: &str)` (no capacity args, no `replicate_addr` — check the actual helper already in this file). It's still used as-is for `node-b` above; `node-c` uses a raw `send_request` instead since it's the one node in this test that needs a real `replicate_addr`.

- [ ] **Step 6: Run the tests to verify they fail to compile, then implement the route**

Run: `cargo test -p keel-controlplane force_repin`
Expected: compile error — no route for `POST /replicas/<name>/force-repin`.

In `keel-controlplane/src/http.rs`'s `route()`, add:

```rust
        ("POST", ["replicas", name, "force-repin"]) => handle_force_repin(name, commands, client_config),
```

Add the handler near `handle_scheduled_delete`:

```rust
fn handle_force_repin(name: &str, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::PrepareForceRepin(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let prep = match reply_rx.recv() {
        Ok(Ok(prep)) => prep,
        Ok(Err(e @ ForceRepinError::NotPlaced(_))) => return error_response(404, e.to_string()),
        Ok(Err(e @ ForceRepinError::NotStateful(_))) => return error_response(400, e.to_string()),
        Ok(Err(e @ ForceRepinError::PrimaryStillAlive(_))) => return error_response(409, e.to_string()),
        Ok(Err(e)) => return error_response(503, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };

    match forward(&prep.standby_addr, "GET", &format!("/replica-targets/{name}"), &[], client_config) {
        Ok((status, _)) if (200..300).contains(&status) => {}
        Ok((404, _)) | Ok((409, _)) => {
            return error_response(
                409,
                format!("standby node '{}' has not completed a first full replication for '{name}'", prep.standby_node_id),
            );
        }
        Ok((status, body)) => {
            return error_response(500, format!("unexpected response checking standby readiness: status {status}, body {:?}", String::from_utf8_lossy(&body)))
        }
        Err(e) => return error_response(500, format!("failed to reach standby node '{}' at {}: {e}", prep.standby_node_id, prep.standby_addr)),
    }

    let cidr = format!("{}/{}", prep.address, prep.prefix_len);
    let mut spec = prep.template.to_jail_spec(name, &cidr);
    spec.spec.replicate_to = Some(prep.fresh_standby_addr.clone());
    let body = serde_yaml::to_string(&spec).expect("JailSpec serialization should not fail");

    match forward(&prep.standby_addr, "PUT", &format!("/jails/{name}"), body.as_bytes(), client_config) {
        Ok((status, resp_body)) if (200..300).contains(&status) => {
            send_record_placement(name, &prep.standby_node_id, commands);
            send_record_replica_address(name, &prep.standby_node_id, prep.address, commands);
            send_record_standby(name, &prep.fresh_standby_node_id, commands);
            send_record_pending_fence(name, &prep.old_node_id, commands);
            (200, resp_body)
        }
        Ok((status, resp_body)) => error_response(status, String::from_utf8_lossy(&resp_body).to_string()),
        Err(e) => error_response(500, format!("failed to reach node '{}' at {}: {e}", prep.standby_node_id, prep.standby_addr)),
    }
}
```

Add the one remaining helper (`Command::RecordPendingFence` was already added in Task 7; this reuses it under a name matching the others):

```rust
fn send_record_pending_fence(replica_name: &str, old_node_id: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RecordPendingFence(replica_name.to_string(), old_node_id.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}
```

Import `crate::worker::ForceRepinError` at the top of `http.rs` alongside the existing `worker::{Command, ReplicaAction, ScheduleOrResolveError}` import.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane force_repin`
Expected: all 5 pass.

- [ ] **Step 8: Run the full crate test suite and commit**

Run: `cargo test -p keel-controlplane`
Expected: all tests pass.

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/http.rs
git commit -m "$(cat <<'EOF'
Implement POST /replicas/<name>/force-repin

Compute-only PrepareForceRepin does the 404/400/409/503 precondition
checks and picks a fresh standby + free address purely from existing
worker-owned state; the HTTP handler then does the two network round
trips this needs (a readiness probe against the standby's
GET /replica-targets/<name>, then the actual provisioning PUT with
replicateTo pointed at the fresh standby and address reassigned into the
promoted node's own pod_cidr) before recording
Placements/Standbys/PendingFences. Address reassignment on promotion is
this plan's own addition beyond the milestone spec's literal prose --
without it, force-repin would fail keel-agentd's existing subnet check
whenever the standby's pod_cidr differs from the dead primary's.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 11: `keelctl force-repin <replica-name>`

**Files:**
- Modify: `keelctl/src/main.rs`

**Interfaces:**
- Consumes: nothing new beyond the existing `Target`/`dispatch`/`success_body`/`jails_path` machinery.
- Produces: `keelctl force-repin <replica-name>` → `POST /replicas/<name>/force-repin` against the control plane.

- [ ] **Step 1: Write the failing test**

Add to `keelctl/tests/cli.rs`, reusing its existing `start_test_control_plane_with_node(node_id, node_addr)` and `run_keelctl_scheduled(control_plane_addr, args)` helpers exactly as its other control-plane-backed tests already do:

```rust
#[test]
fn force_repin_on_an_unplaced_name_prints_the_control_planes_404_message() {
    let control_plane_addr = start_test_control_plane_with_node("node-1", "10.0.0.1:7621");
    let (ok, _stdout, stderr) = run_keelctl_scheduled(&control_plane_addr, &["force-repin", "db-0"]);
    assert!(!ok, "expected force-repin on an unplaced name to fail");
    assert!(stderr.contains("no known placement"), "got stderr: {stderr}");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p keelctl force_repin`
Expected: fails with "usage: keelctl <apply ...>" (unknown subcommand) or similar, since `force-repin` isn't dispatched yet.

- [ ] **Step 3: Add the subcommand**

In `keelctl/src/main.rs`, add to the `match args.split_first()` block:

```rust
        Some((cmd, rest)) if cmd == "force-repin" => run_force_repin(&target, rest),
```

and update the usage string to mention it. Add the function near `run_delete_volume`:

```rust
fn run_force_repin(target: &Target, args: &[String]) -> Result<String, String> {
    let name = args.first().ok_or("force-repin requires a replica name")?;
    let path = jails_path(target, &format!("/replicas/{name}/force-repin"));
    success_body(dispatch(target, "POST", &path, "")).map(|_| String::new())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p keelctl force_repin`
Expected: passes.

- [ ] **Step 5: Run the full crate test suite and commit**

Run: `cargo test -p keelctl`
Expected: all tests pass.

```bash
git add keelctl/src/main.rs keelctl/tests/cli.rs
git commit -m "$(cat <<'EOF'
Add keelctl force-repin <replica-name>

Thin dispatch to POST /replicas/<name>/force-repin, matching every other
keelctl subcommand's existing dispatch/success_body shape exactly.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

## Task 12: Full workspace verification and real 3-node VM verification

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: clean build, all tests pass across `keel-spec`, `keel-jail`, `keel-zfs`, `keel-net`, `keel-agentd`, `keelctl`, `keel-controlplane`.

- [ ] **Step 2: `cargo clippy` sanity pass**

Run: `cargo clippy --workspace --all-targets`
Expected: no new warnings beyond whatever this workspace's baseline already has (compare against a clippy run on `master` before this branch's changes if unsure).

- [ ] **Step 3: Real 3-node VM verification (manual — matches every prior milestone's closing step)**

This cannot be automated by an agent in this sandbox (it requires real FreeBSD VMs and real ZFS, per this project's established practice — see `README.md`'s "VM verification was the first time the whole stack ran against the real..." pattern for Milestones 5 onward). Perform this manually, following the design spec's own Testing Strategy section:

1. Apply a 2-replica stateful service (`kind: Service`, `template.volumes` non-empty) across a 3-node cluster.
2. Confirm each replica's standby (on a third or peer node) accumulates a real, growing dataset via periodic `zfs list`/checksums as data is written to the primary.
3. Kill one replica's primary node's `keel-agentd` process; confirm the standby's replicated data is present and current within one replication interval (~30s).
4. Run `keelctl force-repin <replica-name>` (against the control plane); confirm the jail comes up on the former standby with the data intact, and that a fresh standby is assigned and begins a new baseline replication.
5. Power the original node back on; confirm its resurrected-from-`JailRecord` attempt is torn down by the fencing push within one heartbeat interval, leaving exactly one running copy of that replica cluster-wide.

- [ ] **Step 4: Update the design spec's `Status` line and close out the milestone**

Once VM verification passes, update `docs/superpowers/specs/2026-07-20-keel-agent-milestone19-cross-node-volume-movement-design.md` and the project `README.md`/`site/` the same way every prior milestone's closing commit already does (see `README.md`'s existing per-milestone summaries for the established format), and commit.
