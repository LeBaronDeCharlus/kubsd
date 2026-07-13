# keel Milestone 9: Scheduler, Automatic Node Placement — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 6:** it needs the real FreeBSD VMs (`root@192.168.64.2`, `.4`, `.5`). The coordinating session has direct SSH access to these VMs and should run this task itself rather than dispatching a subagent for it — this mirrors Milestone 8's Task 7 and every prior milestone's real-hardware task. **Tasks 1-5 are pure file edits, verified locally (macOS) via `cargo test`, and involve zero FreeBSD-specific code (no `keel-agentd`, `keel-jail`, `keel-zfs`, or `keel-net` changes at all in this milestone).**

**Goal:** Let a client apply/get/delete a `JailSpec` without naming a node at all; `keel-controlplane` picks the `Alive` node with the fewest jails it has placed there, remembers the choice, and forwards there — sticky on re-apply, with the caller's existing option to still name an exact node unaffected.

**Architecture:** A new `Placements` table (`jail_name -> node_id`) in `keel-controlplane`, owned by the same single worker thread that already owns `Registry`. A new pure `scheduler::pick_node` function picks the `Alive` node with the fewest entries in `Placements`, ties broken by ascending node id. Three new HTTP routes, `PUT`/`GET`/`DELETE /jails/{name}` (no node segment), schedule-or-reuse a placement and forward exactly like Milestone 8's existing `/nodes/{id}/jails/{name}` routes, which are extended to also record/remove placements on success so both route families share one consistent table. `keelctl`'s `--node` flag becomes optional: omit it (with `--control-plane-addr` still set) to trigger scheduling.

**Tech Stack:** Rust (2021 edition), same dependencies already used throughout (`serde`, `serde_yaml`, `thiserror`, `httparse`) — no new external dependencies anywhere.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-13-keel-agent-milestone9-scheduler-design.md` (Approved). Route shapes, the sticky-on-reapply behavior, the pure-read-then-confirmed-write bookkeeping split, and the 503-vs-404 error split described there must match exactly.
- **No new external dependencies.** Every crate touched in this plan (`keel-controlplane`, `keelctl`) already depends on everything needed.
- **`worker::spawn`'s signature changes** from `pub fn spawn(mut registry: Registry) -> (JoinHandle<()>, Sender<Command>)` to `pub fn spawn(mut registry: Registry, mut placements: Placements) -> (JoinHandle<()>, Sender<Command>)` (Task 3). This has **four** call sites across the workspace that must all be updated in the same task/commit they're touched in: `keel-controlplane/src/worker.rs`'s own tests (Task 3), `keel-controlplane/src/main.rs` (Task 3), `keel-controlplane/src/http.rs`'s `start_test_server` test helper (Task 4), and `keelctl/tests/cli.rs`'s `start_test_control_plane_with_node` test helper (Task 5). Each is updated in the task that owns that file, not all at once — expect `cargo build -p keel-controlplane` in Task 3 to succeed while `cargo test -p keelctl` would still fail to compile until Task 5; that's expected mid-plan, not a bug.
- **No new dependency direction.** `Placements` and `scheduler::pick_node` are plain, self-contained modules; neither `keel-agentd` nor `keel-spec` is touched or referenced anywhere in this milestone.
- **A jail whose sticky node has gone `Dead` is never silently rescheduled.** `ResolveOrSchedule` only schedules fresh when a jail has *no* recorded placement; an existing placement is always resolved (and its Dead/Unknown status surfaced as an error) rather than replaced. This is a deliberate behavior, not a gap to fix in this plan.
- **`--node` requires `--control-plane-addr`, not the other way around.** `keelctl`'s existing bidirectional pairing check (Milestone 8: `"--control-plane-addr and --node must be given together"`) is replaced with a one-directional check (Task 5); this changes one existing test's expected error string.
- No placeholders: every task's deliverable is verified with `cargo build -p <crate> && cargo test -p <crate>` before its commit step.
- Current baseline entering this milestone (verified directly before writing this plan): `cargo test --workspace` → 139 passed. Scoped to the crates this plan touches: `keel-controlplane` → 31 (all lib), `keelctl` → 7 (all integration, in `tests/cli.rs`; 0 unit tests in `main.rs`).

---

### Task 1: `Placements` — the jail-name-to-node-id table

**Files:**
- Create: `keel-controlplane/src/placements.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Consumes: nothing (self-contained, `std::collections::HashMap` only).
- Produces: `Placements` (`#[derive(Debug, Default)]`), `Placements::new() -> Self`, `Placements::get(&self, jail_name: &str) -> Option<&str>`, `Placements::set(&mut self, jail_name: String, node_id: String)`, `Placements::remove(&mut self, jail_name: &str)`, `Placements::counts(&self) -> HashMap<&str, usize>`. Re-exported as `keel_controlplane::Placements`.

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/placements.rs`:

```rust
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct Placements {
    by_jail: HashMap<String, String>,
}

