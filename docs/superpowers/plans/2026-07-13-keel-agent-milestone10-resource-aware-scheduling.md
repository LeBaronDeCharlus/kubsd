# keel Milestone 10: Resource-Aware Bin-Packing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 9:** it needs the real FreeBSD VMs (`root@192.168.64.2`, `.4`, `.5`). The coordinating session has direct SSH access to these VMs and should run this task itself rather than dispatching a subagent for it — this mirrors every prior milestone's real-hardware task. **Tasks 1-8 are pure file edits, verified locally (macOS) via `cargo test`.**

**Goal:** `keel-agentd` detects its own CPU/memory capacity via `sysctl` and reports it plus its currently-committed load (the sum of `spec.resources.{cpu,memory}` across its own tracked jails) to `keel-controlplane`; the scheduler ranks `Alive` nodes by `min(free_cpu/capacity_cpu, free_memory/capacity_memory)` (headroom in the most-constrained resource) instead of jail count.

**Architecture:** A new `keel-agentd/src/capacity.rs` shells out to `sysctl -n hw.ncpu`/`sysctl -n hw.physmem` once at startup. `Reconciler::committed_resources` sums requested resources across tracked `JailRecord`s; a new `worker::Command::CommittedResources` exposes it to the registration thread, which now includes `capacity_cpu`/`capacity_memory` in its registration body and `committed_cpu`/`committed_memory` in every heartbeat body. `keel-controlplane`'s `Registry` stores all four numbers per node; `scheduler::pick_node` is replaced (not extended) with a resource-aware version taking a `NodeResources` slice; `Placements::counts()` is deleted as fully superseded. `keel-controlplane` never deserializes a `JailSpec`, unchanged from Milestones 8-9.

**Tech Stack:** Rust (2021 edition), same dependencies already used throughout (`serde`, `serde_yaml`, `thiserror`, `httparse`) — no new external dependencies. `capacity.rs` shells out to `sysctl`, matching the project's existing `Command::new("zfs")`/`Command::new("jail")` idiom (`keel-zfs/src/cli.rs`), not a new crate.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-13-keel-agent-milestone10-resource-aware-scheduling-design.md` (Approved). The headroom-scoring formula, the "node self-reports, control plane stays opaque to `JailSpec`" split, and every stated Non-Goal must match exactly.
- **One deliberate deviation from the spec's Testing Strategy wording, decided while writing this plan:** the spec describes `capacity::detect`'s test as "FreeBSD-only". It doesn't need to be: `sysctl -n hw.ncpu` and `sysctl -n hw.physmem` both exist with identical semantics on macOS too (verified directly: `sysctl -n hw.ncpu` → `10`, `sysctl -n hw.physmem` → a positive byte count, on the machine this plan was written on). Task 1's test runs unconditionally, on any OS, matching Milestone 1's "no FreeBSD required unless the crate actually touches FreeBSD" precedent — it is not gated behind `#[cfg(target_os = "freebsd")]` like `keel-jail`/`keel-zfs`'s regression tests are, because unlike `jail(8)`/`zfs(8)`, these two specific `sysctl` names are not FreeBSD-specific.
- **No compatibility shim for the old wire formats.** `NodeRegistration`'s old two-field shape and the old empty heartbeat body are gone outright, matching every prior milestone's wire-format changes (e.g. Milestone 8's `--advertise-addr` contract change). Every existing test that sends a registration or heartbeat body without the new fields must be updated in the task that touches that file, not left broken.
- **`Placements::counts()` is deleted, not deprecated.** Its two dedicated tests (`counts_aggregates_multiple_jails_on_the_same_node`, `counts_on_an_empty_table_is_empty`) are deleted with it; the two `.counts()` assertion lines inside `set_again_on_the_same_jail_overwrites_rather_than_duplicating` are dropped, and that test's remaining `.get()` assertion is kept as-is. `Placements::{get, set, remove}` and their existing tests are completely untouched.
- **Every new public type, function, and constant introduced by one task and used by a later task is named exactly as given in that task's Produces list** — later tasks must match these names exactly.
- No placeholders: every task's deliverable is verified with `cargo build -p <crate> && cargo test -p <crate>` before its commit step.
- **Task ordering is a real dependency chain, not just narrative order.** Tasks 3→4→5→6→7 (all `keel-controlplane`) must land in that exact sequence: Task 4 needs Task 3's `NodeStatus` fields, Task 6 needs both Task 4's `Registry` API and Task 5's `scheduler` API, Task 7 needs Task 6's `worker::Command` signatures. Tasks 1-2 (`keel-agentd`, self-contained) have no dependency on 3-7 and could run in either order relative to them, but Task 8 needs both the Task 1-2 pieces AND Task 7's finished `keel-controlplane` (Task 8's own test registers against a real control plane that, after Task 7, requires the new wire fields — so Task 8 must run after Task 7).
- Current baseline entering this milestone (verified directly before writing this plan): `cargo test --workspace` → 161 passed. Scoped to the crates this plan touches: `keel-agentd` → 65 (61 lib + 4 bin), `keel-controlplane` → 53 (all lib). `keel-spec`/`keel-jail`/`keel-zfs`/`keel-net`/`keelctl` are untouched by this milestone (baseline 36 + 7 = 43 of the 161, carried through unchanged).

---

### Task 1: `keel-agentd::capacity::detect` — read CPU/memory capacity via `sysctl`

