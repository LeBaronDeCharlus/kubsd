# keel-agentd Milestone 5: Local HTTP API + `keelctl` CLI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 8:** its verification step needs the real FreeBSD
> VM (`root@192.168.64.2`). The coordinating session has direct SSH access
> to this VM. **Every other task (1-7) is pure Rust tested against fakes,
> or a plain `cargo build` check, and needs no FreeBSD VM interaction.**

**Goal:** Build the `keel-agentd` binary (wiring the real `ProcessJailRuntime`/
`CliZfsManager`/`ProcessNetManager` implementations into a `Reconciler`,
driven by a 5-second timer), a local HTTP API over a Unix socket to
`apply`/`get`/`delete` jail specs, and a `keelctl` CLI that talks to it.

**Architecture:** A single worker thread owns the `Reconciler<J, Z, N>` and
processes `Command`s (`Apply`/`Get`/`Delete`/`Tick`) from an `mpsc` channel —
this is the only thing ever allowed to touch the `Reconciler`. A timer
thread and per-connection HTTP handler threads (spawned from a blocking
`UnixListener` accept loop) send into that channel and block on a reply
channel. `keelctl` hand-rolls an HTTP/1.1 request/response over a
`UnixStream`, parsed with `httparse`. Every exact design decision below
(wire protocol, status codes, concurrency model) was worked out and
approved in the design spec — this plan translates it directly into code.

**Tech Stack:** Rust (2021 edition), `serde`/`serde_yaml` (already a
dependency, reused for the wire format — no JSON), `httparse` (new
dependency, small HTTP/1.1 parser, no transitive deps), `std::sync::mpsc`
and `std::thread` for concurrency — no async runtime.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-09-keel-agent-milestone5-http-cli-design.md` (Approved). The wire protocol, status code mapping, concurrency model, and CLI surface there must match exactly, except for the two refinements below (discovered while translating the spec into concrete types — noted here rather than silently diverging).
- **Refinement 1:** the design spec describes `http.rs` as "generic over the same `J: JailRuntime, Z: ZfsManager, N: NetManager` bounds as `Reconciler`." In practice this isn't needed: `worker::Command` (`Apply`/`Get`/`Delete`/`Tick`) is already fully monomorphic (it carries `JailSpec`, `Vec<JailStatus>`, `ReconcileError` — never `J`/`Z`/`N` themselves). Only `worker::spawn` is generic; `http.rs` and `keelctl` operate purely on `Sender<Command>` and the wire types, with no generic parameters of their own. Tests still achieve the spec's fakes-first goal (build a `Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager>`, call `worker::spawn` to get a `Sender<Command>`, then run `http::run` against it) — same effect, simpler code.
- **Refinement 2:** `Reconciler::get`/`list` take an explicit `now: Instant` parameter (the spec's pseudocode omitted it), matching `reconcile`'s existing convention of taking `now` explicitly so tests can simulate time passing without real sleeps.
- Status code mapping (exact): `ReconcileError::InvalidSpec(keel_spec::SpecError::ImmutableField(_))` → `409`; any other `InvalidSpec(_)` → `400`; `NotFound(_)` → `404`; `Store`/`Jail`/`Zfs`/`Net` → `500`.
- Wire bodies are YAML (`serde_yaml`, already a dependency) — no JSON, no format conversion.
- Config is CLI flags with hardcoded defaults: `--pool zroot`, `--state-dir /var/db/keel`, `--socket /var/run/keel-agentd.sock` — no config file.
- No `rc.d` script, no daemonization, no TLS/auth beyond socket permissions (`0600`), no logging framework — all deferred to Milestone 6 per the design spec's Non-Goals.
- No placeholders: every new function has a passing test, except `main.rs` (Task 6), which is verified by `cargo build --workspace` on macOS and manually on the FreeBSD VM (Task 8) — it has no meaningful unit test of its own, it's pure wiring.

---

### Task 1: Wire types (`wire.rs`)

**Files:**
- Create: `keel-agentd/src/wire.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `keel_agentd::record::JailRecord` (existing).
- Produces: `wire::JailStatus { record: JailRecord, running: bool, backoff: BackoffStatus }`, `wire::BackoffStatus { retry_in_secs: Option<u64>, current_delay_secs: Option<u64> }` (implements `Default`), `wire::ErrorBody { error: String }`. All `Serialize + Deserialize + Debug + Clone + PartialEq`. Re-exported as `keel_agentd::{JailStatus, BackoffStatus, ErrorBody}`. Task 2 adds the `BackoffState::status` method that produces a `BackoffStatus`; Task 3 adds `Reconciler::get`/`list` that produce `JailStatus`; Task 5's `http.rs` and Task 7's `keelctl` consume all three for (de)serialization.

- [ ] **Step 1: Write the failing test**

Create `keel-agentd/src/wire.rs`:

