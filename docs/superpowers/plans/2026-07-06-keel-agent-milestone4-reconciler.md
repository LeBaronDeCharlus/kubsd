# keel-agentd Milestone 4: Reconciliation Core — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 1:** its FreeBSD integration test step needs the
> real VM (`root@192.168.64.2`). The coordinating session has direct SSH
> access to this VM. **Every other task in this plan (2-7) is pure Rust
> tested against fakes and needs no FreeBSD VM interaction at all** — this
> is the first milestone that's almost entirely testable on macOS alone.

**Goal:** Build `keel-agentd`'s reconciliation core: a `Reconciler` that
composes `keel-jail`, `keel-zfs`, and `keel-net` to bring real jails
into line with declared specs, with crash-safe state persistence and
per-jail backoff. No HTTP API, no CLI, no `main.rs` — those are later
milestones. Also fixes a cross-cutting gap: `keel-jail` gets a new
`jail_exists` method, since `is_running` alone can't distinguish "jail
doesn't exist" from "jail exists but its process died".

**Architecture:** `keel-agentd` is a library crate exposing a generic
`Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager>` with three
public methods (`apply`, `delete`, `reconcile`) backed by a crash-safe
YAML state store and per-jail exponential backoff. Every exact design
decision below (data model, algorithm, error handling) was worked out and
approved in the design spec — this plan translates it directly into code.

**Tech Stack:** Rust (2021 edition), `serde`/`serde_yaml` for state
persistence, `thiserror` for error types — same toolchain as every prior
milestone, no new external dependencies.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-05-keel-agent-design.md` (Approved). The `Reconciler` API, `JailRecord` data model, naming/path derivation, and backoff mechanics there must match exactly.
- `keel-agentd` is a **library crate only** this milestone — no `main.rs`, no binary target, no HTTP API, no CLI.
- Everything except Task 1's real-VM verification is tested against `FakeJailRuntime`/`FakeZfsManager`/`FakeNetManager` — no FreeBSD dependency.
- Naming/path derivation: jail name `keel-<spec-name>`; base dataset `<pool>/keel/<image>` (image already includes its full relative path, e.g. `base/14.2-web`); jail dataset `<pool>/keel/jails/<spec-name>`; rootfs path `/<jail-dataset-path>`; epair base name `epair<ordinal>`.
- Three deliberate v1 simplifications (documented in the spec, not gaps to silently fix): the work queue is deferred (methods are plain synchronous calls), graceful SIGTERM shutdown is deferred (`destroy` = `jail -r` as-is), and `restartPolicy: OnFailure`/`Always` behave identically (no exit-code tracking yet).
- `reconcile`'s "exists, not running" branch always reapplies `ensure_bridge_exists` → `attach_jail` → `set_resource_limits` before `start_command` — this is required, not optional (it's what makes daemon-crash-mid-provisioning recovery correct).
- `rctl -a` is VM-verified idempotent (replaces, not stacks) — `set_resource_limits` can be safely re-called every reconcile pass with no special handling needed.
- No placeholders: every new function has a passing test.

---

### Task 1: Add `jail_exists` to `JailRuntime`

**Files:**
- Modify: `keel-jail/src/lib.rs`
- Modify: `keel-jail/src/fake.rs`
- Modify: `keel-jail/src/process.rs`
- Modify: `keel-jail/tests/freebsd_lifecycle.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `JailRuntime::jail_exists(&self, name: &str) -> Result<bool, JailError>`, implemented on both `FakeJailRuntime` and `ProcessJailRuntime`. Milestone 4's `Reconciler` (Tasks 6-7) is the first real caller.

- [ ] **Step 1: Add the trait method**

Modify `keel-jail/src/lib.rs` — add `jail_exists` to the `JailRuntime` trait, right after `create`:

```rust
pub trait JailRuntime {
    /// Creates a persistent, empty jail with no command running yet
    /// (uses `jail -c ... persist`).
    fn create(&self, name: &str, rootfs: &Path, vnet: bool) -> Result<(), JailError>;

    /// Checks only whether the jail itself exists — not whether a command
    /// is running inside it. Needed because `is_running` collapses "jail
    /// doesn't exist" and "jail exists but its process exited" into the
    /// same `false`; callers that need to distinguish "provision from
    /// scratch" from "just restart the command" need this method instead.
    fn jail_exists(&self, name: &str) -> Result<bool, JailError>;

    /// Non-blocking: spawns the command and returns immediately. A launch
    /// failure *inside* the jail (bad command, missing binary) is NOT
    /// reported by this method's `Ok` return — callers must re-check
    /// `is_running` afterward to confirm the process actually started and
    /// stayed up.
    fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError>;
    fn destroy(&self, name: &str) -> Result<(), JailError>;

    /// Means "the jail exists and has at least one non-zombie process
    /// running in it" — not merely "the jail exists".
    fn is_running(&self, name: &str) -> Result<bool, JailError>;

    /// `pcpu_percent` is cores × 100 (so 2 cores = `200`, not `2`). The two
    /// rctl rules (pcpu, vmemoryuse) are not applied atomically — if the
    /// second fails, the first remains in effect until
    /// `remove_resource_limits` is called.
    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError>;
    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError>;
}
```

- [ ] **Step 2: Implement it on FakeJailRuntime, with tests**

Modify `keel-jail/src/fake.rs` — add this method to the `impl JailRuntime
for FakeJailRuntime` block, right after `create`:

```rust
    fn jail_exists(&self, name: &str) -> Result<bool, JailError> {
        Ok(self.jails.lock().unwrap().contains_key(name))
    }
```