impl Placements {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_on_an_empty_table_returns_none() {
        let placements = Placements::new();
        assert_eq!(placements.get("web-1"), None);
    }

    #[test]
    fn set_then_get_returns_the_recorded_node() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        assert_eq!(placements.get("web-1"), Some("node-1"));
    }

    #[test]
    fn set_again_on_the_same_jail_overwrites_rather_than_duplicating() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.set("web-1".to_string(), "node-2".to_string());
        assert_eq!(placements.get("web-1"), Some("node-2"));
        assert_eq!(placements.counts().get("node-1"), None);
        assert_eq!(placements.counts().get("node-2"), Some(&1));
    }

    #[test]
    fn remove_clears_the_placement() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.remove("web-1");
        assert_eq!(placements.get("web-1"), None);
    }

    #[test]
    fn counts_aggregates_multiple_jails_on_the_same_node() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.set("web-2".to_string(), "node-1".to_string());
        placements.set("web-3".to_string(), "node-2".to_string());
        let counts = placements.counts();
        assert_eq!(counts.get("node-1"), Some(&2));
        assert_eq!(counts.get("node-2"), Some(&1));
    }

    #[test]
    fn counts_on_an_empty_table_is_empty() {
        let placements = Placements::new();
        assert_eq!(placements.counts(), HashMap::new());
    }
}
```

Modify `keel-controlplane/src/lib.rs`:

```rust
pub mod http;
pub mod placements;
pub mod registry;
pub mod wire;
pub mod worker;