```rust
use crate::record::JailRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailStatus {
    pub record: JailRecord,
    pub running: bool,
    pub backoff: BackoffStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackoffStatus {
    pub retry_in_secs: Option<u64>,
    pub current_delay_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{JailSpec, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};

    fn sample_record() -> JailRecord {
        JailRecord {
            spec: JailSpec {
                api_version: "keel/v1".to_string(),
                kind: "Jail".to_string(),
                metadata: Metadata { name: "web-1".to_string() },
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
            },
            epair_ordinal: 1,
        }
    }

    #[test]
    fn jail_status_round_trips_through_yaml() {
        let status = JailStatus {
            record: sample_record(),
            running: true,
            backoff: BackoffStatus { retry_in_secs: Some(4), current_delay_secs: Some(8) },
        };
        let yaml = serde_yaml::to_string(&status).unwrap();
        let parsed: JailStatus = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn backoff_status_default_is_no_cooldown() {
        let status = BackoffStatus::default();
        assert_eq!(status.retry_in_secs, None);
        assert_eq!(status.current_delay_secs, None);
    }

    #[test]
    fn error_body_round_trips_through_yaml() {
        let body = ErrorBody { error: "jail 'web-1' not found in desired state".to_string() };
        let yaml = serde_yaml::to_string(&body).unwrap();
        let parsed: ErrorBody = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, body);
    }
}
```

- [ ] **Step 2: Register the module**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod backoff;
pub mod record;
pub mod reconciler;
pub mod store;
pub mod wire;

pub use record::JailRecord;
pub use reconciler::{ReconcileError, Reconciler};
pub use wire::{BackoffStatus, ErrorBody, JailStatus};
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p keel-agentd wire::`
Expected: PASS, 3 tests (`jail_status_round_trips_through_yaml`,
`backoff_status_default_is_no_cooldown`, `error_body_round_trips_through_yaml`).

- [ ] **Step 4: Commit**

```bash
git add keel-agentd/src/wire.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd wire types: JailStatus, BackoffStatus, ErrorBody"
```

---

### Task 2: `BackoffState::status` accessor

**Files:**
- Modify: `keel-agentd/src/backoff.rs`

**Interfaces:**
- Consumes: `wire::BackoffStatus` (Task 1).
- Produces: `BackoffState::status(&self, now: Instant) -> BackoffStatus`. Task 3's `Reconciler::get`/`list` are the first callers.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/backoff.rs`'s `#[cfg(test)] mod tests` block (after the existing tests, before the closing `}`):

```rust
    #[test]
    fn status_reports_no_cooldown_for_a_fresh_state() {
        let state = BackoffState::new();
        let status = state.status(Instant::now());
        assert_eq!(status.retry_in_secs, None);
        assert_eq!(status.current_delay_secs, None);
    }

    #[test]
    fn status_reports_retry_in_secs_and_current_delay_after_an_attempt() {
        let mut state = BackoffState::new();
        let t0 = Instant::now();
        state.record_attempt(t0); // next_retry_at = t0 + 1s, current_delay becomes 2s

        let status = state.status(t0);
        assert_eq!(status.retry_in_secs, Some(1));
        assert_eq!(status.current_delay_secs, Some(2));

        let later = state.status(t0 + Duration::from_millis(500));
        assert_eq!(later.retry_in_secs, Some(0), "500ms remaining rounds down to 0 whole seconds");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd backoff::tests::status_ -v`
Expected: FAIL with "no method named `status` found for struct `BackoffState`"

- [ ] **Step 3: Implement `status`**

Add to `keel-agentd/src/backoff.rs`, at the top:

```rust
use crate::wire::BackoffStatus;
```

Add to the `impl BackoffState` block, after `record_attempt`:

```rust
    /// Read-only snapshot for the HTTP API's `get`/`list` — reports the
    /// delay in whole seconds relative to `now`, not an absolute timestamp
    /// (an `Instant` has no wall-clock meaning to report as one).
    pub fn status(&self, now: Instant) -> BackoffStatus {
        match self.next_retry_at {
            Some(next) => BackoffStatus {
                retry_in_secs: Some(next.saturating_duration_since(now).as_secs()),
                current_delay_secs: Some(self.current_delay.as_secs()),
            },
            None => BackoffStatus::default(),
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd backoff::`
Expected: PASS, 7 tests (5 existing + the 2 new `status_*` tests).

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/backoff.rs
git commit -m "Add BackoffState::status read accessor"
```

---

### Task 3: `Reconciler::get`/`list` accessors

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: `wire::JailStatus` (Task 1), `BackoffState::status` (Task 2).
- Produces: `Reconciler::get(&self, name: &str, now: Instant) -> Option<JailStatus>`, `Reconciler::list(&self, now: Instant) -> Vec<JailStatus>` (sorted by name). Task 4's `worker.rs` is the first caller.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/reconciler.rs`'s `#[cfg(test)] mod tests` block, after `reconcile_is_a_no_op_when_jail_already_matches_desired_state`:

```rust
    #[test]
    fn get_returns_none_for_an_unknown_name() {
        let dir = test_state_dir("get_returns_none_for_an_unknown_name");
        let reconciler = new_reconciler(dir);
        assert!(reconciler.get("missing", Instant::now()).is_none());
    }

    #[test]
    fn get_reports_spec_running_state_and_backoff_after_provisioning() {
        let dir = test_state_dir("get_reports_spec_running_state_and_backoff_after_provisioning");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/keel/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let t0 = Instant::now();
        reconciler.reconcile(t0);

        let status = reconciler.get("web-1", t0).unwrap();
        assert_eq!(status.record.spec.metadata.name, "web-1");
        assert!(status.running);
        assert_eq!(status.backoff.retry_in_secs, Some(1));
    }

    #[test]
    fn list_returns_all_records_sorted_by_name() {
        let dir = test_state_dir("list_returns_all_records_sorted_by_name");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-2")).unwrap();
        reconciler.apply(sample_spec("web-1")).unwrap();

        let statuses = reconciler.list(Instant::now());
        let names: Vec<&str> = statuses.iter().map(|s| s.record.spec.metadata.name.as_str()).collect();
        assert_eq!(names, vec!["web-1", "web-2"]);
    }

    #[test]
    fn list_is_empty_when_no_specs_have_been_applied() {
        let dir = test_state_dir("list_is_empty_when_no_specs_have_been_applied");
        let reconciler = new_reconciler(dir);
        assert!(reconciler.list(Instant::now()).is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd reconciler::tests::get_ reconciler::tests::list_ -v`