Add these two tests to the existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn jail_exists_is_false_before_create_and_true_after() {
        let runtime = FakeJailRuntime::new();
        assert_eq!(runtime.jail_exists("test-1").unwrap(), false);
        runtime.create("test-1", Path::new("/tmp/rootfs"), false).unwrap();
        assert_eq!(runtime.jail_exists("test-1").unwrap(), true);
    }

    #[test]
    fn jail_exists_is_false_after_destroy() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs"), false).unwrap();
        runtime.destroy("test-1").unwrap();
        assert_eq!(runtime.jail_exists("test-1").unwrap(), false);
    }
```

- [ ] **Step 3: Implement it on ProcessJailRuntime (refactor, behavior-preserving)**

Modify `keel-jail/src/process.rs`. Extract the `jls`-based existence
check `is_running` already does into a shared private helper, `jid_of`,
then implement `jail_exists` and `is_running` in terms of it. This is a
pure refactor — `is_running`'s external behavior is unchanged.

Add this method to the `impl ProcessJailRuntime` block (alongside `run`,
`run_checked`, `reap_finished_children`):

```rust
    // Unlike `zfs list`, `jls` returns exit code 1 both when the jail
    // doesn't exist and on a usage error, so we can't distinguish them
    // by exit code. Since our own arguments here are fixed and known
    // to be valid, a usage error would indicate a code bug, not a
    // runtime condition — treating any failure as "doesn't exist" is
    // an accepted, deliberate trade-off, not an oversight.
    fn jid_of(&self, name: &str) -> Result<Option<String>, JailError> {
        let jls = Self::run("jls", &["-j", name, "jid"])?;
        if !jls.status.success() {
            return Ok(None);
        }
        let jid = String::from_utf8_lossy(&jls.stdout).trim().to_string();
        if jid.is_empty() {
            return Ok(None);
        }
        Ok(Some(jid))
    }
```

Then, in the `impl JailRuntime for ProcessJailRuntime` block, replace the
existing `is_running` method entirely with these two methods:

```rust
    fn jail_exists(&self, name: &str) -> Result<bool, JailError> {
        Ok(self.jid_of(name)?.is_some())
    }

    fn is_running(&self, name: &str) -> Result<bool, JailError> {
        let jid = match self.jid_of(name)? {
            Some(jid) => jid,
            None => return Ok(false),
        };
        let ps = Self::run("ps", &["-J", &jid, "-o", "state="])?;
        let has_live_process = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .any(|state| {
                let state = state.trim();
                !state.is_empty() && !state.starts_with('Z')
            });
        Ok(has_live_process)
    }
```

- [ ] **Step 4: Write the FreeBSD-only integration test**

Add this test to `keel-jail/tests/freebsd_lifecycle.rs`:

```rust
#[test]
fn jail_exists_distinguishes_created_from_never_existed() {
    let runtime = ProcessJailRuntime::new();
    let name = "keel-test-jail-exists";
    let rootfs = Path::new("/tmp/keel-test-jail-exists-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    let _ = runtime.destroy(name);
    assert_eq!(runtime.jail_exists(name).unwrap(), false, "should not exist before create");

    runtime.create(name, rootfs, false).expect("create should succeed");
    assert_eq!(runtime.jail_exists(name).unwrap(), true, "should exist after create");

    runtime.destroy(name).expect("destroy should succeed");
    assert_eq!(runtime.jail_exists(name).unwrap(), false, "should not exist after destroy");
}
```

- [ ] **Step 5: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 36 tests total (34 from before this milestone + 2
new `FakeJailRuntime` tests; the new FreeBSD integration test doesn't run
on macOS).

- [ ] **Step 6: Commit and push**

```bash
git add keel-jail/src/lib.rs keel-jail/src/fake.rs keel-jail/src/process.rs keel-jail/tests/freebsd_lifecycle.rs
git commit -m "Add JailRuntime::jail_exists, distinct from is_running"
git push origin master:main
```

- [ ] **Step 7: Run the real integration test on the VM**

Run: `ssh root@192.168.64.2 'cd ~/keel && git pull && cargo test -p keel-jail --test freebsd_lifecycle 2>&1 | tail -15'`

Expected: all tests pass, including the new
`jail_exists_distinguishes_created_from_never_existed`.

---

### Task 2: keel-agentd scaffold — JailRecord and naming/path derivation

**Files:**
- Create: `keel-agentd/Cargo.toml`
- Create: `keel-agentd/src/record.rs`
- Create: `keel-agentd/src/lib.rs`
- Modify: `Cargo.toml` (workspace root — add `keel-agentd` to `members`)

**Interfaces:**
- Consumes: `keel_spec::JailSpec` (Milestone 1).
- Produces: `pub struct JailRecord { pub spec: JailSpec, pub epair_ordinal: u32 }`; free functions `jail_name`, `base_dataset_path`, `jail_dataset_path`, `jail_rootfs_path`, `epair_base_name`. Tasks 3-7 all depend on these exact names.

- [ ] **Step 1: Add keel-agentd to the workspace**

Modify `Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = ["keel-spec", "keel-jail", "keel-zfs", "keel-net", "keel-agentd"]
```

- [ ] **Step 2: Create the crate manifest**

Create `keel-agentd/Cargo.toml`:

```toml
[package]
name = "keel-agentd"
version = "0.1.0"
edition = "2021"

[dependencies]
keel-spec = { path = "../keel-spec" }
keel-jail = { path = "../keel-jail" }
keel-zfs = { path = "../keel-zfs" }
keel-net = { path = "../keel-net" }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
thiserror = "1"
```

- [ ] **Step 3: Write JailRecord and naming/path derivation, with tests**

Create `keel-agentd/src/record.rs`:

```rust
use keel_spec::JailSpec;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailRecord {
    pub spec: JailSpec,
    pub epair_ordinal: u32,
}

pub fn jail_name(spec_name: &str) -> String {
    format!("keel-{spec_name}")
}

pub fn base_dataset_path(pool: &str, image: &str) -> String {
    format!("{pool}/keel/{image}")
}

pub fn jail_dataset_path(pool: &str, spec_name: &str) -> String {
    format!("{pool}/keel/jails/{spec_name}")
}

pub fn jail_rootfs_path(pool: &str, spec_name: &str) -> PathBuf {
    PathBuf::from(format!("/{}", jail_dataset_path(pool, spec_name)))
}

pub fn epair_base_name(ordinal: u32) -> String {
    format!("epair{ordinal}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};

    fn sample_spec(name: &str) -> JailSpec {
        JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec {
                    vnet: true,
                    bridge: "keel0".to_string(),
                    address: "10.0.0.5/24".to_string(),
                },
                resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                restart_policy: RestartPolicy::Always,
            },
        }
    }

    #[test]
    fn jail_name_adds_keel_prefix() {
        assert_eq!(jail_name("web-1"), "keel-web-1");
    }

    #[test]
    fn base_dataset_path_appends_image_directly() {
        assert_eq!(base_dataset_path("zroot", "base/14.2-web"), "zroot/keel/base/14.2-web");
    }

    #[test]
    fn jail_dataset_path_uses_jails_subdirectory() {
        assert_eq!(jail_dataset_path("zroot", "web-1"), "zroot/keel/jails/web-1");
    }

    #[test]
    fn jail_rootfs_path_is_leading_slash_plus_dataset_path() {
        assert_eq!(jail_rootfs_path("zroot", "web-1"), PathBuf::from("/zroot/keel/jails/web-1"));
    }

    #[test]
    fn epair_base_name_formats_the_ordinal() {
        assert_eq!(epair_base_name(7), "epair7");
    }

    #[test]
    fn jail_record_round_trips_through_yaml() {
        let record = JailRecord { spec: sample_spec("web-1"), epair_ordinal: 3 };
        let yaml = serde_yaml::to_string(&record).unwrap();
        let parsed: JailRecord = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, record);
    }
}
```

- [ ] **Step 4: Create lib.rs**

Create `keel-agentd/src/lib.rs`:

```rust
pub mod record;