**Files:**
- Create: `keel-agentd/src/capacity.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: nothing (self-contained, `std::process::Command` only).
- Produces: `capacity::detect() -> Result<(f64, u64), String>` (first element: CPU cores as reported by `hw.ncpu`; second: memory bytes as reported by `hw.physmem`). Exposed as `keel_agentd::capacity::detect`.

- [ ] **Step 1: Write the failing test**

Create `keel-agentd/src/capacity.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_positive_cpu_and_memory() {
        let (cpu, memory) = detect().expect("sysctl -n hw.ncpu / hw.physmem should succeed on any BSD-derived OS");
        assert!(cpu > 0.0, "expected a positive CPU count, got {cpu}");
        assert!(memory > 0, "expected a positive memory size, got {memory}");
    }
}
```

Modify `keel-agentd/src/lib.rs`, inserting `pub mod capacity;` between the existing `pub mod backoff;` and `pub mod http;` lines (the file's existing `pub mod` lines are alphabetical; `capacity` sorts between `backoff` and `http`):

```rust
pub mod backoff;
pub mod capacity;
pub mod http;
pub mod record;
pub mod reconciler;
pub mod registration;
pub mod store;
pub mod wire;
pub mod worker;
```

No `pub use` addition needed — `capacity::detect` is called as `keel_agentd::capacity::detect()`, matching how `keel_controlplane::worker::spawn` etc. are already called by full path elsewhere in this codebase, not re-exported at the crate root.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p keel-agentd capacity`
Expected: FAIL to compile — `detect` is not yet defined in this module.

- [ ] **Step 3: Implement `detect`**

Add to `keel-agentd/src/capacity.rs`, above the `#[cfg(test)]` block:

```rust
use std::process::Command;

pub fn detect() -> Result<(f64, u64), String> {
    let cpu = run_sysctl("hw.ncpu")?.parse::<f64>().map_err(|e| format!("invalid hw.ncpu value: {e}"))?;
    let memory = run_sysctl("hw.physmem")?.parse::<u64>().map_err(|e| format!("invalid hw.physmem value: {e}"))?;
    Ok((cpu, memory))
}

fn run_sysctl(name: &str) -> Result<String, String> {
    let output =
        Command::new("sysctl").arg("-n").arg(name).output().map_err(|e| format!("failed to run sysctl -n {name}: {e}"))?;
    if !output.status.success() {
        return Err(format!("sysctl -n {name} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p keel-agentd`
Expected: all 66 tests pass (65 inherited, 1 new in `capacity`).

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/capacity.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd's capacity::detect: read CPU/memory capacity via sysctl"
```

---

### Task 2: `Reconciler::committed_resources` and `worker::Command::CommittedResources`

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`
- Modify: `keel-agentd/src/worker.rs`

**Interfaces:**
- Consumes: `Reconciler`'s existing `records: HashMap<String, JailRecord>` field, `keel_spec::parse_cpu_cores`/`parse_memory_bytes` (already used elsewhere in this file).
- Produces: `Reconciler::committed_resources(&self) -> (f64, u64)`; `worker::Command::CommittedResources(Sender<(f64, u64)>)`, handled by `handle_command`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/reconciler.rs`'s `#[cfg(test)] mod tests` block, after `new_starts_with_no_records_on_an_empty_state_dir`:

```rust
    fn sample_spec_with_resources(name: &str, cpu: &str, memory: &str) -> JailSpec {
        let mut spec = sample_spec(name);
        spec.spec.resources = ResourcesSpec { cpu: cpu.to_string(), memory: memory.to_string() };
        spec
    }

    #[test]
    fn committed_resources_on_an_empty_reconciler_is_zero() {
        let dir = test_state_dir("committed_resources_on_an_empty_reconciler_is_zero");
        let reconciler = new_reconciler(dir);
        assert_eq!(reconciler.committed_resources(), (0.0, 0));
    }

    #[test]
    fn committed_resources_sums_across_all_tracked_jails() {
        let dir = test_state_dir("committed_resources_sums_across_all_tracked_jails");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec_with_resources("web-1", "2", "512M")).unwrap();
        reconciler.apply(sample_spec_with_resources("web-2", "1.5", "1G")).unwrap();
        let (cpu, memory) = reconciler.committed_resources();
        assert_eq!(cpu, 3.5);
        assert_eq!(memory, 512 * 1024 * 1024 + 1024 * 1024 * 1024);
    }

    #[test]
    fn committed_resources_drops_a_deleted_jails_contribution() {
        let dir = test_state_dir("committed_resources_drops_a_deleted_jails_contribution");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec_with_resources("web-1", "2", "512M")).unwrap();
        reconciler.apply(sample_spec_with_resources("web-2", "1", "256M")).unwrap();
        reconciler.delete("web-1").unwrap();
        let (cpu, memory) = reconciler.committed_resources();
        assert_eq!(cpu, 1.0);
        assert_eq!(memory, 256 * 1024 * 1024);
    }
```

Add to `keel-agentd/src/worker.rs`'s `Command` enum:

```rust
pub enum Command {
    Apply(JailSpec, Sender<Result<(), ReconcileError>>),
    Get(Option<String>, Sender<Vec<JailStatus>>),
    Delete(String, Sender<Result<(), ReconcileError>>),
    Tick,
    CommittedResources(Sender<(f64, u64)>),
}
```

Add to `worker.rs`'s `#[cfg(test)] mod tests` block, after `tick_command_is_processed_without_blocking_subsequent_commands`:

```rust
    #[test]
    fn committed_resources_command_returns_the_reconcilers_totals() {
        let commands = spawn_test_worker("committed_resources_command_returns_the_reconcilers_totals");

        let (apply_tx, apply_rx) = mpsc::channel();
        commands.send(Command::Apply(sample_spec("web-1"), apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::CommittedResources(tx)).unwrap();
        // sample_spec's fixed resources: cpu "2", memory "512M".
        assert_eq!(rx.recv().unwrap(), (2.0, 512 * 1024 * 1024));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd`