Expected: FAIL with "no method named `get`/`list` found for struct `Reconciler<...>`"

- [ ] **Step 3: Implement `get`/`list`**

Add to `keel-agentd/src/reconciler.rs`, at the top:

```rust
use crate::wire::JailStatus;
```

Add to the `impl<J: JailRuntime, Z: ZfsManager, N: NetManager> Reconciler<J, Z, N>` block, after `reconcile_one` (as new public methods, before the closing `}` of the impl block):

```rust
    pub fn get(&self, name: &str, now: Instant) -> Option<JailStatus> {
        let record = self.records.get(name)?.clone();
        let jail_name = record::jail_name(name);
        // A transient runtime query error is treated as "not confirmed
        // running" rather than failing the whole status read — the spec
        // and backoff info below are still valid and useful on their own.
        let running = self.jails.is_running(&jail_name).unwrap_or(false);
        let backoff = self.backoff.get(name).map(|b| b.status(now)).unwrap_or_default();
        Some(JailStatus { record, running, backoff })
    }

    pub fn list(&self, now: Instant) -> Vec<JailStatus> {
        let mut names: Vec<&String> = self.records.keys().collect();
        names.sort();
        names.iter().filter_map(|name| self.get(name, now)).collect()
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd reconciler::`
Expected: PASS, 23 tests (19 existing + 4 new).

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/reconciler.rs
git commit -m "Add Reconciler::get and Reconciler::list read accessors"
```

---

### Task 4: `worker.rs` — the reconciler-owning thread

**Files:**
- Create: `keel-agentd/src/worker.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `Reconciler<J, Z, N>` (existing), `wire::JailStatus` (Task 1).
- Produces: `worker::Command` enum (`Apply(JailSpec, Sender<Result<(), ReconcileError>>)`, `Get(Option<String>, Sender<Vec<JailStatus>>)`, `Delete(String, Sender<Result<(), ReconcileError>>)`, `Tick`), `worker::spawn<J, Z, N>(reconciler: Reconciler<J, Z, N>) -> (JoinHandle<()>, Sender<Command>)` where `J: JailRuntime + Send + 'static`, `Z: ZfsManager + Send + 'static`, `N: NetManager + Send + 'static`. Task 5's `http.rs` and Task 6's `main.rs` are the first callers.

- [ ] **Step 1: Write the failing tests**

Create `keel-agentd/src/worker.rs`:

```rust
use crate::reconciler::{ReconcileError, Reconciler};
use crate::wire::JailStatus;
use keel_jail::JailRuntime;
use keel_net::NetManager;
use keel_spec::JailSpec;
use keel_zfs::ZfsManager;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub enum Command {
    Apply(JailSpec, Sender<Result<(), ReconcileError>>),
    Get(Option<String>, Sender<Vec<JailStatus>>),
    Delete(String, Sender<Result<(), ReconcileError>>),
    Tick,
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_jail::FakeJailRuntime;
    use keel_net::FakeNetManager;
    use keel_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
    use keel_zfs::FakeZfsManager;
    use std::path::PathBuf;

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
        let dir = std::env::temp_dir().join(format!("keel-agentd-worker-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn spawn_test_worker(name: &str) -> Sender<Command> {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(
            FakeJailRuntime::new(),
            zfs,
            FakeNetManager::new(),
            "zroot".to_string(),
            test_state_dir(name),
        )
        .unwrap();
        let (_handle, commands) = spawn(reconciler);
        commands
    }

    #[test]
    fn apply_command_persists_and_reconciles_immediately() {
        let commands = spawn_test_worker("apply_command_persists_and_reconciles_immediately");

        let (reply_tx, reply_rx) = mpsc::channel();
        commands.send(Command::Apply(sample_spec("web-1"), reply_tx)).unwrap();
        assert!(reply_rx.recv().unwrap().is_ok());

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(Some("web-1".to_string()), get_tx)).unwrap();
        let statuses = get_rx.recv().unwrap();
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].running, "expected apply to trigger an immediate reconcile that provisions the jail");
    }

    #[test]
    fn invalid_apply_command_returns_an_error_without_crashing_the_worker() {
        let commands = spawn_test_worker("invalid_apply_command_returns_an_error_without_crashing_the_worker");
        let mut invalid = sample_spec("web-1");
        invalid.metadata.name = "Invalid_Name".to_string();

        let (reply_tx, reply_rx) = mpsc::channel();
        commands.send(Command::Apply(invalid, reply_tx)).unwrap();
        assert!(matches!(reply_rx.recv().unwrap(), Err(ReconcileError::InvalidSpec(_))));

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(None, get_tx)).unwrap();
        assert!(get_rx.recv().unwrap().is_empty());
    }

    #[test]
    fn delete_command_removes_the_record() {
        let commands = spawn_test_worker("delete_command_removes_the_record");

        let (apply_tx, apply_rx) = mpsc::channel();
        commands.send(Command::Apply(sample_spec("web-1"), apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();

        let (delete_tx, delete_rx) = mpsc::channel();
        commands.send(Command::Delete("web-1".to_string(), delete_tx)).unwrap();
        assert!(delete_rx.recv().unwrap().is_ok());

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(None, get_tx)).unwrap();
        assert!(get_rx.recv().unwrap().is_empty());
    }

    #[test]
    fn delete_command_on_unknown_name_returns_not_found() {
        let commands = spawn_test_worker("delete_command_on_unknown_name_returns_not_found");
        let (delete_tx, delete_rx) = mpsc::channel();
        commands.send(Command::Delete("missing".to_string(), delete_tx)).unwrap();
        assert!(matches!(delete_rx.recv().unwrap(), Err(ReconcileError::NotFound(_))));
    }

    #[test]
    fn tick_command_is_processed_without_blocking_subsequent_commands() {
        let commands = spawn_test_worker("tick_command_is_processed_without_blocking_subsequent_commands");
        commands.send(Command::Tick).unwrap();

        // mpsc is FIFO: this Get is only answered once Tick has already
        // been processed, proving Tick doesn't hang or crash the worker.
        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(None, get_tx)).unwrap();
        assert!(get_rx.recv().unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd worker:: -v`