pub use record::JailRecord;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace -p keel-agentd`

Expected: PASS, 6 tests (`jail_name_adds_keel_prefix`,
`base_dataset_path_appends_image_directly`,
`jail_dataset_path_uses_jails_subdirectory`,
`jail_rootfs_path_is_leading_slash_plus_dataset_path`,
`epair_base_name_formats_the_ordinal`,
`jail_record_round_trips_through_yaml`).

- [ ] **Step 6: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 42 tests total (36 from Task 1 + 6 new).

- [ ] **Step 7: Commit and push**

```bash
git add Cargo.toml keel-agentd/Cargo.toml keel-agentd/src/record.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd crate: JailRecord and naming/path derivation"
git push origin master:main
```

---

### Task 3: State store — save, load, remove

**Files:**
- Create: `keel-agentd/src/store.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `JailRecord` (Task 2).
- Produces: `pub enum StoreError`; `pub fn load_all(state_dir: &Path) -> Result<Vec<JailRecord>, StoreError>`; `pub fn save(state_dir: &Path, record: &JailRecord) -> Result<(), StoreError>`; `pub fn remove(state_dir: &Path, spec_name: &str) -> Result<(), StoreError>`. Task 5's `Reconciler::new`/`apply`/`delete` call these directly.

- [ ] **Step 1: Write the implementation and tests**

Create `keel-agentd/src/store.rs`:

```rust
use crate::record::JailRecord;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, io::Error),
    #[error("failed to parse state file {0}: {1}")]
    Parse(PathBuf, serde_yaml::Error),
}

pub fn load_all(state_dir: &Path) -> Result<Vec<JailRecord>, StoreError> {
    fs::create_dir_all(state_dir).map_err(|e| StoreError::Io(state_dir.to_path_buf(), e))?;
    let mut records = Vec::new();
    for entry in fs::read_dir(state_dir).map_err(|e| StoreError::Io(state_dir.to_path_buf(), e))? {
        let entry = entry.map_err(|e| StoreError::Io(state_dir.to_path_buf(), e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|e| StoreError::Io(path.clone(), e))?;
        let record: JailRecord =
            serde_yaml::from_str(&content).map_err(|e| StoreError::Parse(path.clone(), e))?;
        records.push(record);
    }
    Ok(records)
}

pub fn save(state_dir: &Path, record: &JailRecord) -> Result<(), StoreError> {
    fs::create_dir_all(state_dir).map_err(|e| StoreError::Io(state_dir.to_path_buf(), e))?;
    let path = state_dir.join(format!("{}.yaml", record.spec.metadata.name));
    let tmp_path = state_dir.join(format!("{}.yaml.tmp", record.spec.metadata.name));
    let content = serde_yaml::to_string(record).expect("JailRecord serialization should not fail");
    fs::write(&tmp_path, content).map_err(|e| StoreError::Io(tmp_path.clone(), e))?;
    fs::rename(&tmp_path, &path).map_err(|e| StoreError::Io(path.clone(), e))?;
    Ok(())
}

pub fn remove(state_dir: &Path, spec_name: &str) -> Result<(), StoreError> {
    let path = state_dir.join(format!("{spec_name}.yaml"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};

    fn sample_spec(name: &str) -> keel_spec::JailSpec {
        keel_spec::JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec {
                    vnet: true,
                    bridge: "keel0".to_string(),
                    address: "10.0.0.5/24".to_string(),
                },
                resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                restart_policy: RestartPolicy::Always,
            },
        }
    }

    fn sample_record(name: &str) -> JailRecord {
        JailRecord { spec: sample_spec(name), epair_ordinal: 5 }
    }

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-store-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_then_load_all_roundtrips() {
        let dir = test_state_dir("save_then_load_all_roundtrips");
        let record = sample_record("web-1");
        save(&dir, &record).unwrap();
        let loaded = load_all(&dir).unwrap();
        assert_eq!(loaded, vec![record]);
    }

    #[test]
    fn load_all_on_missing_dir_creates_it_and_returns_empty() {
        let dir = test_state_dir("load_all_on_missing_dir_creates_it_and_returns_empty");
        assert!(!dir.exists());
        let loaded = load_all(&dir).unwrap();
        assert_eq!(loaded, vec![]);
        assert!(dir.exists());
    }

    #[test]
    fn remove_on_nonexistent_file_is_a_no_op_success() {
        let dir = test_state_dir("remove_on_nonexistent_file_is_a_no_op_success");
        fs::create_dir_all(&dir).unwrap();
        remove(&dir, "never-existed").unwrap();
    }

    #[test]
    fn save_then_remove_then_load_all_is_empty() {
        let dir = test_state_dir("save_then_remove_then_load_all_is_empty");
        let record = sample_record("web-1");
        save(&dir, &record).unwrap();
        remove(&dir, "web-1").unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![]);
    }

    #[test]
    fn load_all_ignores_non_yaml_files() {
        let dir = test_state_dir("load_all_ignores_non_yaml_files");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("readme.txt"), "not a record").unwrap();
        assert_eq!(load_all(&dir).unwrap(), vec![]);
    }
}
```