Expected: FAIL to compile — `committed_resources` is not yet defined on `Reconciler`, and `Command::CommittedResources` is not yet handled by `handle_command`.

- [ ] **Step 3: Implement `committed_resources` and the new command**

In `keel-agentd/src/reconciler.rs`, add to the `impl<J, Z, N> Reconciler<J, Z, N>` block, after `list`:

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

In `keel-agentd/src/worker.rs`'s `handle_command`, add a new match arm (after `Command::Tick`):

```rust
        Command::CommittedResources(reply) => {
            let _ = reply.send(reconciler.committed_resources());
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd`
Expected: all 70 tests pass (66 from Task 1, 3 new in `reconciler`, 1 new in `worker`).

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/reconciler.rs keel-agentd/src/worker.rs
git commit -m "Add Reconciler::committed_resources and worker::Command::CommittedResources"
```

---

### Task 3: `keel-controlplane` wire format — capacity and committed-resource fields

**Files:**
- Modify: `keel-controlplane/src/wire.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `NodeRegistration` gains `capacity_cpu: f64`/`capacity_memory: u64`; new `Heartbeat { committed_cpu: f64, committed_memory: u64 }`; `NodeStatus` gains `capacity_cpu`/`capacity_memory`/`committed_cpu`/`committed_memory`.

- [ ] **Step 1: Write the failing tests**

Update the two existing round-trip tests in the `#[cfg(test)] mod tests` block, leaving the struct definitions above them untouched for now (this is what makes Step 2's failure a genuine one: the tests below reference fields that don't exist yet on the current two-field `NodeRegistration`/four-field `NodeStatus`):

```rust
    #[test]
    fn node_registration_round_trips_through_yaml() {
        let registration = NodeRegistration {
            id: "node-1".to_string(),
            addr: "192.168.64.4".to_string(),
            capacity_cpu: 4.0,
            capacity_memory: 8 * 1024 * 1024 * 1024,
        };
        let yaml = serde_yaml::to_string(&registration).unwrap();
        let parsed: NodeRegistration = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, registration);
    }

    #[test]
    fn node_status_round_trips_through_yaml() {
        let status = NodeStatus {
            id: "node-1".to_string(),
            addr: "192.168.64.4".to_string(),
            status: NodeState::Alive,
            last_seen_secs: 3,
            capacity_cpu: 4.0,
            capacity_memory: 8 * 1024 * 1024 * 1024,
            committed_cpu: 1.5,
            committed_memory: 512 * 1024 * 1024,
        };
        let yaml = serde_yaml::to_string(&status).unwrap();
        let parsed: NodeStatus = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, status);
    }
```

Add a new test, after `node_status_round_trips_through_yaml`:

```rust
    #[test]
    fn heartbeat_round_trips_through_yaml() {
        let heartbeat = Heartbeat { committed_cpu: 2.0, committed_memory: 1024 * 1024 * 1024 };
        let yaml = serde_yaml::to_string(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, heartbeat);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane wire`
Expected: FAIL to compile — `NodeRegistration`/`NodeStatus` literals in the tests reference fields (`capacity_cpu`, `committed_cpu`, etc.) that don't exist on the current struct definitions, and `Heartbeat` doesn't exist at all yet.

- [ ] **Step 3: Add the new fields and the `Heartbeat` type**

Replace `keel-controlplane/src/wire.rs`'s type definitions (everything above `#[cfg(test)]`) with:

```rust
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum NodeState {
    Alive,
    Dead,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 54 tests pass (53 inherited, 1 new: `heartbeat_round_trips_through_yaml`; the two modified tests are not new, just updated).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/wire.rs
git commit -m "Add capacity/committed-resource fields to keel-controlplane's wire types"
```

---

### Task 4: `keel-controlplane::Registry` — store and refresh capacity/committed resources

**Files:**
- Modify: `keel-controlplane/src/registry.rs`

**Interfaces:**
- Consumes: `NodeStatus`'s new fields (Task 3).
- Produces: `Registry::register(&mut self, id: String, addr: String, capacity_cpu: f64, capacity_memory: u64, now: Instant)` (was 3 args, now 5); `Registry::heartbeat(&mut self, id: &str, committed_cpu: f64, committed_memory: u64, now: Instant) -> Result<(), UnknownNode>` (was 2 args, now 4).

- [ ] **Step 1: Write the failing tests**

In `keel-controlplane/src/registry.rs`, replace `NodeRecord`'s definition:

```rust
#[derive(Debug, Clone)]
struct NodeRecord {
    addr: String,
    last_heartbeat: Instant,
    capacity_cpu: f64,
    capacity_memory: u64,
    committed_cpu: f64,
    committed_memory: u64,
}
```

Update every existing test's `register`/`heartbeat` call to the new signatures — there are seven `register(...)` calls and one `heartbeat(...)` call across the existing test module; add `4.0, 8 * 1024 * 1024 * 1024` as the two new arguments to every `register(...)` call (right after `addr`, before `now`), and `0.0, 0` as the two new arguments to the one existing `heartbeat(...)` call (`heartbeat_on_a_known_node_updates_its_last_heartbeat`, right after `id`, before `t1`).

Add two new tests, after `resolve_on_a_dead_node_returns_dead_error_with_elapsed_seconds`:

```rust
    #[test]
    fn register_initializes_committed_resources_to_zero() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now);

        let statuses = registry.list(now);
        assert_eq!(statuses[0].capacity_cpu, 4.0);
        assert_eq!(statuses[0].capacity_memory, 8 * 1024 * 1024 * 1024);
        assert_eq!(statuses[0].committed_cpu, 0.0);
        assert_eq!(statuses[0].committed_memory, 0);
    }

    #[test]
    fn heartbeat_updates_committed_resources_without_changing_capacity() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0);

        let t1 = t0 + Duration::from_secs(5);
        registry.heartbeat("node-1", 2.0, 1024 * 1024 * 1024, t1).unwrap();

        let statuses = registry.list(t1);
        assert_eq!(statuses[0].committed_cpu, 2.0);
        assert_eq!(statuses[0].committed_memory, 1024 * 1024 * 1024);
        assert_eq!(statuses[0].capacity_cpu, 4.0, "heartbeat must not change capacity");
        assert_eq!(statuses[0].capacity_memory, 8 * 1024 * 1024 * 1024, "heartbeat must not change capacity");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane registry`
Expected: FAIL to compile — `register`/`heartbeat` still take their old 3/2-argument signatures.

- [ ] **Step 3: Implement the new signatures**

Replace `register` and `heartbeat` in `keel-controlplane/src/registry.rs`:

```rust
    pub fn register(&mut self, id: String, addr: String, capacity_cpu: f64, capacity_memory: u64, now: Instant) {
        self.nodes.insert(
            id,
            NodeRecord {
                addr,
                last_heartbeat: now,
                capacity_cpu,
                capacity_memory,
                committed_cpu: 0.0,
                committed_memory: 0,
            },
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

Update `list` to carry the four new fields through into each `NodeStatus`:

```rust
    pub fn list(&self, now: Instant) -> Vec<NodeStatus> {
        let mut statuses: Vec<NodeStatus> = self
            .nodes
            .iter()
            .map(|(id, record)| {
                let elapsed = now.saturating_duration_since(record.last_heartbeat);
                NodeStatus {
                    id: id.clone(),
                    addr: record.addr.clone(),
                    status: if elapsed < DEAD_THRESHOLD { NodeState::Alive } else { NodeState::Dead },
                    last_seen_secs: elapsed.as_secs(),
                    capacity_cpu: record.capacity_cpu,
                    capacity_memory: record.capacity_memory,
                    committed_cpu: record.committed_cpu,
                    committed_memory: record.committed_memory,
                }
            })
            .collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }
```

`resolve` is completely unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 56 tests pass (54 from Task 3, 2 new in `registry`).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/registry.rs
git commit -m "Add capacity/committed-resource tracking to keel-controlplane's Registry"
```

---

### Task 5: `keel-controlplane::scheduler` — replace count-based `pick_node` with resource-aware scoring

**Files:**
- Modify: `keel-controlplane/src/scheduler.rs`

**Interfaces:**
- Consumes: nothing (pure function, no dependency on `wire`/`Registry`).
- Produces: `NodeResources { id: String, capacity_cpu: f64, capacity_memory: u64, committed_cpu: f64, committed_memory: u64 }`; `pick_node(nodes: &[NodeResources]) -> Result<String, ScheduleError>` (was `pick_node(alive_ids: &[String], counts: &HashMap<&str, usize>)`); `ScheduleError::NoAvailableNodes` unchanged.

- [ ] **Step 1: Write the failing tests**

Replace the entire contents of `keel-controlplane/src/scheduler.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no alive nodes available to schedule onto")]
    NoAvailableNodes,
}

pub struct NodeResources {
    pub id: String,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
    pub committed_cpu: f64,
    pub committed_memory: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, capacity_cpu: f64, capacity_memory: u64, committed_cpu: f64, committed_memory: u64) -> NodeResources {
        NodeResources { id: id.to_string(), capacity_cpu, capacity_memory, committed_cpu, committed_memory }
    }

    #[test]
    fn no_alive_nodes_returns_no_available_nodes_error() {
        let nodes: Vec<NodeResources> = vec![];
        assert_eq!(pick_node(&nodes), Err(ScheduleError::NoAvailableNodes));
    }

    #[test]
    fn a_single_alive_node_is_always_picked() {
        let nodes = vec![node("node-1", 4.0, 8 * 1024 * 1024 * 1024, 0.0, 0)];
        assert_eq!(pick_node(&nodes), Ok("node-1".to_string()));
    }

    #[test]
    fn the_node_with_more_headroom_in_its_most_constrained_resource_wins() {
        // node-1: 50% cpu headroom, 90% memory headroom -> min = 0.5
        // node-2: 90% cpu headroom, 50% memory headroom -> min = 0.5
        // node-3: 75% cpu headroom, 75% memory headroom -> min = 0.75, wins
        let nodes = vec![
            node("node-1", 4.0, 100, 2.0, 10),
            node("node-2", 4.0, 100, 0.4, 50),
            node("node-3", 4.0, 100, 1.0, 25),
        ];
        assert_eq!(pick_node(&nodes), Ok("node-3".to_string()));
    }

    #[test]
    fn ties_on_the_min_fraction_score_are_broken_by_ascending_node_id() {
        let nodes = vec![node("node-2", 4.0, 100, 2.0, 50), node("node-1", 4.0, 100, 2.0, 50)];
        assert_eq!(pick_node(&nodes), Ok("node-1".to_string()));
    }

    #[test]
    fn an_over_committed_node_is_still_picked_when_it_is_the_only_alive_one() {
        // committed_cpu exceeds capacity_cpu: negative headroom, but still the only option.
        let nodes = vec![node("node-1", 4.0, 100, 6.0, 50)];
        assert_eq!(pick_node(&nodes), Ok("node-1".to_string()));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane scheduler`