Expected: FAIL to compile with "cannot find function `spawn` in module `worker`" (or similar — the `Command` enum exists but nothing processes it yet).

- [ ] **Step 3: Implement `spawn` and command handling**

Add to `keel-agentd/src/worker.rs`, after the `Command` enum definition (before `#[cfg(test)]`):

```rust
pub fn spawn<J, Z, N>(mut reconciler: Reconciler<J, Z, N>) -> (JoinHandle<()>, Sender<Command>)
where
    J: JailRuntime + Send + 'static,
    Z: ZfsManager + Send + 'static,
    N: NetManager + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut reconciler, command);
        }
    });
    (handle, tx)
}

fn handle_command<J: JailRuntime, Z: ZfsManager, N: NetManager>(
    reconciler: &mut Reconciler<J, Z, N>,
    command: Command,
) {
    match command {
        Command::Apply(spec, reply) => {
            let result = reconciler.apply(spec);
            // Reconcile immediately so a client's apply/delete call
            // observes its effects by the time it gets a response,
            // rather than waiting for the next timer tick. The resulting
            // per-jail failures (if any) are already surfaced via each
            // jail's backoff status on a later `get`, so they're
            // discarded here — same treatment as a plain `Tick`.
            let _ = reconciler.reconcile(Instant::now());
            let _ = reply.send(result);
        }
        Command::Delete(name, reply) => {
            let result = reconciler.delete(&name);
            let _ = reconciler.reconcile(Instant::now());
            let _ = reply.send(result);
        }
        Command::Get(name, reply) => {
            let now = Instant::now();
            let statuses = match name {
                Some(n) => reconciler.get(&n, now).into_iter().collect(),
                None => reconciler.list(now),
            };
            let _ = reply.send(statuses);
        }
        Command::Tick => {
            let _ = reconciler.reconcile(Instant::now());
        }
    }
}
```

- [ ] **Step 4: Register the module**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod backoff;
pub mod record;
pub mod reconciler;
pub mod store;
pub mod wire;
pub mod worker;

pub use record::JailRecord;
pub use reconciler::{ReconcileError, Reconciler};
pub use wire::{BackoffStatus, ErrorBody, JailStatus};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd worker::`
Expected: PASS, 5 tests.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/worker.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd worker: single-threaded Reconciler owner driven by a Command channel"
```

---

### Task 5: `http.rs` — HTTP/1.1 server over a Unix socket

**Files:**
- Create: `keel-agentd/src/http.rs`
- Modify: `keel-agentd/src/lib.rs`
- Modify: `keel-agentd/Cargo.toml`

**Interfaces:**
- Consumes: `worker::Command` (Task 4), `wire::{JailStatus, ErrorBody}` (Task 1), `reconciler::ReconcileError`.
- Produces: `http::run(listener: UnixListener, commands: Sender<Command>)` — blocking accept loop, spawns one thread per connection, never returns. Task 6's `main.rs` and Task 7's `keelctl` integration tests are the first callers.

- [ ] **Step 1: Add the `httparse` dependency**

Modify `keel-agentd/Cargo.toml`:

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
httparse = "1"
```

- [ ] **Step 2: Write the failing tests**

Create `keel-agentd/src/http.rs`:

```rust
use crate::reconciler::ReconcileError;
use crate::wire::ErrorBody;
use crate::worker::Command;
use keel_spec::JailSpec;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender};
use std::thread;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

pub fn run(listener: UnixListener, commands: Sender<Command>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        thread::spawn(move || {
            let _ = handle_connection(stream, &commands);
        });
    }
}

struct ParsedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle_connection(mut stream: UnixStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response(&mut stream, status, &body)
}

fn read_request(stream: &mut UnixStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length) = loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let content_length = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_MESSAGE_BYTES {
                    return Ok(None);
                }
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Ok(None),
        }
    };

    let total_len = header_len + content_length;
    while buf.len() < total_len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_len..total_len].to_vec();
    Ok(Some(ParsedRequest { method, path, body }))
}