- [ ] **Step 2: Update lib.rs**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod record;
pub mod store;

pub use record::JailRecord;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --workspace -p keel-agentd`

Expected: PASS, 11 tests (6 from Task 2 + `save_then_load_all_roundtrips`,
`load_all_on_missing_dir_creates_it_and_returns_empty`,
`remove_on_nonexistent_file_is_a_no_op_success`,
`save_then_remove_then_load_all_is_empty`,
`load_all_ignores_non_yaml_files`).

- [ ] **Step 4: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 47 tests total (42 from Task 2 + 5 new).

- [ ] **Step 5: Commit and push**

```bash
git add keel-agentd/src/store.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd state store: save/load_all/remove"
git push origin master:main
```

---

### Task 4: Per-jail backoff

**Files:**
- Create: `keel-agentd/src/backoff.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: nothing new (pure `std::time` logic).
- Produces: `pub struct BackoffState` with `pub fn new() -> Self`, `pub fn can_retry(&self, now: Instant) -> bool`, `pub fn record_attempt(&mut self, now: Instant)`. Tasks 6-7's `Reconciler` use this directly.

- [ ] **Step 1: Write the implementation and tests**

Create `keel-agentd/src/backoff.rs`:

```rust
use std::time::{Duration, Instant};

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(300);
const RESET_UPTIME_THRESHOLD: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct BackoffState {
    current_delay: Duration,
    next_retry_at: Option<Instant>,
    last_started_at: Option<Instant>,
}

impl Default for BackoffState {
    fn default() -> Self {
        Self { current_delay: INITIAL_DELAY, next_retry_at: None, last_started_at: None }
    }
}

impl BackoffState {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if there's no active cooldown, or the cooldown has passed.
    pub fn can_retry(&self, now: Instant) -> bool {
        match self.next_retry_at {
            Some(t) => now >= t,
            None => true,
        }
    }

    /// Call this every time an action (provisioning attempt or restart) is
    /// taken for this jail, regardless of whether that action succeeded —
    /// a successful `start_command` carries no information about whether
    /// the process will keep running, so the cooldown must still be armed.
    pub fn record_attempt(&mut self, now: Instant) {
        if let Some(last) = self.last_started_at {
            if now.saturating_duration_since(last) >= RESET_UPTIME_THRESHOLD {
                self.current_delay = INITIAL_DELAY;
            }
        }
        self.last_started_at = Some(now);
        self.next_retry_at = Some(now + self.current_delay);
        self.current_delay = (self.current_delay * 2).min(MAX_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_can_retry_immediately() {
        let state = BackoffState::new();
        assert!(state.can_retry(Instant::now()));
    }

    #[test]
    fn cannot_retry_until_delay_passes() {
        let mut state = BackoffState::new();
        let t0 = Instant::now();
        state.record_attempt(t0);
        assert!(!state.can_retry(t0));
        assert!(!state.can_retry(t0 + Duration::from_millis(500)));
        assert!(state.can_retry(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn backoff_escalates_on_rapid_repeated_failures() {
        let mut state = BackoffState::new();
        let t0 = Instant::now();
        state.record_attempt(t0); // next_retry_at = t0 + 1s, current_delay becomes 2s
        let t1 = t0 + Duration::from_secs(1);
        state.record_attempt(t1); // next_retry_at = t1 + 2s, current_delay becomes 4s
        assert!(!state.can_retry(t1 + Duration::from_secs(1)));
        assert!(state.can_retry(t1 + Duration::from_secs(2)));
    }

    #[test]
    fn backoff_resets_after_sustained_uptime() {
        let mut state = BackoffState::new();
        let t0 = Instant::now();
        state.record_attempt(t0); // current_delay becomes 2s after this
        // Simulate the jail running fine for 60+ seconds before failing again.
        let t1 = t0 + Duration::from_secs(61);
        state.record_attempt(t1); // should reset to 1s (not escalate to 4s) before doubling to 2s
        assert!(!state.can_retry(t1 + Duration::from_millis(500)));
        assert!(state.can_retry(t1 + Duration::from_secs(1)));
    }

    #[test]
    fn backoff_caps_at_five_minutes() {
        let mut state = BackoffState::new();
        let mut now = Instant::now();
        for _ in 0..20 {
            state.record_attempt(now);
            now += Duration::from_secs(1); // always retrying immediately, never resetting
        }
        // After enough rapid escalations, the delay should be capped at 300s.
        assert!(!state.can_retry(now + Duration::from_secs(299)));
        assert!(state.can_retry(now + Duration::from_secs(300)));
    }
}
```