Expected: FAIL to compile — `pick_node` is not yet defined.

- [ ] **Step 3: Implement `pick_node`**

Add to `keel-controlplane/src/scheduler.rs`, after the `NodeResources` definition (before `#[cfg(test)]`):

```rust
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

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 56 tests pass (56 from Task 4; this task replaces 5 old scheduler tests with 5 new ones, net count unchanged).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/scheduler.rs
git commit -m "Replace keel-controlplane's count-based pick_node with resource-aware headroom scoring"
```

---

### Task 6: `keel-controlplane::worker` — wire the new scheduler into `ResolveOrSchedule`, delete `Placements::counts()`

**Files:**
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/placements.rs`

**Interfaces:**
- Consumes: `Registry::{register, heartbeat}` (Task 4), `scheduler::{NodeResources, pick_node}` (Task 5).
- Produces: `Command::Register(String, String, f64, u64, Sender<()>)` (was 3 fields, now 5); `Command::Heartbeat(String, f64, u64, Sender<Result<(), UnknownNode>>)` (was 2 fields, now 4); `ResolveOrSchedule`'s scheduling behavior is now resource-aware. `Placements::counts()` no longer exists.

- [ ] **Step 1: Write the failing tests (and update every existing call site to the new signatures)**

In `keel-controlplane/src/worker.rs`, update the `Command` enum:

```rust
pub enum Command {
    Register(String, String, f64, u64, Sender<()>),
    Heartbeat(String, f64, u64, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
    ResolveOrSchedule(String, Sender<Result<(String, String), ScheduleOrResolveError>>),
    ResolvePlacement(String, Sender<Result<(String, String), PlacementError>>),
    RecordPlacement(String, String, Sender<()>),
    RemovePlacement(String, Sender<()>),
}
```

In the `#[cfg(test)] mod tests` block, replace the standalone `register_node` helper and add a new `heartbeat_node` helper:

```rust
    fn register_node(commands: &Sender<Command>, id: &str, addr: &str, capacity_cpu: f64, capacity_memory: u64) {
        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(id.to_string(), addr.to_string(), capacity_cpu, capacity_memory, reg_tx))
            .unwrap();
        reg_rx.recv().unwrap();
    }

    fn heartbeat_node(commands: &Sender<Command>, id: &str, committed_cpu: f64, committed_memory: u64) {
        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat(id.to_string(), committed_cpu, committed_memory, hb_tx)).unwrap();
        hb_rx.recv().unwrap().unwrap();
    }
```

Update every existing test that calls `Command::Register` directly (not through the helper) or the old `register_node`/inline pattern to the new 5-field `Command::Register` / new `register_node` signature: `register_command_makes_the_node_visible_in_list`, `heartbeat_command_on_a_registered_node_succeeds` (also update its `Command::Heartbeat` send to the new 4-field form, with `0.0, 0` as the committed values), `resolve_command_on_a_registered_alive_node_returns_its_address`. Use `4.0, 8 * 1024 * 1024 * 1024` as filler capacity for any test that doesn't care about the actual numbers.

Replace `resolve_or_schedule_on_a_fresh_jail_name_schedules_onto_the_least_loaded_alive_node` (its whole premise — jail count — no longer applies) with:

```rust
    #[test]
    fn resolve_or_schedule_on_a_fresh_jail_name_schedules_onto_the_node_with_more_headroom() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 100);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 100);
        heartbeat_node(&commands, "node-1", 3.0, 10); // 25% cpu headroom
        heartbeat_node(&commands, "node-2", 1.0, 10); // 75% cpu headroom

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-2".to_string(), "10.0.0.2".to_string())));
    }
```

Update `resolve_or_schedule_on_an_already_placed_jail_is_sticky` and `record_then_remove_placement_is_reflected_by_resolve_placement` to use the new `register_node` signature (add `4.0, 8 * 1024 * 1024 * 1024` to each call).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo build -p keel-controlplane`
Expected: FAIL to compile — `handle_command` still destructures `Command::Register`/`Command::Heartbeat` with the old arities, and calls `registry.register`/`registry.heartbeat` with the old signatures.

- [ ] **Step 3: Rewire `handle_command` and delete `Placements::counts()`**

In `keel-controlplane/src/worker.rs`, replace the `Command::Register`, `Command::Heartbeat`, and `Command::ResolveOrSchedule` arms of `handle_command`:

```rust
        Command::Register(id, addr, capacity_cpu, capacity_memory, reply) => {
            registry.register(id, addr, capacity_cpu, capacity_memory, Instant::now());
            let _ = reply.send(());
        }
        Command::Heartbeat(id, committed_cpu, committed_memory, reply) => {
            let result = registry.heartbeat(&id, committed_cpu, committed_memory, Instant::now());
            let _ = reply.send(result);
        }
```

```rust
        Command::ResolveOrSchedule(jail_name, reply) => {
            let now = Instant::now();
            let result = if let Some(node_id) = placements.get(&jail_name).map(|s| s.to_string()) {
                registry.resolve(&node_id, now).map(|addr| (node_id, addr)).map_err(ScheduleOrResolveError::from)
            } else {
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
                    registry
                        .resolve(&node_id, now)
                        .map(|addr| (node_id, addr))
                        .map_err(ScheduleOrResolveError::from)
                })
            };
            let _ = reply.send(result);
        }
```

In `keel-controlplane/src/placements.rs`, delete the `counts` method entirely:

```rust
    /// node_id -> number of jails currently recorded against it.
    pub fn counts(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for node_id in self.by_jail.values() {
            *counts.entry(node_id.as_str()).or_insert(0) += 1;
        }
        counts
    }