pub use placements::Placements;
pub use registry::Registry;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane placements`
Expected: FAIL to compile — `get`/`set`/`remove`/`counts` are not yet defined on `Placements`.

- [ ] **Step 3: Implement `Placements`**

In `keel-controlplane/src/placements.rs`, add to the `impl Placements` block (after `new`):

```rust
    pub fn get(&self, jail_name: &str) -> Option<&str> {
        self.by_jail.get(jail_name).map(|s| s.as_str())
    }

    pub fn set(&mut self, jail_name: String, node_id: String) {
        self.by_jail.insert(jail_name, node_id);
    }

    pub fn remove(&mut self, jail_name: &str) {
        self.by_jail.remove(jail_name);
    }

    /// node_id -> number of jails currently recorded against it.
    pub fn counts(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for node_id in self.by_jail.values() {
            *counts.entry(node_id.as_str()).or_insert(0) += 1;
        }
        counts
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 37 tests pass (31 inherited, 6 new in `placements`; `lib.rs`'s new `pub mod`/`pub use` lines add no tests of their own).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/placements.rs keel-controlplane/src/lib.rs
git commit -m "Add keel-controlplane's Placements: the jail-name-to-node-id table"
```

---

### Task 2: `scheduler::pick_node` — least-loaded-by-count, ties by ascending id

**Files:**
- Create: `keel-controlplane/src/scheduler.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Consumes: nothing (pure function, `std::collections::HashMap` only).
- Produces: `ScheduleError::NoAvailableNodes` (`#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]`), `pick_node(alive_ids: &[String], counts: &HashMap<&str, usize>) -> Result<String, ScheduleError>`.

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/scheduler.rs`:

```rust
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no alive nodes available to schedule onto")]
    NoAvailableNodes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_alive_nodes_returns_no_available_nodes_error() {
        let alive_ids: Vec<String> = vec![];
        let counts = HashMap::new();
        assert_eq!(pick_node(&alive_ids, &counts), Err(ScheduleError::NoAvailableNodes));
    }

    #[test]
    fn a_single_alive_node_is_always_picked() {
        let alive_ids = vec!["node-1".to_string()];
        let counts = HashMap::new();
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-1".to_string()));
    }

    #[test]
    fn the_node_with_the_fewest_recorded_jails_wins() {
        let alive_ids = vec!["node-1".to_string(), "node-2".to_string()];
        let mut counts = HashMap::new();
        counts.insert("node-1", 3);
        counts.insert("node-2", 1);
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-2".to_string()));
    }

    #[test]
    fn ties_are_broken_by_ascending_node_id() {
        let alive_ids = vec!["node-2".to_string(), "node-1".to_string()];
        let counts = HashMap::new();
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-1".to_string()));
    }

    #[test]
    fn a_dead_node_with_a_lower_count_is_never_picked_since_it_is_absent_from_alive_ids() {
        let alive_ids = vec!["node-2".to_string()];
        let mut counts = HashMap::new();
        counts.insert("node-1", 0);
        counts.insert("node-2", 5);
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-2".to_string()));
    }
}
```

Modify `keel-controlplane/src/lib.rs`:

```rust
pub mod http;
pub mod placements;
pub mod registry;
pub mod scheduler;
pub mod wire;
pub mod worker;

pub use placements::Placements;
pub use registry::Registry;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane scheduler`
Expected: FAIL to compile — `pick_node` is not yet defined.

- [ ] **Step 3: Implement `pick_node`**

In `keel-controlplane/src/scheduler.rs`, add after the `ScheduleError` definition (before `#[cfg(test)]`):

```rust
pub fn pick_node(alive_ids: &[String], counts: &HashMap<&str, usize>) -> Result<String, ScheduleError> {
    alive_ids
        .iter()
        .min_by_key(|id| (counts.get(id.as_str()).copied().unwrap_or(0), (*id).clone()))
        .cloned()
        .ok_or(ScheduleError::NoAvailableNodes)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 42 tests pass (37 from Task 1, 5 new in `scheduler`).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/scheduler.rs keel-controlplane/src/lib.rs
git commit -m "Add keel-controlplane's scheduler::pick_node: least-loaded, ties by ascending id"
```

---

### Task 3: `worker.rs` — scheduling/placement commands and the new `spawn` signature

**Files:**
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `Placements::{get, set, remove, counts}` (Task 1), `scheduler::pick_node`, `ScheduleError` (Task 2), `Registry::resolve`, `ResolveError` (existing), `NodeState` (existing, `keel-controlplane/src/wire.rs`).
- Produces: `ScheduleOrResolveError::{Schedule(ScheduleError), Resolve(ResolveError)}`, `PlacementError::{NotPlaced(String), Resolve(ResolveError)}` (both `#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]`), `Command::{ResolveOrSchedule(String, Sender<Result<(String, String), ScheduleOrResolveError>>), ResolvePlacement(String, Sender<Result<(String, String), PlacementError>>), RecordPlacement(String, String, Sender<()>), RemovePlacement(String, Sender<()>)}`, and the changed `pub fn spawn(mut registry: Registry, mut placements: Placements) -> (JoinHandle<()>, Sender<Command>)`.

- [ ] **Step 1: Write the failing tests (and update existing call sites to the new `spawn` signature)**

Replace the top of `keel-controlplane/src/worker.rs` (imports through the `Command` enum) with:

```rust
use crate::placements::Placements;
use crate::registry::{Registry, ResolveError, UnknownNode};
use crate::scheduler::{self, ScheduleError};
use crate::wire::{NodeState, NodeStatus};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleOrResolveError {
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("no known placement for jail '{0}'")]
    NotPlaced(String),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

pub enum Command {
    Register(String, String, Sender<()>),
    Heartbeat(String, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
    ResolveOrSchedule(String, Sender<Result<(String, String), ScheduleOrResolveError>>),
    ResolvePlacement(String, Sender<Result<(String, String), PlacementError>>),
    RecordPlacement(String, String, Sender<()>),
    RemovePlacement(String, Sender<()>),
}

pub fn spawn(mut registry: Registry, mut placements: Placements) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, &mut placements, command);
        }
    });
    (handle, tx)
}
```

In the same file, update every existing test's `spawn(Registry::new())` call to `spawn(Registry::new(), Placements::new())` — there are six occurrences, one in each of: `register_command_makes_the_node_visible_in_list`, `heartbeat_command_on_unknown_id_returns_an_error`, `heartbeat_command_on_a_registered_node_succeeds`, `list_command_on_a_fresh_worker_is_empty`, `resolve_command_on_a_registered_alive_node_returns_its_address`, `resolve_command_on_an_unknown_node_returns_an_error`.

Then add, at the end of the `#[cfg(test)] mod tests` block:

```rust
    fn register_node(commands: &Sender<Command>, id: &str, addr: &str) {
        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register(id.to_string(), addr.to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();
    }

    #[test]
    fn resolve_or_schedule_on_a_fresh_jail_name_schedules_onto_the_least_loaded_alive_node() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1");
        register_node(&commands, "node-2", "10.0.0.2");

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("existing".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-2".to_string(), "10.0.0.2".to_string())));
    }

    #[test]
    fn resolve_or_schedule_on_an_already_placed_jail_is_sticky() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1");
        register_node(&commands, "node-2", "10.0.0.2");

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-1".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-1".to_string(), "10.0.0.1".to_string())));
    }

    #[test]
    fn resolve_or_schedule_with_no_alive_nodes_returns_no_available_nodes() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(ScheduleOrResolveError::Schedule(ScheduleError::NoAvailableNodes)));
    }

    #[test]
    fn resolve_placement_on_an_unplaced_jail_returns_not_placed() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(PlacementError::NotPlaced("web-1".to_string())));
    }

    #[test]
    fn record_then_remove_placement_is_reflected_by_resolve_placement() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1");

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-1".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-1".to_string(), "10.0.0.1".to_string())));

        let (rem_tx, rem_rx) = mpsc::channel();
        commands.send(Command::RemovePlacement("web-1".to_string(), rem_tx)).unwrap();
        rem_rx.recv().unwrap();

        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Err(PlacementError::NotPlaced("web-1".to_string())));
    }
```

Modify `keel-controlplane/src/main.rs`:

```rust
use keel_controlplane::placements::Placements;
use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;
```

and its `main()` body's worker line:

```rust
    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo build -p keel-controlplane`
Expected: FAIL to compile — `handle_command` doesn't yet accept a `&mut Placements` parameter or handle the four new `Command` variants, and `Command`'s new variants aren't yet consumed.

- [ ] **Step 3: Implement `handle_command`'s new arms**

Replace `keel-controlplane/src/worker.rs`'s `handle_command` function with:

```rust
fn handle_command(registry: &mut Registry, placements: &mut Placements, command: Command) {
    match command {
        Command::Register(id, addr, reply) => {
            registry.register(id, addr, Instant::now());
            let _ = reply.send(());
        }
        Command::Heartbeat(id, reply) => {
            let result = registry.heartbeat(&id, Instant::now());
            let _ = reply.send(result);
        }
        Command::List(reply) => {
            let _ = reply.send(registry.list(Instant::now()));
        }
        Command::Resolve(id, reply) => {
            let result = registry.resolve(&id, Instant::now());
            let _ = reply.send(result);
        }
        Command::ResolveOrSchedule(jail_name, reply) => {
            let now = Instant::now();
            let result = if let Some(node_id) = placements.get(&jail_name).map(|s| s.to_string()) {
                registry.resolve(&node_id, now).map(|addr| (node_id, addr)).map_err(ScheduleOrResolveError::from)
            } else {
                let alive_ids: Vec<String> = registry
                    .list(now)
                    .into_iter()
                    .filter(|status| status.status == NodeState::Alive)
                    .map(|status| status.id)
                    .collect();
                let counts = placements.counts();
                scheduler::pick_node(&alive_ids, &counts).map_err(ScheduleOrResolveError::from).and_then(
                    |node_id| {
                        registry
                            .resolve(&node_id, now)
                            .map(|addr| (node_id, addr))
                            .map_err(ScheduleOrResolveError::from)
                    },
                )
            };
            let _ = reply.send(result);
        }
        Command::ResolvePlacement(jail_name, reply) => {
            let result = match placements.get(&jail_name).map(|s| s.to_string()) {
                None => Err(PlacementError::NotPlaced(jail_name)),
                Some(node_id) => registry
                    .resolve(&node_id, Instant::now())
                    .map(|addr| (node_id, addr))
                    .map_err(PlacementError::from),
            };
            let _ = reply.send(result);
        }
        Command::RecordPlacement(jail_name, node_id, reply) => {
            placements.set(jail_name, node_id);
            let _ = reply.send(());
        }
        Command::RemovePlacement(jail_name, reply) => {
            placements.remove(&jail_name);
            let _ = reply.send(());
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo build -p keel-controlplane && cargo test -p keel-controlplane`
Expected: build succeeds; all 47 tests pass (42 from Task 2, 5 new in `worker`).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/main.rs
git commit -m "Add keel-controlplane's ResolveOrSchedule/ResolvePlacement/RecordPlacement/RemovePlacement commands"
```

---

### Task 4: `http.rs` — the scheduled `/jails/{name}` routes and shared bookkeeping

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `Command::{ResolveOrSchedule, ResolvePlacement, RecordPlacement, RemovePlacement}`, `ScheduleOrResolveError`, `PlacementError` (Task 3), `Placements` (Task 1), existing `forward`, `error_response`, `reason_phrase`.
- Produces: `handle_scheduled_apply`, `handle_scheduled_read`, `handle_scheduled_delete`, `resolve_placement`, `send_record_placement`, `send_remove_placement` (all private free functions), three new route arms (`PUT`/`GET`/`DELETE ["jails", name]`), bookkeeping added to the existing `("PUT", ["nodes", id, "jails", name])` and `("DELETE", ["nodes", id, "jails", name])` arms, and a new `503` entry in `reason_phrase`.

- [ ] **Step 1: Write the failing tests**

Modify `keel-controlplane/src/http.rs`'s top import line:

```rust
use crate::wire::{ErrorBody, NodeRegistration};
use crate::worker::{Command, PlacementError, ScheduleOrResolveError};
```

In the `#[cfg(test)] mod tests` block, change:

```rust
    use crate::registry::Registry;
    use crate::worker;
```

to:

```rust
    use crate::placements::Placements;
    use crate::registry::Registry;
    use crate::worker;
```

and change `start_test_server`'s worker line from `worker::spawn(Registry::new())` to `worker::spawn(Registry::new(), Placements::new())`.

Add these tests to the end of the `mod tests` block:

```rust
    #[test]
    fn scheduled_put_lands_on_the_lower_id_node_when_counts_are_equal() {
        let cp_addr = start_test_server();
        let node_a_addr = start_fake_remote_agentd(200, "node: node-a\n");
        let node_b_addr = start_fake_remote_agentd(200, "node: node-b\n");
        register_node(&cp_addr, "node-b", &node_b_addr);
        register_node(&cp_addr, "node-a", &node_a_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("node-a"), "expected the lower id (node-a) to win the tie, got: {body}");
    }

    #[test]
    fn scheduled_put_is_sticky_across_repeated_apply() {
        let cp_addr = start_test_server();
        let node_a_addr = start_fake_remote_agentd(200, "node: node-a\n");
        register_node(&cp_addr, "node-a", &node_a_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("node-a"));

        // node-0 joins with a lower id and zero recorded jails, and would win
        // a fresh scheduling decision -- but web-1 is already placed, so it
        // must stay put.
        let node_0_addr = start_fake_remote_agentd(200, "node: node-0\n");
        register_node(&cp_addr, "node-0", &node_0_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("node-a"), "expected sticky placement on node-a, got: {body}");
    }

    #[test]
    fn scheduled_get_and_delete_on_an_unplaced_jail_return_404() {
        let cp_addr = start_test_server();

        let (status, body) = send_request(&cp_addr, "GET", "/jails/missing", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");

        let (status, body) = send_request(&cp_addr, "DELETE", "/jails/missing", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");
    }

    #[test]
    fn scheduled_delete_removes_the_placement_so_a_later_get_returns_404() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "node: node-a\n");
        register_node(&cp_addr, "node-a", &node_addr);

        send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        let (status, _) = send_request(&cp_addr, "DELETE", "/jails/web-1", "");
        assert_eq!(status, 200);

        let (status, body) = send_request(&cp_addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");
    }

    #[test]
    fn scheduled_put_with_no_alive_nodes_returns_503() {
        let cp_addr = start_test_server();
        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 503);
        assert!(body.contains("no alive nodes"), "got: {body}");
    }

    #[test]
    fn named_route_apply_and_scheduled_route_share_the_same_placement_table() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, _) = send_request(&cp_addr, "PUT", "/nodes/node-1/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);

        let (status, body) = send_request(&cp_addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 200, "expected the scheduled GET to find the placement recorded by the named-node PUT");
        assert!(body.contains("running: true"), "got: {body}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo build -p keel-controlplane`
Expected: FAIL to compile — `route()` has no arms for `["jails", name]`, so the new tests' requests hit the `_ => error_response(404, ...)` fallback instead of scheduling, and `PlacementError`/`ScheduleOrResolveError` are unused-import errors until `route()` references them.

- [ ] **Step 3: Implement the scheduled routes and bookkeeping**

In `keel-controlplane/src/http.rs`, replace the `route()` function's match arms with:

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, commands),
        ("GET", ["nodes"]) => handle_list(commands),
        ("PUT", ["nodes", id, "jails", name]) => {
            let (status, body) = handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands);
            if (200..300).contains(&status) {
                send_record_placement(name, id, commands);
            }
            (status, body)
        }
        ("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands),
        ("GET", ["nodes", id, "jails", name]) => {
            handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands)
        }
        ("DELETE", ["nodes", id, "jails", name]) => {
            let (status, body) = handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands);
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, body)
        }
        ("PUT", ["jails", name]) => handle_scheduled_apply(name, &request.body, commands),
        ("GET", ["jails", name]) => handle_scheduled_read(name, commands),
        ("DELETE", ["jails", name]) => handle_scheduled_delete(name, commands),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}