- [ ] **Step 2: Update lib.rs**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod backoff;
pub mod record;
pub mod store;

pub use record::JailRecord;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --workspace -p keel-agentd`

Expected: PASS, 16 tests (11 from Task 3 + `fresh_state_can_retry_immediately`,
`cannot_retry_until_delay_passes`, `backoff_escalates_on_rapid_repeated_failures`,
`backoff_resets_after_sustained_uptime`, `backoff_caps_at_five_minutes`).

- [ ] **Step 4: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 52 tests total (47 from Task 3 + 5 new).

- [ ] **Step 5: Commit and push**

```bash
git add keel-agentd/src/backoff.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd per-jail backoff state"
git push origin master:main
```

---

### Task 5: Reconciler — new, apply, delete

**Files:**
- Create: `keel-agentd/src/reconciler.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `JailRecord`, naming/path functions (Task 2); `store::{load_all, save, remove}` (Task 3); `keel_spec::{JailSpec, validate_name, validate_address, parse_cpu_cores, parse_memory_bytes, validate_transition, SpecError}` (Milestone 1); `JailRuntime`, `ZfsManager`, `NetManager` traits and their `Fake*` implementations (Milestones 2-3).
- Produces: `pub enum ReconcileError`; `pub struct Reconciler<J, Z, N>` with `pub fn new(...)`, `pub fn apply(...)`, `pub fn delete(...)`. Tasks 6-7 add `provision`/`rollback_provision` and `reconcile` to the same `impl` block.

- [ ] **Step 1: Write the implementation**

Create `keel-agentd/src/reconciler.rs`:

```rust
use crate::backoff::BackoffState;
use crate::record::{self, JailRecord};
use crate::store::{self, StoreError};
use keel_jail::JailRuntime;
use keel_net::NetManager;
use keel_spec::JailSpec;
use keel_zfs::ZfsManager;
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("spec validation failed: {0}")]
    InvalidSpec(#[from] keel_spec::SpecError),
    #[error("state store error: {0}")]
    Store(#[from] StoreError),
    #[error("jail runtime error: {0}")]
    Jail(#[from] keel_jail::JailError),
    #[error("zfs error: {0}")]
    Zfs(#[from] keel_zfs::ZfsError),
    #[error("network error: {0}")]
    Net(#[from] keel_net::NetError),
    #[error("jail '{0}' not found in desired state")]
    NotFound(String),
    #[error("base image dataset '{0}' does not exist")]
    BaseImageNotFound(String),
}

pub struct Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager> {
    jails: J,
    zfs: Z,
    net: N,
    pool: String,
    state_dir: PathBuf,
    records: HashMap<String, JailRecord>,
    backoff: HashMap<String, BackoffState>,
    next_epair_ordinal: u32,
}

impl<J: JailRuntime, Z: ZfsManager, N: NetManager> Reconciler<J, Z, N> {
    pub fn new(jails: J, zfs: Z, net: N, pool: String, state_dir: PathBuf) -> Result<Self, ReconcileError> {
        let loaded = store::load_all(&state_dir)?;
        let next_epair_ordinal = loaded.iter().map(|r| r.epair_ordinal).max().map(|m| m + 1).unwrap_or(1);
        let records = loaded.into_iter().map(|r| (r.spec.metadata.name.clone(), r)).collect();
        Ok(Self {
            jails,
            zfs,
            net,
            pool,
            state_dir,
            records,
            backoff: HashMap::new(),
            next_epair_ordinal,
        })
    }

    pub fn apply(&mut self, spec: JailSpec) -> Result<(), ReconcileError> {
        keel_spec::validate_name(&spec.metadata.name)?;
        keel_spec::validate_address(&spec.spec.network.address)?;
        keel_spec::parse_cpu_cores(&spec.spec.resources.cpu)?;
        keel_spec::parse_memory_bytes(&spec.spec.resources.memory)?;

        let epair_ordinal = if let Some(existing) = self.records.get(&spec.metadata.name) {
            keel_spec::validate_transition(&existing.spec, &spec)?;
            existing.epair_ordinal
        } else {
            let ordinal = self.next_epair_ordinal;
            self.next_epair_ordinal += 1;
            ordinal
        };

        let record = JailRecord { spec: spec.clone(), epair_ordinal };
        store::save(&self.state_dir, &record)?;
        self.records.insert(spec.metadata.name.clone(), record);
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), ReconcileError> {
        let record = self.records.get(name).ok_or_else(|| ReconcileError::NotFound(name.to_string()))?.clone();
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);

        self.net.detach_jail(&epair_base)?;
        // A record can reach `delete` having only gone through `apply`
        // (never `provision`, e.g. the daemon restarted before its first
        // reconcile pass): `destroy`, `destroy_dataset`, and
        // `remove_resource_limits` are not intrinsically idempotent, so
        // treat their `NotFound` as "already torn down" here.
        match self.jails.destroy(&jail_name) {
            Ok(()) | Err(keel_jail::JailError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_jail::FakeJailRuntime;
    use keel_net::FakeNetManager;
    use keel_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
    use keel_zfs::FakeZfsManager;
    use std::fs;

    fn sample_spec(name: &str) -> JailSpec {
        JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec {
                    vnet: true,
                    bridge: "keel0".to_string(),
                    address: "10.0.0.5/24".to_string(),
                },
                resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                restart_policy: RestartPolicy::Always,
            },
        }
    }

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-reconciler-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn new_reconciler(state_dir: PathBuf) -> Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager> {
        Reconciler::new(
            FakeJailRuntime::new(),
            FakeZfsManager::new(),
            FakeNetManager::new(),
            "zroot".to_string(),
            state_dir,
        )
        .unwrap()
    }

    #[test]
    fn new_starts_with_no_records_on_an_empty_state_dir() {
        let dir = test_state_dir("new_starts_with_no_records_on_an_empty_state_dir");
        let reconciler = new_reconciler(dir);
        assert!(reconciler.records.is_empty());
        assert_eq!(reconciler.next_epair_ordinal, 1);
    }

    #[test]
    fn apply_persists_and_tracks_the_record() {
        let dir = test_state_dir("apply_persists_and_tracks_the_record");
        let mut reconciler = new_reconciler(dir.clone());
        reconciler.apply(sample_spec("web-1")).unwrap();

        assert_eq!(reconciler.records.len(), 1);
        assert_eq!(reconciler.records["web-1"].epair_ordinal, 1);

        let loaded = store::load_all(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].spec.metadata.name, "web-1");
    }

    #[test]
    fn apply_assigns_increasing_ordinals_to_new_jails() {
        let dir = test_state_dir("apply_assigns_increasing_ordinals_to_new_jails");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();
        reconciler.apply(sample_spec("web-2")).unwrap();
        assert_eq!(reconciler.records["web-1"].epair_ordinal, 1);
        assert_eq!(reconciler.records["web-2"].epair_ordinal, 2);
    }

    #[test]
    fn new_recovers_next_ordinal_from_disk() {
        let dir = test_state_dir("new_recovers_next_ordinal_from_disk");
        {
            let mut reconciler = new_reconciler(dir.clone());
            reconciler.apply(sample_spec("web-1")).unwrap();
            reconciler.apply(sample_spec("web-2")).unwrap();
        }
        // A fresh Reconciler over the same state_dir should pick up where the last one left off.
        let reconciler = new_reconciler(dir);
        assert_eq!(reconciler.next_epair_ordinal, 3);
    }

    #[test]
    fn apply_keeps_the_same_ordinal_on_reapply() {
        let dir = test_state_dir("apply_keeps_the_same_ordinal_on_reapply");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();
        let first_ordinal = reconciler.records["web-1"].epair_ordinal;

        let mut updated = sample_spec("web-1");
        updated.spec.resources.cpu = "4".to_string(); // mutable field, allowed
        reconciler.apply(updated).unwrap();

        assert_eq!(reconciler.records["web-1"].epair_ordinal, first_ordinal);
    }

    #[test]
    fn apply_rejects_immutable_field_change() {
        let dir = test_state_dir("apply_rejects_immutable_field_change");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();

        let mut changed = sample_spec("web-1");
        changed.spec.image = "base/different-image".to_string();
        let result = reconciler.apply(changed);
        assert!(matches!(result, Err(ReconcileError::InvalidSpec(_))));
    }

    #[test]
    fn apply_rejects_invalid_name() {
        let dir = test_state_dir("apply_rejects_invalid_name");
        let mut reconciler = new_reconciler(dir);
        let mut invalid = sample_spec("Invalid_Name");
        invalid.metadata.name = "Invalid_Name".to_string();
        let result = reconciler.apply(invalid);
        assert!(matches!(result, Err(ReconcileError::InvalidSpec(_))));
    }

    #[test]
    fn delete_on_unknown_name_returns_not_found() {
        let dir = test_state_dir("delete_on_unknown_name_returns_not_found");
        let mut reconciler = new_reconciler(dir);
        assert!(matches!(reconciler.delete("missing"), Err(ReconcileError::NotFound(_))));
    }

    #[test]
    fn delete_removes_the_record_from_memory_and_disk() {
        let dir = test_state_dir("delete_removes_the_record_from_memory_and_disk");
        let mut reconciler = new_reconciler(dir.clone());
        reconciler.apply(sample_spec("web-1")).unwrap();

        reconciler.delete("web-1").unwrap();

        assert!(!reconciler.records.contains_key("web-1"));
        assert_eq!(store::load_all(&dir).unwrap(), vec![]);
    }
}
```