```

and delete its two dedicated tests, `counts_aggregates_multiple_jails_on_the_same_node` and `counts_on_an_empty_table_is_empty`. In `set_again_on_the_same_jail_overwrites_rather_than_duplicating`, delete only these two lines, keeping the rest of the test (its `.get()` assertion) as-is:

```rust
        assert_eq!(placements.counts().get("node-1"), None);
        assert_eq!(placements.counts().get("node-2"), Some(&1));
```

If `HashMap` becomes unused in `placements.rs` after this deletion, remove its `use` line too (it won't: `by_jail: HashMap<String, String>` still needs it).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo build -p keel-controlplane && cargo test -p keel-controlplane`
Expected: build succeeds; all 54 tests pass (56 from Task 5, minus 2 deleted `placements` tests, worker.rs's own test count unchanged at 11 since its rewritten test replaces the old one 1-for-1).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/placements.rs
git commit -m "Wire resource-aware scheduling into ResolveOrSchedule; delete Placements::counts"
```

---

### Task 7: `keel-controlplane::http` — accept capacity/committed fields over the wire

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `Command::Register`/`Command::Heartbeat`'s new signatures (Task 6), `NodeRegistration`/`Heartbeat`'s new/changed fields (Task 3).
- Produces: `handle_register` deserializes the extended `NodeRegistration`; `handle_heartbeat` gains a `body: &[u8]` parameter and deserializes the new `Heartbeat` type; `route()`'s heartbeat arm passes the request body through.

- [ ] **Step 1: Write the failing tests (and fix every existing register/heartbeat test body)**

In `keel-controlplane/src/http.rs`'s `route()` function, change the heartbeat arm from `("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, commands),` to:

```rust
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, &request.body, commands),
```

Update the `#[cfg(test)] mod tests` block's `register_node` helper (used by 10 other tests in this file) to send capacity fields:

```rust
    fn register_node(cp_addr: &str, id: &str, node_addr: &str) {
        send_request(
            cp_addr,
            "POST",
            "/nodes/register",
            &format!("id: {id}\naddr: {node_addr}\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n"),
        );
    }
```

Update the five existing tests that build a registration/heartbeat body directly (not through `register_node`):

```rust
    #[test]
    fn register_returns_200_and_the_node_appears_in_get_nodes() {
        let addr = start_test_server();
        let (status, _) = send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );
        assert_eq!(status, 200);

        let (status, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert!(body.contains("node-1"), "expected node-1 in body: {body}");
        assert!(body.contains("Alive"), "expected Alive status in body: {body}");
    }

    #[test]
    fn reregistering_the_same_id_updates_its_address_without_duplicating() {
        let addr = start_test_server();
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.2\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );

        let (_, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(body.matches("node-1").count(), 1, "expected exactly one node-1 entry, got body: {body}");
        assert!(body.contains("10.0.0.2"), "expected refreshed address in body: {body}");
    }

    #[test]
    fn heartbeat_on_a_registered_node_returns_200() {
        let addr = start_test_server();
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );

        let (status, _) = send_request(&addr, "POST", "/nodes/node-1/heartbeat", "committed_cpu: 1\ncommitted_memory: 1073741824\n");
        assert_eq!(status, 200);
    }

    #[test]
    fn heartbeat_on_an_unknown_node_returns_404() {
        let addr = start_test_server();
        let (status, body) =
            send_request(&addr, "POST", "/nodes/missing/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\n");
        assert_eq!(status, 404);
        assert!(body.contains("missing"));
    }

    #[test]
    fn register_with_invalid_yaml_returns_400() {
        let addr = start_test_server();
        let (status, _) = send_request(&addr, "POST", "/nodes/register", "not: valid: yaml: at: all: -");
        assert_eq!(status, 400);
    }
```

(`register_with_invalid_yaml_returns_400` is unchanged in content — shown here only so the diff context around it is unambiguous.)

Add two new tests, after `register_with_invalid_yaml_returns_400`:

```rust
    #[test]
    fn get_nodes_includes_capacity_and_committed_resources() {
        let addr = start_test_server();
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );

        let (_, body) = send_request(&addr, "GET", "/nodes", "");
        assert!(body.contains("capacity_cpu: 4"), "got: {body}");
        assert!(body.contains("capacity_memory: 8589934592"), "got: {body}");
        assert!(body.contains("committed_cpu: 0"), "got: {body}");
        assert!(body.contains("committed_memory: 0"), "got: {body}");
    }

    #[test]
    fn heartbeat_with_invalid_yaml_body_returns_400() {
        let addr = start_test_server();
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );

        let (status, _) = send_request(&addr, "POST", "/nodes/node-1/heartbeat", "not: valid: yaml: at: all: -");
        assert_eq!(status, 400);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo build -p keel-controlplane`
Expected: FAIL to compile — `handle_heartbeat` still takes only `(id, commands)`, and `handle_register` doesn't yet deserialize the extended `NodeRegistration` shape into a 5-argument `Command::Register`.

- [ ] **Step 3: Implement the new bodies**

Replace `handle_register` and `handle_heartbeat` in `keel-controlplane/src/http.rs`:

```rust
fn handle_register(body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let registration: NodeRegistration = match serde_yaml::from_slice(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands
        .send(Command::Register(registration.id, registration.addr, registration.capacity_cpu, registration.capacity_memory, reply_tx))
        .is_err()
    {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(()) => (200, Vec::new()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_heartbeat(id: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let heartbeat: crate::wire::Heartbeat = match serde_yaml::from_slice(body) {
        Ok(h) => h,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands
        .send(Command::Heartbeat(id.to_string(), heartbeat.committed_cpu, heartbeat.committed_memory, reply_tx))
        .is_err()
    {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}
```

Update the top import line to bring in `Heartbeat`:

```rust
use crate::wire::{ErrorBody, Heartbeat, NodeRegistration};
```

(then simplify `handle_heartbeat`'s body type annotation from `crate::wire::Heartbeat` to plain `Heartbeat`, now that it's imported directly).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 56 tests pass (54 from Task 6, 2 new: `get_nodes_includes_capacity_and_committed_resources`, `heartbeat_with_invalid_yaml_body_returns_400`).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Accept capacity/committed-resource fields in keel-controlplane's register/heartbeat routes"
```

---

### Task 8: `keel-agentd::registration` and `main.rs` — report capacity and committed load

**Files:**
- Modify: `keel-agentd/src/registration.rs`
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `capacity::detect` (Task 1), `worker::Command::CommittedResources` (Task 2), `keel-controlplane`'s now-required registration/heartbeat fields (Task 7).
- Produces: `registration::spawn`'s new signature: `spawn(node_id: String, advertise_addr: String, control_plane_addr: String, heartbeat_interval: Duration, capacity_cpu: f64, capacity_memory: u64, commands: Sender<crate::worker::Command>) -> JoinHandle<()>` (was 4 params, now 7).

- [ ] **Step 1: Write the failing test**

In `keel-agentd/src/registration.rs`'s `#[cfg(test)] mod tests` block, update `start_test_control_plane` to register-compatible capacity (no change needed there — it's the fake control plane, unaffected), and update `registers_and_then_keeps_heartbeating`:

```rust
    #[test]
    fn registers_and_then_keeps_heartbeating() {
        let control_plane_addr = start_test_control_plane();
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                keel_net::FakeNetManager::new(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-registers_and_then_keeps_heartbeating"),
            )
            .unwrap(),
        );
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            commands,
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(body.contains("node-1"), "expected node-1 to have registered, got: {body}");
        assert!(body.contains("Alive"), "expected node-1 to be Alive, got: {body}");
        assert!(body.contains("capacity_cpu: 4"), "expected reported capacity in body: {body}");
    }
```

Add a second test, after it:

```rust
    #[test]
    fn heartbeats_report_the_reconcilers_committed_resources() {
        let control_plane_addr = start_test_control_plane();
        let zfs = keel_zfs::FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = crate::Reconciler::new(
            keel_jail::FakeJailRuntime::new(),
            zfs,
            keel_net::FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join("keel-agentd-registration-test-heartbeats_report_the_reconcilers_committed_resources"),
        )
        .unwrap();
        let (_worker_handle, commands) = crate::worker::spawn(reconciler);

        let (apply_tx, apply_rx) = mpsc::channel();
        commands
            .send(crate::worker::Command::Apply(
                keel_spec::JailSpec {
                    api_version: "keel/v1".to_string(),
                    kind: "Jail".to_string(),
                    metadata: keel_spec::Metadata { name: "web-1".to_string() },
                    spec: keel_spec::Spec {
                        image: "base/14.2-web".to_string(),
                        command: vec!["/usr/local/bin/myapp".to_string()],
                        network: keel_spec::NetworkSpec {
                            vnet: true,
                            bridge: "keel0".to_string(),
                            address: "10.0.0.5/24".to_string(),
                        },
                        resources: keel_spec::ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                        restart_policy: keel_spec::RestartPolicy::Always,
                    },
                },
                apply_tx,
            ))
            .unwrap();
        apply_rx.recv().unwrap().unwrap();

        let control_plane_addr_clone = control_plane_addr.clone();
        let _handle =
            spawn("node-1".to_string(), "10.0.0.1".to_string(), control_plane_addr_clone, Duration::from_millis(50), 4.0, 8 * 1024 * 1024 * 1024, commands);

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(body.contains("committed_cpu: 2"), "expected committed_cpu from the applied jail, got: {body}");
        assert!(body.contains("committed_memory: 536870912"), "expected committed_memory from the applied jail, got: {body}");
    }
```

Add `use std::sync::mpsc;` to the test module's imports if not already present via `use super::*;` (it is not — `mpsc` is used directly by `send_request`/`heartbeat_once` in the parent module via `std::sync::mpsc`, but the test module needs its own explicit channel for the `Apply` reply; check whether `super::*` already brings `mpsc` into scope from the parent's `use std::sync::mpsc::{...}`-less registration.rs — it does not, since the parent module doesn't import the full `mpsc` path — add `use std::sync::mpsc;` directly to the test module).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p keel-agentd registration`
Expected: FAIL to compile — `spawn` still takes only 4 parameters.

- [ ] **Step 3: Implement the new `spawn`, `register_once`, and `heartbeat_once`**

Replace `keel-agentd/src/registration.rs`'s `spawn`, `register_once`, and `heartbeat_once`:

```rust
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr, capacity_cpu, capacity_memory) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id, &commands) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("keel-agentd: heartbeat failed: {e}");
                        registered = false;
                    }
                }
            }
            thread::sleep(heartbeat_interval);
        }
    })
}

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

fn heartbeat_once(control_plane_addr: &str, node_id: &str, commands: &Sender<crate::worker::Command>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body)
}
```

Update the top of the file to import `Sender`:

```rust
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;
```

In `keel-agentd/src/main.rs`, move `worker::spawn(reconciler)` before nothing needs to change order-wise (it already runs before the control-plane block), and update the control-plane block:

```rust
    if let (Some(node_id), Some(control_plane_addr), Some(advertise_addr)) =
        (config.node_id.clone(), config.control_plane_addr.clone(), config.advertise_addr.clone())
    {
        let (capacity_cpu, capacity_memory) = keel_agentd::capacity::detect()
            .unwrap_or_else(|e| panic!("failed to detect node capacity via sysctl: {e}"));
        eprintln!(
            "keel-agentd: registering with control plane at {control_plane_addr} as node '{node_id}' ({advertise_addr}), capacity {capacity_cpu} cores / {capacity_memory} bytes"
        );
        keel_agentd::registration::spawn(
            node_id,
            advertise_addr.clone(),
            control_plane_addr,
            Duration::from_secs(5),
            capacity_cpu,
            capacity_memory,
            commands.clone(),
        );

        eprintln!("keel-agentd: serving jails API over TCP on {advertise_addr}");
        let tcp_listener = TcpListener::bind(&advertise_addr)
            .unwrap_or_else(|e| panic!("failed to bind jails-API TCP listener on {advertise_addr}: {e}"));
        let tcp_commands = commands.clone();
        thread::spawn(move || keel_agentd::http::run_tcp(tcp_listener, tcp_commands));
    }
```

(`commands.clone()` here is one more clone than before — `main.rs` already clones `commands` twice, for the TCP listener and the timer thread; this adds a third clone for the registration thread, all from the same original `Sender` returned by `worker::spawn`.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo build --workspace && cargo test --workspace`
Expected: the whole workspace builds; all 171 tests pass (161 baseline + 10 new: 1 in `capacity`, 4 in `reconciler`/`worker` from Task 2, 1 in `wire`, 2 in `registry`, 0 net in `scheduler`, -2 net in `placements`/`worker` from Task 6, 2 in `http` from Task 7, 2 in `registration` from this task — sum: `+1+4+1+2+0-2+2+2 = 10`).

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/registration.rs keel-agentd/src/main.rs
git commit -m "Report detected capacity and committed resources from keel-agentd's registration thread"
```

---

### Task 9: FreeBSD VM verification

**Files:** none (verification only, run by the coordinating session directly, not a subagent)

- [ ] **Step 1: Build and deploy the updated binaries to the three VMs**

Pull the latest commits on `.2`/`.4`/`.5` (`git pull --ff-only origin main` — push local `master` to `origin/main` first if needed), then `cargo build --release --workspace` on each and reinstall `keel-agentd`/`keel-controlplane`/`keelctl` to `/usr/local/bin`, the same process used for Milestone 9's Task 6.

- [ ] **Step 2: Confirm `sysctl`-detected capacity appears correctly**

Start `keel-controlplane` on `.2`, then `keel-agentd` on `.4` and `.5` with their existing control-plane flags (`--node-id`, `--control-plane-addr 192.168.64.2:7620`, `--advertise-addr`). Query `GET /nodes` (`curl -s http://192.168.64.2:7620/nodes`) and confirm `capacity_cpu`/`capacity_memory` for both nodes are positive and match what `ssh root@192.168.64.4 sysctl -n hw.ncpu hw.physmem` (and the same on `.5`) actually report on each host directly — not just "some positive number", the *exact* values.

- [ ] **Step 3: Confirm resource-aware placement favors headroom, not just fewer jails**

Apply a jail with a large resource request (e.g. `cpu: "3", memory: "2G"`) to deliberately commit most of one node's reported capacity, confirm via `GET /nodes` that its `committed_cpu`/`committed_memory` rose accordingly within one heartbeat interval, then apply a second, differently-named jail through the scheduler (no `--node`) and confirm it lands on the *other* node (more headroom), even if that other node happens to have a higher `capacity_cpu`/lower absolute committed number — the decision must be by headroom fraction, not raw numbers. Confirm via `jls` on both `.4` and `.5` directly.

- [ ] **Step 4: Confirm sticky re-apply and named-node routing are unaffected**

Re-apply the second jail (no `--node`) and confirm it stays on the node it was scheduled to, exactly as Milestone 9 already verified — this milestone changes what informs the *first* scheduling decision, not the sticky behavior itself. Then apply a third jail with an explicit `--node`, confirm it lands there regardless of that node's reported headroom, confirming named-node routing still bypasses the scheduler entirely.

- [ ] **Step 5: Clean teardown**

Delete all jails applied during this task from both nodes, confirm via `jls` on `.4` and `.5` that nothing is left running, and stop the manually-started `keel-controlplane`/`keel-agentd` processes, returning the VMs to their pre-task idle state.

- [ ] **Step 6: Record the outcome**

No code changes result from this task if everything behaves as designed. If any step surfaces a real bug, stop and treat it as a new task inserted before the final commit, following the same TDD steps as Tasks 1-8.

---

## Final Review

Once Tasks 1-9 are complete, do a whole-branch review (same discipline as every prior milestone): re-run `cargo test --workspace` and confirm the final count (66 `keel-agentd` lib-side additions folded in — recompute precisely as 72 `keel-agentd` [65 baseline + 7 new: 1 capacity + 3 reconciler + 1 worker(Task 2) + 2 registration(Task 8)] + 56 `keel-controlplane` [53 baseline + 3 net: +1 wire +2 registry +0 scheduler -2 placements +2 http, i.e. 53+1+2+0-2+2=56] + 7 `keelctl` + 36 untouched = 171 passed), then update `README.md`'s roadmap (mark item 10 done, add the Milestone 10 write-up) and the website pages, mirroring exactly what was done for Milestones 8 and 9's doc updates.