fn handle_scheduled_apply(name: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ResolveOrSchedule(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let (node_id, addr) = match reply_rx.recv() {
        Ok(Ok(pair)) => pair,
        Ok(Err(ScheduleOrResolveError::Schedule(e))) => return error_response(503, e.to_string()),
        Ok(Err(ScheduleOrResolveError::Resolve(e))) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, "PUT", &format!("/jails/{name}"), body) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_record_placement(name, &node_id, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_read(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "GET", &format!("/jails/{name}"), &[]) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_delete(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "DELETE", &format!("/jails/{name}"), &[]) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn resolve_placement(name: &str, commands: &Sender<Command>) -> Result<(String, String), (u16, Vec<u8>)> {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ResolvePlacement(name.to_string(), reply_tx)).is_err() {
        return Err(error_response(500, "control plane worker is not running".to_string()));
    }
    match reply_rx.recv() {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(e)) => Err(error_response(404, e.to_string())),
        Err(_) => Err(error_response(500, "control plane worker did not respond".to_string())),
    }
}

fn send_record_placement(name: &str, node_id: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RecordPlacement(name.to_string(), node_id.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}

fn send_remove_placement(name: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RemovePlacement(name.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}
```

Update `reason_phrase`:

```rust
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 53 tests pass (47 from Task 3, 6 new in `http`).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Add keel-controlplane's PUT/GET/DELETE /jails/{name} scheduled routes"
```

---

### Task 5: `keelctl` — `--node` becomes optional

**Files:**
- Modify: `keelctl/src/main.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `keel_controlplane::worker::spawn` (now `(Registry, Placements)`, Task 3), `keel_controlplane::Placements` (Task 1).
- Produces: `Target::ControlPlane { addr: String, node: Option<String> }` (was `node: String`), `jails_path`'s new `node: None` arm, `run_keelctl_scheduled` test helper.

- [ ] **Step 1: Write the failing tests**

Modify `keelctl/tests/cli.rs`'s `start_test_control_plane_with_node` function body's worker line, from:

```rust
    let (_worker_handle, commands) = keel_controlplane::worker::spawn(keel_controlplane::Registry::new());
```

to:

```rust
    let (_worker_handle, commands) =
        keel_controlplane::worker::spawn(keel_controlplane::Registry::new(), keel_controlplane::Placements::new());
```

Add, after `run_keelctl_routed`:

```rust
fn run_keelctl_scheduled(control_plane_addr: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(args)
        .arg("--control-plane-addr")
        .arg(control_plane_addr)
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}
```

Replace the existing `control_plane_addr_without_node_is_a_usage_error` test (its behavior is changing: `--control-plane-addr` alone is now valid, not an error) with:

```rust
#[test]
fn control_plane_addr_without_node_schedules_through_the_control_plane() {
    let node_addr = start_test_agentd_tcp("scheduled_round_trip");
    let control_plane_addr = start_test_control_plane_with_node("node-1", &node_addr);
    let spec_path = write_spec_file("scheduled_round_trip", "web-1");

    let (ok, _, stderr) =
        run_keelctl_scheduled(&control_plane_addr, &["apply", "-f", spec_path.to_str().unwrap()]);
    assert!(ok, "apply failed: {stderr}");

    let (ok, stdout, stderr) = run_keelctl_scheduled(&control_plane_addr, &["get", "web-1"]);
    assert!(ok, "get failed: {stderr}");
    assert!(stdout.contains("running: true"), "expected running: true, got: {stdout}");

    let (ok, _, stderr) = run_keelctl_scheduled(&control_plane_addr, &["delete", "web-1"]);
    assert!(ok, "delete failed: {stderr}");
}
```

Update `node_without_control_plane_addr_is_a_usage_error`'s assertion (the error message text is changing) from:

```rust
    assert!(
        stderr.contains("--control-plane-addr and --node must be given together"),
        "got: {stderr}"
    );
```

to:

```rust
    assert!(stderr.contains("--node requires --control-plane-addr"), "got: {stderr}");
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keelctl`
Expected: FAIL to compile — `keel_controlplane::worker::spawn` still takes one argument (Task 3 already changed the real signature, so this is actually the *last* remaining call site; expect a compile error here, not a runtime failure), and `Target::ControlPlane`'s `node` field is still a required `String`, not `Option<String>`.

- [ ] **Step 3: Update `keelctl`'s `Target` and argument parsing**

In `keelctl/src/main.rs`, replace:

```rust
enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: String },
}
```

with:

```rust
enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: Option<String> },
}
```

Replace the target-selection block in `main()`:

```rust
    let target = match (control_plane_addr, node) {
        (Some(addr), Some(node)) => Target::ControlPlane { addr, node },
        (None, None) => Target::Socket(socket),
        _ => {
            eprintln!("error: --control-plane-addr and --node must be given together");
            return ExitCode::FAILURE;
        }
    };
```

with:

```rust
    let target = match (control_plane_addr, node) {
        (Some(addr), node) => Target::ControlPlane { addr, node },
        (None, Some(_)) => {
            eprintln!("error: --node requires --control-plane-addr");
            return ExitCode::FAILURE;
        }
        (None, None) => Target::Socket(socket),
    };
```

Replace `jails_path`:

```rust
fn jails_path(target: &Target, suffix: &str) -> String {
    match target {
        Target::Socket(_) => suffix.to_string(),
        Target::ControlPlane { node: Some(node), .. } => format!("/nodes/{node}{suffix}"),
        Target::ControlPlane { node: None, .. } => suffix.to_string(),
    }
}
```

`dispatch`, `run_apply`, `run_get`, `run_delete`, `send_request`, `send_request_tcp`, and `parse_response` are all unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo build --workspace && cargo test --workspace`
Expected: the whole workspace builds; all 161 tests pass (139 baseline + 22 new in `keel-controlplane` across Tasks 1-4: 6 + 5 + 5 + 6; `keelctl`'s own count stays at 7 — one test was replaced 1-for-1, not added).

- [ ] **Step 5: Commit**

```bash
git add keelctl/src/main.rs keelctl/tests/cli.rs
git commit -m "Add keelctl --control-plane-addr without --node: schedule instead of naming a node"
```

---

### Task 6: FreeBSD VM sanity check

**Files:** none (verification only, run by the coordinating session directly, not a subagent)

- [ ] **Step 1: Build and deploy the updated binaries to the three VMs**

On the coordinating session's machine, build for the VM target (matching the process used in every prior milestone's VM task) and copy `keel-controlplane` to `.2` and `keelctl` to all three VMs. `keel-agentd` is unchanged in this milestone, so it does not need redeploying, but confirm the already-running `keel-agentd` on `.4` and `.5` is still the Milestone 8 build (no restart needed).

- [ ] **Step 2: Start `keel-controlplane` on `.2`**

```bash
ssh root@192.168.64.2 'keel-controlplane --addr 0.0.0.0:7620 &'
```

Confirm nodes `.4` and `.5` are already registered and `Alive` (they should already be running with their Milestone 7/8 `--control-plane-addr`/`--node-id` flags from prior milestones):

```bash
ssh root@192.168.64.2 'keelctl --socket /nonexistent --control-plane-addr 127.0.0.1:7620 --node dummy get 2>&1 || curl -s http://127.0.0.1:7620/nodes'
```

Expected: both nodes listed as `Alive`.

- [ ] **Step 3: Apply two differently-named specs through the scheduler and confirm they land on different nodes**

From `.2` (or any machine that can reach `.2:7620`):

```bash
keelctl --control-plane-addr 192.168.64.2:7620 apply -f spec-a.yaml   # metadata.name: sched-a
keelctl --control-plane-addr 192.168.64.2:7620 apply -f spec-b.yaml   # metadata.name: sched-b
```

Confirm on `.4` and `.5` directly (via `jls`, same check prior milestones used) that `sched-a` and `sched-b` exist on two *different* nodes, not both on the same one.

- [ ] **Step 4: Confirm sticky re-apply**

```bash
keelctl --control-plane-addr 192.168.64.2:7620 apply -f spec-a.yaml   # re-apply, unchanged
```

Confirm `sched-a` is still only on the node it originally landed on (no duplicate created on the other node).

- [ ] **Step 5: Clean up and confirm named-node routing (Milestone 8) still works unaffected**

```bash
keelctl --control-plane-addr 192.168.64.2:7620 delete sched-a
keelctl --control-plane-addr 192.168.64.2:7620 delete sched-b
keelctl --control-plane-addr 192.168.64.2:7620 --node node-1 apply -f spec-a.yaml
```

Confirm the explicit `--node` form still pins to exactly the named node, byte-for-byte as in Milestone 8, then delete it to leave the VMs clean.

- [ ] **Step 6: Record the outcome**

No code changes result from this task if everything behaves as designed. If any step surfaces a real bug (unlikely, since this milestone touches no FreeBSD-specific code), stop and treat it as a new task inserted before the final commit, following the same TDD steps as Tasks 1-5.

---

## Final Review

Once Tasks 1-6 are complete, do a whole-branch review (same discipline as every prior milestone): re-run `cargo test --workspace` and confirm the final count (53 `keel-controlplane` + 65 `keel-agentd` + 7 `keelctl` + 36 untouched in `keel-spec`/`keel-jail`/`keel-zfs`/`keel-net` = 139 baseline + 22 new = 161 passed), then update `README.md`'s roadmap (mark item 9 done, add the Milestone 9 write-up) and the website pages, mirroring exactly what was done for Milestone 8's doc update.