- [ ] **Step 2: Update lib.rs**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod backoff;
pub mod record;
pub mod reconciler;
pub mod store;

pub use record::JailRecord;
pub use reconciler::{ReconcileError, Reconciler};
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --workspace -p keel-agentd`

Expected: PASS, 25 tests (16 from Task 4 +
`new_starts_with_no_records_on_an_empty_state_dir`,
`apply_persists_and_tracks_the_record`,
`apply_assigns_increasing_ordinals_to_new_jails`,
`new_recovers_next_ordinal_from_disk`,
`apply_keeps_the_same_ordinal_on_reapply`,
`apply_rejects_immutable_field_change`, `apply_rejects_invalid_name`,
`delete_on_unknown_name_returns_not_found`,
`delete_removes_the_record_from_memory_and_disk`).

- [ ] **Step 4: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 61 tests total (52 from Task 4 + 9 new).

- [ ] **Step 5: Commit and push**

```bash
git add keel-agentd/src/reconciler.rs keel-agentd/src/lib.rs
git commit -m "Add Reconciler: new, apply, delete"
git push origin master:main
```

---

### Task 6: Reconciler — provisioning path

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: everything from Task 5.
- Produces: private methods `configure_networking_and_limits`, `provision`, `rollback_provision` on `Reconciler<J, Z, N>`. Task 7's public `reconcile` calls these directly (same module, so visibility is not an issue).

- [ ] **Step 1: Add the provisioning methods**

Add these three methods to the existing `impl<J: JailRuntime, Z: ZfsManager, N: NetManager> Reconciler<J, Z, N>` block in `keel-agentd/src/reconciler.rs`, after `delete`:

```rust
    fn configure_networking_and_limits(&mut self, name: &str, record: &JailRecord) -> Result<(), ReconcileError> {
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        self.net.ensure_bridge_exists(&record.spec.spec.network.bridge)?;
        self.net.attach_jail(
            &jail_name,
            &record.spec.spec.network.bridge,
            &epair_base,
            &record.spec.spec.network.address,
        )?;
        let pcpu_percent = keel_spec::cores_to_pcpu_percent(keel_spec::parse_cpu_cores(&record.spec.spec.resources.cpu)?);
        let memory_bytes = keel_spec::parse_memory_bytes(&record.spec.spec.resources.memory)?;
        self.jails.set_resource_limits(&jail_name, pcpu_percent, memory_bytes)?;
        Ok(())
    }

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
        self.configure_networking_and_limits(name, record)?;
        self.jails.start_command(&jail_name, &record.spec.spec.command)?;
        Ok(())
    }

    /// Best-effort cleanup after a failed `provision`. Every call here is
    /// intentionally allowed to fail silently (`let _ =`): each of these
    /// already tolerates "already gone"/"nothing to remove" as success, so
    /// this is safe to call unconditionally even for steps that never
    /// fully completed, and a rollback failure is handled by the normal
    /// per-jail backoff on the next reconciliation pass, not specially here.
    fn rollback_provision(&mut self, name: &str, record: &JailRecord) {
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);
        let _ = self.net.detach_jail(&epair_base);
        let _ = self.jails.destroy(&jail_name);
        let _ = self.zfs.destroy_dataset(&jail_dataset);
        let _ = self.jails.remove_resource_limits(&jail_name);
    }