fn write_response(stream: &mut UnixStream, status: u16, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("PUT", ["jails", name]) => handle_apply(name, &request.body, commands),
        ("GET", ["jails"]) => handle_get(None, commands),
        ("GET", ["jails", name]) => handle_get(Some(name.to_string()), commands),
        ("DELETE", ["jails", name]) => handle_delete(name, commands),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}

fn handle_apply(path_name: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let spec: JailSpec = match serde_yaml::from_slice(body) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    if spec.metadata.name != path_name {
        return error_response(
            400,
            format!("path name '{path_name}' does not match spec.metadata.name '{}'", spec.metadata.name),
        );
    }

    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Apply(spec, reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}

fn handle_get(name: Option<String>, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Get(name.clone(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    let statuses = match reply_rx.recv() {
        Ok(s) => s,
        Err(_) => return error_response(500, "reconciler worker did not respond".to_string()),
    };
    match name {
        Some(n) => match statuses.into_iter().next() {
            Some(status) => yaml_response(200, &status),
            None => error_response(404, format!("jail '{n}' not found")),
        },
        None => yaml_response(200, &statuses),
    }
}

fn handle_delete(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Delete(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}

fn status_for_error(error: &ReconcileError) -> u16 {
    match error {
        ReconcileError::InvalidSpec(keel_spec::SpecError::ImmutableField(_)) => 409,
        ReconcileError::InvalidSpec(_) => 400,
        ReconcileError::NotFound(_) => 404,
        _ => 500,
    }
}

fn error_response(status: u16, message: String) -> (u16, Vec<u8>) {
    let body = serde_yaml::to_string(&ErrorBody { error: message })
        .expect("ErrorBody serialization should not fail");
    (status, body.into_bytes())
}

fn yaml_response<T: serde::Serialize>(status: u16, value: &T) -> (u16, Vec<u8>) {
    let body = serde_yaml::to_string(value).expect("wire type serialization should not fail");
    (status, body.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconciler::Reconciler;
    use crate::worker;
    use keel_jail::FakeJailRuntime;
    use keel_net::FakeNetManager;
    use keel_zfs::FakeZfsManager;
    use std::path::PathBuf;

    fn sample_spec_yaml(name: &str) -> String {
        format!(
            "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: {name}\nspec:\n  image: base/14.2-web\n  command: [\"/usr/local/bin/myapp\"]\n  network:\n    vnet: true\n    bridge: keel0\n    address: 10.0.0.5/24\n  resources:\n    cpu: \"2\"\n    memory: 512M\n  restartPolicy: Always\n"
        )
    }

    fn start_test_server(name: &str) -> PathBuf {
        let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-test-state-{name}"));
        let _ = std::fs::remove_dir_all(&state_dir);
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(
            FakeJailRuntime::new(),
            zfs,
            FakeNetManager::new(),
            "zroot".to_string(),
            state_dir,
        )
        .unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let socket_path = std::env::temp_dir().join(format!("keel-agentd-http-test-{name}.sock"));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();
        thread::spawn(move || run(listener, commands));
        socket_path
    }

    fn send_request(socket_path: &PathBuf, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = UnixStream::connect(socket_path).unwrap();
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();

        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut parsed = httparse::Response::new(&mut headers);
        let header_len = match parsed.parse(&response).unwrap() {
            httparse::Status::Complete(len) => len,
            httparse::Status::Partial => panic!("incomplete response: {response:?}"),
        };
        let status = parsed.code.unwrap();
        let body = String::from_utf8(response[header_len..].to_vec()).unwrap();
        (status, body)
    }

    #[test]
    fn put_valid_spec_returns_200_and_provisions_the_jail() {
        let socket_path = start_test_server("put_valid_spec_returns_200_and_provisions_the_jail");
        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        assert_eq!(status, 200);

        let (status, body) = send_request(&socket_path, "GET", "/jails/web-1", "");
        assert_eq!(status, 200);
        assert!(body.contains("running: true"), "expected running: true in body: {body}");
    }

    #[test]
    fn put_with_mismatched_path_and_body_name_returns_400() {
        let socket_path = start_test_server("put_with_mismatched_path_and_body_name_returns_400");
        let (status, body) = send_request(&socket_path, "PUT", "/jails/other-name", &sample_spec_yaml("web-1"));
        assert_eq!(status, 400);
        assert!(body.contains("does not match"));
    }

    #[test]
    fn put_changing_an_immutable_field_returns_409() {
        let socket_path = start_test_server("put_changing_an_immutable_field_returns_409");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));

        let changed_yaml = sample_spec_yaml("web-1").replace("base/14.2-web", "base/different-image");
        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1", &changed_yaml);
        assert_eq!(status, 409);
    }

    #[test]
    fn get_on_unknown_name_returns_404() {
        let socket_path = start_test_server("get_on_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "GET", "/jails/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn delete_on_unknown_name_returns_404() {
        let socket_path = start_test_server("delete_on_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "DELETE", "/jails/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn delete_removes_a_provisioned_jail() {
        let socket_path = start_test_server("delete_removes_a_provisioned_jail");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        let (status, _) = send_request(&socket_path, "DELETE", "/jails/web-1", "");
        assert_eq!(status, 200);

        let (status, _) = send_request(&socket_path, "GET", "/jails/web-1", "");
        assert_eq!(status, 404, "deleted jail should no longer be found");
    }

    #[test]
    fn get_jails_lists_all_applied_jails() {
        let socket_path = start_test_server("get_jails_lists_all_applied_jails");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        send_request(&socket_path, "PUT", "/jails/web-2", &sample_spec_yaml("web-2"));

        let (status, body) = send_request(&socket_path, "GET", "/jails", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-1"));
        assert!(body.contains("web-2"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd http:: -v`
Expected: FAIL to compile with "unresolved import `crate::http`" (module not registered yet in `lib.rs`).

- [ ] **Step 4: Register the module**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod backoff;
pub mod http;
pub mod record;
pub mod reconciler;
pub mod store;
pub mod wire;
pub mod worker;

pub use record::JailRecord;
pub use reconciler::{ReconcileError, Reconciler};
pub use wire::{BackoffStatus, ErrorBody, JailStatus};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd http::`
Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/Cargo.toml keel-agentd/src/http.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd HTTP server: routing, YAML wire format, status code mapping"
```

---

### Task 6: `keel-agentd` binary (`main.rs`)

**Files:**
- Create: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_agentd::worker::{spawn, Command}`, `keel_agentd::http::run`, `keel_agentd::Reconciler`, `keel_jail::ProcessJailRuntime`, `keel_zfs::CliZfsManager`, `keel_net::ProcessNetManager` (all existing).
- Produces: the `keel-agentd` executable. Cargo auto-detects a binary target named after the package from `src/main.rs` alongside the existing `src/lib.rs` — no `Cargo.toml` change needed. Task 8 (FreeBSD VM) is the only functional verification; this task's own verification is a compile check (see Step 2).

- [ ] **Step 1: Write `main.rs`**

Create `keel-agentd/src/main.rs`:

```rust
use keel_agentd::worker::{self, Command};
use keel_agentd::Reconciler;
use keel_jail::ProcessJailRuntime;
use keel_net::ProcessNetManager;
use keel_zfs::CliZfsManager;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

struct Config {
    pool: String,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pool: "zroot".to_string(),
            state_dir: PathBuf::from("/var/db/keel"),
            socket: PathBuf::from("/var/run/keel-agentd.sock"),
        }
    }
}

fn parse_args() -> Config {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--pool" => config.pool = value,
            "--state-dir" => config.state_dir = PathBuf::from(value),
            "--socket" => config.socket = PathBuf::from(value),
            other => panic!("unknown flag: {other}"),
        }
    }
    config
}

fn main() {
    let config = parse_args();

    let reconciler = Reconciler::new(
        ProcessJailRuntime::new(),
        CliZfsManager::new(),
        ProcessNetManager::new(),
        config.pool,
        config.state_dir,
    )
    .expect("failed to initialize reconciler from on-disk state");

    let (_worker_handle, commands) = worker::spawn(reconciler);

    let timer_commands = commands.clone();
    thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        if timer_commands.send(Command::Tick).is_err() {
            break;
        }
    });

    if config.socket.exists() {
        std::fs::remove_file(&config.socket).expect("failed to remove stale socket file");
    }
    let listener = UnixListener::bind(&config.socket).expect("failed to bind Unix socket");
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))
        .expect("failed to set socket permissions");

    keel_agentd::http::run(listener, commands);
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build --workspace`
Expected: builds successfully (this exercises `main.rs` on macOS even though `ProcessJailRuntime`/`CliZfsManager`/`ProcessNetManager` only function correctly on FreeBSD at runtime — none of them are `cfg`-gated).

- [ ] **Step 3: Commit**

```bash
git add keel-agentd/src/main.rs
git commit -m "Add keel-agentd binary: wires real implementations, timer, and HTTP server"
```

---

### Task 7: `keelctl` CLI crate

**Files:**
- Create: `keelctl/Cargo.toml`
- Create: `keelctl/src/main.rs`
- Create: `keelctl/tests/cli.rs`
- Modify: `Cargo.toml` (workspace root)

**Interfaces:**
- Consumes: `keel_agentd::{ErrorBody, worker, http, Reconciler}` (for the crate itself and its tests), `keel_spec::parse_and_validate`.
- Produces: the `keelctl` executable (`apply -f FILE`, `get [NAME]`, `delete NAME`, all with `--socket PATH`).

- [ ] **Step 1: Create the crate manifest**

Create `keelctl/Cargo.toml`:

```toml
[package]
name = "keelctl"
version = "0.1.0"
edition = "2021"

[dependencies]
keel-agentd = { path = "../keel-agentd" }
keel-spec = { path = "../keel-spec" }
serde_yaml = "0.9"
httparse = "1"

[dev-dependencies]
keel-jail = { path = "../keel-jail" }
keel-zfs = { path = "../keel-zfs" }
keel-net = { path = "../keel-net" }
```

- [ ] **Step 2: Write `main.rs`**

Create `keelctl/src/main.rs`:

```rust
use keel_agentd::ErrorBody;
use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_SOCKET: &str = "/var/run/keel-agentd.sock";

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));

    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "apply" => run_apply(&socket, rest),
        Some((cmd, rest)) if cmd == "get" => run_get(&socket, rest),
        Some((cmd, rest)) if cmd == "delete" => run_delete(&socket, rest),
        _ => {
            eprintln!("usage: keelctl <apply -f FILE|get [name]|delete NAME> [--socket PATH]");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn extract_socket_flag(args: &mut Vec<String>) -> Option<PathBuf> {
    let index = args.iter().position(|a| a == "--socket")?;
    args.remove(index);
    let value = args.remove(index);
    Some(PathBuf::from(value))
}

fn run_apply(socket: &PathBuf, args: &[String]) -> Result<String, String> {
    let index = args.iter().position(|a| a == "-f").ok_or("apply requires -f FILE")?;
    let file = args.get(index + 1).ok_or("apply requires -f FILE")?;
    let yaml = std::fs::read_to_string(file).map_err(|e| format!("failed to read {file}: {e}"))?;
    let spec = keel_spec::parse_and_validate(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
    let path = format!("/jails/{}", spec.metadata.name);
    send_request(socket, "PUT", &path, &yaml).map(|_| String::new())
}

fn run_get(socket: &PathBuf, args: &[String]) -> Result<String, String> {
    let path = match args.first() {
        Some(name) => format!("/jails/{name}"),
        None => "/jails".to_string(),
    };
    send_request(socket, "GET", &path, "")
}

fn run_delete(socket: &PathBuf, args: &[String]) -> Result<String, String> {
    let name = args.first().ok_or("delete requires a jail name")?;
    send_request(socket, "DELETE", &format!("/jails/{name}"), "").map(|_| String::new())
}

fn send_request(socket: &PathBuf, method: &str, path: &str, body: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("failed to connect to {}: {e}", socket.display()))?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(&response).map_err(|e| format!("malformed response: {e}"))? {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Err("incomplete response from server".to_string()),
    };
    let status = parsed.code.unwrap_or(0);
    let response_body = String::from_utf8_lossy(&response[header_len..]).to_string();

    if (200..300).contains(&status) {
        Ok(response_body)
    } else {
        let error: ErrorBody =
            serde_yaml::from_str(&response_body).unwrap_or(ErrorBody { error: response_body });
        Err(error.error)
    }
}
```

- [ ] **Step 3: Write the integration tests**

Create `keelctl/tests/cli.rs`:

```rust
use keel_agentd::{worker, Reconciler};
use keel_jail::FakeJailRuntime;
use keel_net::FakeNetManager;
use keel_zfs::FakeZfsManager;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

fn start_test_server(name: &str) -> PathBuf {
    let state_dir = std::env::temp_dir().join(format!("keelctl-test-state-{name}"));
    let _ = std::fs::remove_dir_all(&state_dir);
    let zfs = FakeZfsManager::new();
    zfs.seed_dataset("zroot/keel/base/14.2-web");
    let reconciler =
        Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), "zroot".to_string(), state_dir)
            .unwrap();
    let (_worker_handle, commands) = worker::spawn(reconciler);

    // A short, non-descriptive filename (not the full test name) — macOS/BSD
    // cap Unix socket paths at ~104 bytes (SUN_LEN), and the default macOS
    // TMPDIR (/var/folders/.../T/) already uses ~50 of them.
    let socket_path = short_unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    thread::spawn(move || keel_agentd::http::run(listener, commands));
    socket_path
}

fn short_unique_socket_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ka-{}-{}.sock", std::process::id(), id))
}

fn write_spec_file(test_name: &str, jail_name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("keelctl-test-spec-{test_name}.yaml"));
    let yaml = format!(
        "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: {jail_name}\nspec:\n  image: base/14.2-web\n  command: [\"/usr/local/bin/myapp\"]\n  network:\n    vnet: true\n    bridge: keel0\n    address: 10.0.0.5/24\n  resources:\n    cpu: \"2\"\n    memory: 512M\n  restartPolicy: Always\n"
    );
    std::fs::write(&path, yaml).unwrap();
    path
}

fn run_keelctl(socket_path: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(args)
        .arg("--socket")
        .arg(socket_path)
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn apply_get_delete_round_trip() {
    let socket_path = start_test_server("apply_get_delete_round_trip");
    let spec_path = write_spec_file("apply_get_delete_round_trip", "web-1");

    let (ok, _, stderr) = run_keelctl(&socket_path, &["apply", "-f", spec_path.to_str().unwrap()]);
    assert!(ok, "apply failed: {stderr}");

    let (ok, stdout, stderr) = run_keelctl(&socket_path, &["get", "web-1"]);
    assert!(ok, "get failed: {stderr}");
    assert!(stdout.contains("running: true"), "expected running: true, got: {stdout}");

    let (ok, _, stderr) = run_keelctl(&socket_path, &["delete", "web-1"]);
    assert!(ok, "delete failed: {stderr}");

    let (ok, _, stderr) = run_keelctl(&socket_path, &["get", "web-1"]);
    assert!(!ok, "expected get on a deleted jail to fail");
    assert!(stderr.contains("not found"), "expected 'not found' in stderr, got: {stderr}");
}

#[test]
fn apply_rejects_a_file_with_an_invalid_spec() {
    let socket_path = start_test_server("apply_rejects_a_file_with_an_invalid_spec");
    let path = std::env::temp_dir().join("keelctl-test-invalid-spec.yaml");
    std::fs::write(&path, "not: valid: yaml: [").unwrap();

    let (ok, _, stderr) = run_keelctl(&socket_path, &["apply", "-f", path.to_str().unwrap()]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn get_lists_multiple_applied_jails() {
    let socket_path = start_test_server("get_lists_multiple_applied_jails");
    run_keelctl(&socket_path, &["apply", "-f", write_spec_file("get_lists_multiple_applied_jails_1", "web-1").to_str().unwrap()]);
    run_keelctl(&socket_path, &["apply", "-f", write_spec_file("get_lists_multiple_applied_jails_2", "web-2").to_str().unwrap()]);

    let (ok, stdout, stderr) = run_keelctl(&socket_path, &["get"]);
    assert!(ok, "get failed: {stderr}");
    assert!(stdout.contains("web-1"));
    assert!(stdout.contains("web-2"));
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p keelctl`
Expected: FAIL with `error: package ID specification 'keelctl' did not match any packages` — the crate exists on disk but isn't a workspace member yet.

- [ ] **Step 5: Register the workspace member**

Modify `Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = ["keel-spec", "keel-jail", "keel-zfs", "keel-net", "keel-agentd", "keelctl"]
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p keelctl`
Expected: PASS, 3 tests.

- [ ] **Step 7: Run the full workspace suite**

Run: `cargo test --workspace`
Expected: PASS, all tests across every crate (prior milestones' tests plus this milestone's 24 new tests: 3 wire + 2 backoff + 4 reconciler + 5 worker + 7 http + 3 keelctl). Zero failures.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml keelctl/
git commit -m "Add keelctl CLI: apply/get/delete over the keel-agentd HTTP API"
```

---

### Task 8: FreeBSD VM manual verification

**Files:** none (verification only, no code changes expected unless the VM surfaces a real bug).

- [ ] **Step 1: Sync the repo on the VM**

```bash
ssh root@192.168.64.2 "cd keel && git pull && cargo build --workspace"
```

Expected: builds successfully, producing `target/debug/keel-agentd` and `target/debug/keelctl`.

- [ ] **Step 2: Prepare a base image dataset (if not already present from earlier milestones)**

```bash
ssh root@192.168.64.2 "zfs list zroot/keel/base/test 2>/dev/null || echo missing"
```

If missing, reuse whatever base-image setup earlier milestones' VM verification already established (Milestones 2-4 required `zroot/keel/base/test` and `zroot/keel/jails` to exist) — do not recreate it if it's already there.

- [ ] **Step 3: Run `keel-agentd` in the foreground**

```bash
ssh root@192.168.64.2 "cd keel && ./target/debug/keel-agentd --pool zroot --state-dir /var/db/keel-test --socket /var/run/keel-agentd-test.sock" &
```

Expected: process starts and blocks (no output means it's running the accept loop). Leave it running for the remaining steps.

- [ ] **Step 4: Verify socket permissions**

```bash
ssh root@192.168.64.2 "stat -f '%Op %Su:%Sg' /var/run/keel-agentd-test.sock"
```

Expected: `100600 root:wheel` (or the FreeBSD `stat` equivalent showing mode `0600`, owner `root`).

- [ ] **Step 5: Apply a real spec via `keelctl`**

Write a spec pointing at the test base image (e.g. `image: base/test`), then:

```bash
ssh root@192.168.64.2 "cd keel && ./target/debug/keelctl --socket /var/run/keel-agentd-test.sock apply -f /tmp/vm-test-spec.yaml"
ssh root@192.168.64.2 "cd keel && ./target/debug/keelctl --socket /var/run/keel-agentd-test.sock get vm-test"
```

Expected: `apply` exits 0; `get` shows `running: true` within the next reconcile cycle (poll `get` a few times up to 5s apart if it shows `running: false` immediately after `apply`, since VM real jail startup isn't instant like the fakes).

- [ ] **Step 6: Confirm the real jail exists**

```bash
ssh root@192.168.64.2 "jls | grep keel-vm-test"
```

Expected: the jail is listed and running.

- [ ] **Step 7: Delete it and confirm teardown**

```bash
ssh root@192.168.64.2 "cd keel && ./target/debug/keelctl --socket /var/run/keel-agentd-test.sock delete vm-test"
ssh root@192.168.64.2 "jls | grep keel-vm-test || echo gone"
```

Expected: `delete` exits 0; the jail no longer appears in `jls`.

- [ ] **Step 8: Stop the test daemon and clean up**

```bash
ssh root@192.168.64.2 "pkill -f 'keel-agentd --pool zroot --state-dir /var/db/keel-test' ; rm -f /var/run/keel-agentd-test.sock ; rm -rf /var/db/keel-test"
```

- [ ] **Step 9: Record the outcome**

If every step above passed with no code changes needed, note in the final commit message or a follow-up commit that Milestone 5 was VM-verified on this date. If the VM surfaced a real bug, fix it on macOS with a regression test added to the relevant task's test module (Tasks 1-7), re-verify with `cargo test --workspace`, then re-run the affected VM steps above before considering the milestone done.

---

## Milestone Exit Criteria

- `cargo test --workspace` passes on macOS with zero FreeBSD dependency for every test except the pre-existing `#![cfg(target_os = "freebsd")]` integration tests from earlier milestones.
- `keel-agentd` is now also a binary: it wires the real `ProcessJailRuntime`/`CliZfsManager`/`ProcessNetManager` implementations into a `Reconciler`, runs a 5-second timer, and serves `apply`/`get`/`delete` over a `0600` Unix socket.
- `keelctl` can `apply -f`, `get [name]`, and `delete <name>` against a running `keel-agentd`, verified both against fakes (macOS, `keelctl/tests/cli.rs`) and against the real implementations (FreeBSD VM, Task 8).
- No `rc.d` integration, no daemonization, no logging framework — all correctly deferred to Milestone 6 per the design spec.