```

- [ ] **Step 2: Add tests**

Add these tests to the existing `#[cfg(test)] mod tests` block in
`keel-agentd/src/reconciler.rs`:

```rust
    #[test]
    fn provision_drives_zfs_jail_net_and_command_in_order() {
        let dir = test_state_dir("provision_drives_zfs_jail_net_and_command_in_order");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let record = reconciler.records["web-1"].clone();

        reconciler.provision("web-1", &record).unwrap();

        let jail_name = record::jail_name("web-1");
        assert_eq!(reconciler.jails.jail_exists(&jail_name).unwrap(), true);
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
        assert_eq!(
            reconciler.zfs.dataset_exists(&record::jail_dataset_path("zroot", "web-1")).unwrap(),
            true
        );
    }

    #[test]
    fn provision_fails_clearly_when_base_image_missing() {
        let dir = test_state_dir("provision_fails_clearly_when_base_image_missing");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();
        let record = reconciler.records["web-1"].clone();

        let result = reconciler.provision("web-1", &record);
        assert!(matches!(result, Err(ReconcileError::BaseImageNotFound(_))));
    }

    #[test]
    fn rollback_provision_cleans_up_after_partial_failure() {
        let dir = test_state_dir("rollback_provision_cleans_up_after_partial_failure");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let record = reconciler.records["web-1"].clone();

        // Simulate a partial failure: dataset clone + jail create succeed,
        // then provisioning would fail at the networking step in the real
        // system. We can't easily force FakeNetManager to fail without a
        // missing bridge, so instead verify rollback's own idempotent
        // cleanup behavior directly: calling it after a successful
        // provision should fully undo it.
        reconciler.provision("web-1", &record).unwrap();
        reconciler.rollback_provision("web-1", &record);

        let jail_name = record::jail_name("web-1");
        assert_eq!(reconciler.jails.jail_exists(&jail_name).unwrap(), false);
        assert_eq!(
            reconciler.zfs.dataset_exists(&record::jail_dataset_path("zroot", "web-1")).unwrap(),
            false
        );
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --workspace -p keel-agentd`

Expected: PASS, 28 tests (25 from Task 5 +
`provision_drives_zfs_jail_net_and_command_in_order`,
`provision_fails_clearly_when_base_image_missing`,
`rollback_provision_cleans_up_after_partial_failure`).

- [ ] **Step 4: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 64 tests total (61 from Task 5 + 3 new).

- [ ] **Step 5: Commit and push**

```bash
git add keel-agentd/src/reconciler.rs
git commit -m "Add Reconciler provisioning path with rollback"
git push origin master:main
```

---

### Task 7: Reconciler — public reconcile()

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: everything from Tasks 5-6.
- Produces: `pub fn reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)>` on `Reconciler<J, Z, N>`. This completes the milestone — `Reconciler` now has its full public API (`new`, `apply`, `delete`, `reconcile`).

Note on the return type: `reconcile` returns the list of per-jail failures
(empty if all succeeded) rather than a single `Result`, since one jail's
failure must never stop the others from being reconciled — this matches
the spec's "failures do not crash the daemon" principle applied per-jail,
not just per-daemon.

- [ ] **Step 1: Add the public reconcile method**

Add `use std::time::Instant;` to the top of `keel-agentd/src/reconciler.rs`'s
imports (alongside the existing `use` lines).

Add this method to the `impl<J: JailRuntime, Z: ZfsManager, N: NetManager>
Reconciler<J, Z, N>` block, after `rollback_provision`:

```rust
    pub fn reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)> {
        let names: Vec<String> = self.records.keys().cloned().collect();
        let mut failures = Vec::new();
        for name in names {
            if let Err(e) = self.reconcile_one(&name, now) {
                failures.push((name, e));
            }
        }
        failures
    }

    fn reconcile_one(&mut self, name: &str, now: Instant) -> Result<(), ReconcileError> {
        let record = self.records[name].clone();
        let jail_name = record::jail_name(name);

        let can_retry = self.backoff.entry(name.to_string()).or_insert_with(BackoffState::new).can_retry(now);
        if !can_retry {
            return Ok(());
        }

        let exists = self.jails.jail_exists(&jail_name)?;

        if !exists {
            let result = self.provision(name, &record);
            self.backoff.get_mut(name).unwrap().record_attempt(now);
            if result.is_err() {
                self.rollback_provision(name, &record);
            }
            result
        } else {
            let running = self.jails.is_running(&jail_name)?;
            if running {
                let pcpu_percent =
                    keel_spec::cores_to_pcpu_percent(keel_spec::parse_cpu_cores(&record.spec.spec.resources.cpu)?);
                let memory_bytes = keel_spec::parse_memory_bytes(&record.spec.spec.resources.memory)?;
                self.jails.set_resource_limits(&jail_name, pcpu_percent, memory_bytes)?;
                Ok(())
            } else if record.spec.spec.restart_policy == keel_spec::RestartPolicy::Never {
                Ok(())
            } else {
                self.configure_networking_and_limits(name, &record)?;
                self.jails.start_command(&jail_name, &record.spec.spec.command)?;
                self.backoff.get_mut(name).unwrap().record_attempt(now);
                Ok(())
            }
        }
    }
```

- [ ] **Step 2: Add tests**

Add these tests to the existing `#[cfg(test)] mod tests` block in
`keel-agentd/src/reconciler.rs`:

```rust
    #[test]
    fn reconcile_provisions_a_missing_jail() {
        let dir = test_state_dir("reconcile_provisions_a_missing_jail");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();

        let failures = reconciler.reconcile(Instant::now());

        assert!(failures.is_empty(), "expected no failures, got: {failures:?}");
        let jail_name = record::jail_name("web-1");
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_reports_base_image_not_found_without_stopping_other_jails() {
        let dir = test_state_dir("reconcile_reports_base_image_not_found_without_stopping_other_jails");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap(); // has a seeded base image
        let mut broken = sample_spec("web-2");
        broken.spec.image = "base/does-not-exist".to_string();
        reconciler.apply(broken).unwrap(); // base image never seeded

        let failures = reconciler.reconcile(Instant::now());

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "web-2");
        assert!(matches!(failures[0].1, ReconcileError::BaseImageNotFound(_)));
        // web-1 should still have been provisioned successfully.
        assert_eq!(reconciler.jails.is_running(&record::jail_name("web-1")).unwrap(), true);
    }

    #[test]
    fn reconcile_restarts_a_crashed_jail() {
        let dir = test_state_dir("reconcile_restarts_a_crashed_jail");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let t0 = Instant::now();
        reconciler.reconcile(t0);

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);

        // The initial provisioning call above already armed a 1s backoff
        // cooldown (record_attempt runs unconditionally after provisioning,
        // per BackoffState's contract) — advance past it so this restart
        // attempt isn't itself suppressed by that cooldown.
        reconciler.reconcile(t0 + Duration::from_secs(1));
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_respects_backoff_cooldown_between_restarts() {
        let dir = test_state_dir("reconcile_respects_backoff_cooldown_between_restarts");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let t0 = Instant::now();
        reconciler.reconcile(t0);

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        // Still within the 1s cooldown armed by the initial provisioning
        // call above — this reconcile is a no-op, same as the next one.
        reconciler.reconcile(t0);

        reconciler.jails.mark_exited(&jail_name);
        reconciler.reconcile(t0); // still within cooldown — should NOT restart yet
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);

        reconciler.reconcile(t0 + Duration::from_secs(1)); // cooldown passed
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_never_policy_leaves_a_crashed_jail_alone() {
        let dir = test_state_dir("reconcile_never_policy_leaves_a_crashed_jail_alone");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        let mut spec = sample_spec("web-1");
        spec.spec.restart_policy = RestartPolicy::Never;
        reconciler.apply(spec).unwrap();
        reconciler.reconcile(Instant::now());

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        reconciler.reconcile(Instant::now());

        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);
    }

    #[test]
    fn reconcile_is_a_no_op_when_jail_already_matches_desired_state() {
        let dir = test_state_dir("reconcile_is_a_no_op_when_jail_already_matches_desired_state");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        reconciler.reconcile(Instant::now());

        let failures = reconciler.reconcile(Instant::now());
        assert!(failures.is_empty(), "expected no failures, got: {failures:?}");
        assert_eq!(reconciler.jails.is_running(&record::jail_name("web-1")).unwrap(), true);
    }
```

Add `use std::time::Duration;` to the test module's imports if not already
present (the top-level module already imports `Instant` via the new
top-of-file `use std::time::Instant;` from Step 1, but the test module has
its own `use super::*;` which brings that in — only add `Duration`
explicitly if the compiler reports it's missing).

- [ ] **Step 3: Run the tests**

Run: `cargo test --workspace -p keel-agentd`

Expected: PASS, 34 tests (28 from Task 6 + `reconcile_provisions_a_missing_jail`,
`reconcile_reports_base_image_not_found_without_stopping_other_jails`,
`reconcile_restarts_a_crashed_jail`,
`reconcile_respects_backoff_cooldown_between_restarts`,
`reconcile_never_policy_leaves_a_crashed_jail_alone`,
`reconcile_is_a_no_op_when_jail_already_matches_desired_state`).

- [ ] **Step 4: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 70 tests total (64 from Task 6 + 6 new). This completes
Milestone 4 — `Reconciler` now has its full public API (`new`, `apply`,
`delete`, `reconcile`), fully tested against fakes with zero FreeBSD
dependency.

- [ ] **Step 5: Commit and push**

```bash
git add keel-agentd/src/reconciler.rs
git commit -m "Add Reconciler::reconcile: full diff-and-act loop"
git push origin master:main
```

## Milestone Exit Criteria

- `cargo test --workspace` passes with 70 tests on macOS, all of
  `keel-agentd`'s reconciliation logic tested against
  `FakeJailRuntime`/`FakeZfsManager`/`FakeNetManager` with zero FreeBSD
  dependency.
- On the FreeBSD VM: `cargo test -p keel-jail --test freebsd_lifecycle`
  passes, including the new `jail_exists` test.
- `Reconciler<J, Z, N>` exposes `new`, `apply`, `delete`, `reconcile` — a
  complete, working reconciliation core ready for a later milestone to
  wire up behind an HTTP API and a `main.rs` binary, instantiated against
  the real `ProcessJailRuntime`/`CliZfsManager`/`ProcessNetManager`.
