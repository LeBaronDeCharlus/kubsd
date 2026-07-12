# keel Milestone 7: Node Registry (Sub-Project 2, First Milestone) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 9:** it needs the real FreeBSD VMs (`root@192.168.64.2`, `.4`, `.5`). The coordinating session has direct SSH access to these VMs and should run this task itself rather than dispatching a subagent for it — this mirrors Milestone 5's Task 8 and Milestone 6's Task 4. **Tasks 1-8 are pure file edits, verified locally (macOS) via `cargo test`, and need no FreeBSD VM interaction at all.**

**Goal:** Stand up `keel-controlplane`, a new crate tracking cluster membership (which nodes exist and are alive) via a register/heartbeat HTTP API over TCP, and make `keel-agentd` optionally register itself and heartbeat to one, entirely opt-in and with zero effect on any node that doesn't configure it.

**Architecture:** `keel-controlplane` mirrors `keel-agentd`'s existing shape exactly: a hand-rolled `httparse`-based HTTP server (over `TcpListener` instead of a Unix socket, since nodes are on separate hosts) in front of a single worker thread that exclusively owns an in-memory `Registry` — same "one thread owns the state, handlers reach it only through a channel" pattern as `keel-agentd`'s `Reconciler`/`worker.rs`. `keel-agentd` gains three new optional CLI flags and a `registration.rs` module: a background thread that registers once at startup and heartbeats on a timer, treating any heartbeat rejection (control plane restarted and forgot it) the same as a connection failure — just re-register. No persistence anywhere in this milestone; self-healing via re-registration is the whole strategy.

**Tech Stack:** Rust (2021 edition), `serde`/`serde_yaml` (wire format, matching every existing API in this workspace), `httparse` (existing dependency, same hand-rolled HTTP/1.1 parsing already used by `keel-agentd`/`keelctl`), no new dependencies anywhere.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-10-keel-agent-milestone7-node-registry-design.md` (Approved). Endpoint shapes, timing constants, and the self-healing re-registration behavior described there must match exactly.
- **No new crate dependencies anywhere.** `keel-controlplane` uses only `serde`, `serde_yaml`, `thiserror`, `httparse` — the same four already used by `keel-agentd`. No HTTP client crate (`reqwest`, `ureq`); all outbound calls in `keel-agentd`'s `registration.rs` are hand-rolled `TcpStream` requests, the exact pattern `keelctl::send_request` already uses over a `UnixStream`.
- **`keel-agentd`'s existing behavior is unchanged when `--control-plane-addr` is absent.** This is the most important regression constraint in this plan: Milestones 1-6's tests and the existing smoke test must keep passing with zero modification, since none of them pass the new flags.
- **One deliberate deviation from the design spec's exact pseudocode, for testability:** the design spec's `registration.rs` loop hardcodes a 5s `HEARTBEAT_INTERVAL`. This plan instead makes it a parameter (`registration::spawn(..., heartbeat_interval: Duration)`), with `main.rs` passing `Duration::from_secs(5)` — functionally identical in production, but lets Task 7's tests use a tiny interval (tens of milliseconds) instead of waiting multiple real seconds per test. `DEAD_THRESHOLD` in `Registry` does **not** need this treatment: `Registry::list` already takes an explicit `now: Instant` parameter (matching `BackoffState`'s existing pattern), so its tests inject arbitrary instants with no real sleeping at all.
- Every new public type, function, and constant introduced by one task and used by a later task is named exactly as given in that task's **Produces** list — later tasks must match these names exactly.
- No placeholders: every task's deliverable is verified with `cargo build -p <crate> && cargo test -p <crate>` (or `--workspace` where noted) before its commit step.
- **A second, empirically-verified deviation from the design spec's Testing Strategy wording:** the design spec describes an in-process test that drops a `TcpListener` and rebinds a fresh one to the same address to simulate a control-plane restart. This is not actually possible with `std::net::TcpListener`: a socket bound and actively `accept`-ing (as `keel-controlplane`'s permanently-looping `http::run` does) cannot be rebound by a second listener on the same address while the first is still alive — confirmed directly (`TcpListener::bind` on the second attempt returns "Address already in use", os error 48, even with the first listener's owning thread still running). Fixing this would require adding a shutdown mechanism to `http::run`, which is scope beyond this milestone's design. Task 7 therefore tests only the straightforward register-then-heartbeat path in-process; the actual "control plane restarts, nodes re-register" behavior is verified for real in Task 9 (Step 5), where `keel-controlplane` is a genuine separate OS process — `pkill` there fully closes its listening socket, so the fresh process's `bind` is not fighting a still-open predecessor the way an in-process test would.

---

### Task 1: `keel-controlplane` crate scaffold + wire types

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `keel-controlplane/Cargo.toml`
- Create: `keel-controlplane/src/lib.rs`
- Create: `keel-controlplane/src/wire.rs`

**Interfaces:**
- Produces: `NodeRegistration { pub id: String, pub addr: String }`, `NodeState::{Alive, Dead}`, `NodeStatus { pub id: String, pub addr: String, pub status: NodeState, pub last_seen_secs: u64 }`, `ErrorBody { pub error: String }` — all `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]` (`NodeState` additionally `Copy`), re-exported at the crate root as `keel_controlplane::{NodeRegistration, NodeState, NodeStatus, ErrorBody}`.

- [ ] **Step 1: Add the new crate to the workspace**

Modify `Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = ["keel-spec", "keel-jail", "keel-zfs", "keel-net", "keel-agentd", "keelctl", "keel-controlplane"]
```

- [ ] **Step 2: Create the crate manifest**

Create `keel-controlplane/Cargo.toml`:

```toml
[package]
name = "keel-controlplane"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
thiserror = "1"
httparse = "1"
```

- [ ] **Step 3: Write the wire types with round-trip tests**

Create `keel-controlplane/src/wire.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub id: String,
    pub addr: String,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_registration_round_trips_through_yaml() {
        let registration = NodeRegistration { id: "node-1".to_string(), addr: "192.168.64.4".to_string() };
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
        };
        let yaml = serde_yaml::to_string(&status).unwrap();
        let parsed: NodeStatus = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn node_state_dead_round_trips_through_yaml() {
        let yaml = serde_yaml::to_string(&NodeState::Dead).unwrap();
        let parsed: NodeState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, NodeState::Dead);
    }

    #[test]
    fn error_body_round_trips_through_yaml() {
        let body = ErrorBody { error: "unknown node 'node-9'".to_string() };
        let yaml = serde_yaml::to_string(&body).unwrap();
        let parsed: ErrorBody = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, body);
    }
}
```

- [ ] **Step 4: Create `lib.rs`**

Create `keel-controlplane/src/lib.rs`:

```rust
pub mod wire;

pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p keel-controlplane`
Expected: 4 tests pass (`node_registration_round_trips_through_yaml`, `node_status_round_trips_through_yaml`, `node_state_dead_round_trips_through_yaml`, `error_body_round_trips_through_yaml`).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml keel-controlplane/Cargo.toml keel-controlplane/src/lib.rs keel-controlplane/src/wire.rs
git commit -m "Scaffold keel-controlplane crate with its wire types"
```

---

### Task 2: `Registry` — register, heartbeat, list

**Files:**
- Create: `keel-controlplane/src/registry.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Consumes: `NodeState`, `NodeStatus` (from Task 1's `wire.rs`).
- Produces: `Registry` with `Registry::new() -> Self`, `Registry::register(&mut self, id: String, addr: String, now: Instant)`, `Registry::heartbeat(&mut self, id: &str, now: Instant) -> Result<(), UnknownNode>`, `Registry::list(&self, now: Instant) -> Vec<NodeStatus>` (sorted by `id`, ascending). `UnknownNode(pub String)`, a `thiserror::Error` whose `Display` is `"unknown node '{0}'"`.

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/registry.rs`:

```rust
use crate::wire::{NodeState, NodeStatus};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEAD_THRESHOLD: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct NodeRecord {
    addr: String,
    last_heartbeat: Instant,
}

#[derive(Debug, Default)]
pub struct Registry {
    nodes: HashMap<String, NodeRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown node '{0}'")]
pub struct UnknownNode(pub String);

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_list_shows_the_node_as_alive() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-1".to_string(), "192.168.64.4".to_string(), now);

        let statuses = registry.list(now);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-1");
        assert_eq!(statuses[0].addr, "192.168.64.4");
        assert_eq!(statuses[0].status, NodeState::Alive);
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn reregistering_an_existing_id_refreshes_its_address_and_heartbeat() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let t1 = t0 + Duration::from_secs(5);
        registry.register("node-1".to_string(), "10.0.0.2".to_string(), t1);

        let statuses = registry.list(t1);
        assert_eq!(statuses.len(), 1, "re-registering must not create a second entry");
        assert_eq!(statuses[0].addr, "10.0.0.2");
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn heartbeat_on_a_known_node_updates_its_last_heartbeat() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let t1 = t0 + Duration::from_secs(10);
        registry.heartbeat("node-1", t1).unwrap();

        let statuses = registry.list(t1);
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn heartbeat_on_an_unknown_node_returns_unknown_node_error() {
        let mut registry = Registry::new();
        let err = registry.heartbeat("missing", Instant::now()).unwrap_err();
        assert_eq!(err, UnknownNode("missing".to_string()));
        assert_eq!(err.to_string(), "unknown node 'missing'");
    }

    #[test]
    fn list_reports_dead_once_a_node_exceeds_the_dead_threshold() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let just_under = t0 + Duration::from_secs(14);
        assert_eq!(registry.list(just_under)[0].status, NodeState::Alive);

        let at_threshold = t0 + DEAD_THRESHOLD;
        assert_eq!(registry.list(at_threshold)[0].status, NodeState::Dead);
    }

    #[test]
    fn list_is_sorted_by_id() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-2".to_string(), "10.0.0.2".to_string(), now);
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), now);

        let statuses = registry.list(now);
        assert_eq!(statuses.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["node-1", "node-2"]);
    }

    #[test]
    fn list_on_an_empty_registry_is_empty() {
        let registry = Registry::new();
        assert_eq!(registry.list(Instant::now()), vec![]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane registry`
Expected: FAIL to compile — `register`, `heartbeat`, and `list` are not yet defined on `Registry`.

- [ ] **Step 3: Implement `register`/`heartbeat`/`list`**

In `keel-controlplane/src/registry.rs`, add to the `impl Registry` block (after `pub fn new`):

```rust
    pub fn register(&mut self, id: String, addr: String, now: Instant) {
        self.nodes.insert(id, NodeRecord { addr, last_heartbeat: now });
    }

    pub fn heartbeat(&mut self, id: &str, now: Instant) -> Result<(), UnknownNode> {
        match self.nodes.get_mut(id) {
            Some(record) => {
                record.last_heartbeat = now;
                Ok(())
            }
            None => Err(UnknownNode(id.to_string())),
        }
    }

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
                }
            })
            .collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }
```

- [ ] **Step 4: Declare the module**

Modify `keel-controlplane/src/lib.rs`:

```rust
pub mod registry;
pub mod wire;

pub use registry::Registry;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 11 tests pass (4 from Task 1's `wire.rs`, 7 new in `registry.rs`).

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/registry.rs keel-controlplane/src/lib.rs
git commit -m "Add Registry: register/heartbeat/list with computed Alive/Dead status"
```

---

### Task 3: `worker.rs` — single-owner thread over `Registry`

**Files:**
- Create: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Consumes: `Registry` (Task 2), `UnknownNode` (Task 2), `NodeStatus` (Task 1).
- Produces: `Command::{Register(String, String, Sender<()>), Heartbeat(String, Sender<Result<(), UnknownNode>>), List(Sender<Vec<NodeStatus>>)}`, `worker::spawn(registry: Registry) -> (JoinHandle<()>, Sender<Command>)`.

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/worker.rs`:

```rust
use crate::registry::{Registry, UnknownNode};
use crate::wire::NodeStatus;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub enum Command {
    Register(String, String, Sender<()>),
    Heartbeat(String, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_command_makes_the_node_visible_in_list() {
        let commands = spawn(Registry::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        let statuses = list_rx.recv().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-1");
    }

    #[test]
    fn heartbeat_command_on_unknown_id_returns_an_error() {
        let commands = spawn(Registry::new()).1;

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("missing".to_string(), hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_err());
    }

    #[test]
    fn heartbeat_command_on_a_registered_node_succeeds() {
        let commands = spawn(Registry::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("node-1".to_string(), hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_ok());
    }

    #[test]
    fn list_command_on_a_fresh_worker_is_empty() {
        let commands = spawn(Registry::new()).1;

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        assert_eq!(list_rx.recv().unwrap(), vec![]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane worker`
Expected: FAIL to compile — `spawn` is not yet defined.

- [ ] **Step 3: Implement `spawn` and command handling**

In `keel-controlplane/src/worker.rs`, add after the `Command` enum definition (before the `#[cfg(test)]` module):

```rust
pub fn spawn(mut registry: Registry) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, command);
        }
    });
    (handle, tx)
}

fn handle_command(registry: &mut Registry, command: Command) {
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
    }
}
```

- [ ] **Step 4: Declare the module**

Modify `keel-controlplane/src/lib.rs`:

```rust
pub mod registry;
pub mod wire;
pub mod worker;

pub use registry::Registry;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 15 tests pass (11 from Tasks 1-2, 4 new in `worker.rs`).

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/lib.rs
git commit -m "Add worker::spawn: single thread exclusively owning the Registry"
```

---

### Task 4: `http.rs` — TCP HTTP server and routing

**Files:**
- Create: `keel-controlplane/src/http.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Consumes: `Command` (Task 3), `NodeRegistration`/`ErrorBody` (Task 1).
- Produces: `http::run(listener: TcpListener, commands: Sender<Command>)`. Routes: `POST /nodes/register` (YAML body `NodeRegistration`) → 200; `POST /nodes/:id/heartbeat` → 200 or 404; `GET /nodes` → 200 with a YAML `Vec<NodeStatus>` body.

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/http.rs`:

```rust
use crate::wire::{ErrorBody, NodeRegistration};
use crate::worker::Command;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Sender};
use std::thread;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

pub fn run(listener: TcpListener, commands: Sender<Command>) {
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

fn handle_connection(mut stream: TcpStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response(&mut stream, status, &body)
}

fn read_request(stream: &mut TcpStream) -> io::Result<Option<ParsedRequest>> {
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
    if total_len > MAX_MESSAGE_BYTES {
        return Ok(None);
    }
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

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
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
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, commands),
        ("GET", ["nodes"]) => handle_list(commands),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
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
    use crate::registry::Registry;
    use crate::worker;

    fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new());
        thread::spawn(move || run(listener, commands));
        addr
    }

    fn send_request(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).unwrap();
        if let Err(e) = stream.shutdown(std::net::Shutdown::Write) {
            assert_eq!(e.kind(), std::io::ErrorKind::NotConnected, "unexpected shutdown error: {e}");
        }

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
    fn register_returns_200_and_the_node_appears_in_get_nodes() {
        let addr = start_test_server();
        let (status, _) = send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1\n");
        assert_eq!(status, 200);

        let (status, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert!(body.contains("node-1"), "expected node-1 in body: {body}");
        assert!(body.contains("Alive"), "expected Alive status in body: {body}");
    }

    #[test]
    fn reregistering_the_same_id_updates_its_address_without_duplicating() {
        let addr = start_test_server();
        send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1\n");
        send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.2\n");

        let (_, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(body.matches("node-1").count(), 1, "expected exactly one node-1 entry, got body: {body}");
        assert!(body.contains("10.0.0.2"), "expected refreshed address in body: {body}");
    }

    #[test]
    fn heartbeat_on_a_registered_node_returns_200() {
        let addr = start_test_server();
        send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1\n");

        let (status, _) = send_request(&addr, "POST", "/nodes/node-1/heartbeat", "");
        assert_eq!(status, 200);
    }

    #[test]
    fn heartbeat_on_an_unknown_node_returns_404() {
        let addr = start_test_server();
        let (status, body) = send_request(&addr, "POST", "/nodes/missing/heartbeat", "");
        assert_eq!(status, 404);
        assert!(body.contains("missing"));
    }

    #[test]
    fn get_nodes_on_an_empty_registry_returns_200_with_an_empty_list() {
        let addr = start_test_server();
        let (status, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert_eq!(body.trim(), "[]");
    }

    #[test]
    fn register_with_invalid_yaml_returns_400() {
        let addr = start_test_server();
        let (status, _) = send_request(&addr, "POST", "/nodes/register", "not: valid: yaml: at: all: -");
        assert_eq!(status, 400);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane http`
Expected: FAIL to compile — `handle_register`, `handle_heartbeat`, `handle_list` are not yet defined.

- [ ] **Step 3: Implement the route handlers**

In `keel-controlplane/src/http.rs`, add after `route` (before `error_response`):

```rust
fn handle_register(body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let registration: NodeRegistration = match serde_yaml::from_slice(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Register(registration.id, registration.addr, reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(()) => (200, Vec::new()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_heartbeat(id: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Heartbeat(id.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_list(commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::List(reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(statuses) => yaml_response(200, &statuses),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}
```

- [ ] **Step 4: Declare the module**

Modify `keel-controlplane/src/lib.rs`:

```rust
pub mod http;
pub mod registry;
pub mod wire;
pub mod worker;

pub use registry::Registry;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 21 tests pass (15 from Tasks 1-3, 6 new in `http.rs`).

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/http.rs keel-controlplane/src/lib.rs
git commit -m "Add keel-controlplane's TCP HTTP API: register/heartbeat/list"
```

---

### Task 5: `keel-controlplane` binary

**Files:**
- Create: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `keel_controlplane::{Registry, worker, http}` (Tasks 2-4).
- Produces: the `keel-controlplane` executable. Nothing later depends on its internals — `main.rs` has no `#[cfg(test)]` module, matching `keel-agentd/src/main.rs`'s existing precedent of leaving CLI-wiring binaries untested beyond a manual run (Milestone 5/6 established this; genuine logic lives in the library crate, already covered by Tasks 1-4).

- [ ] **Step 1: Write `main.rs`**

Create `keel-controlplane/src/main.rs`:

```rust
use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;

struct Config {
    addr: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { addr: "0.0.0.0:7620".to_string() }
    }
}

fn parse_args() -> Config {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--addr" => config.addr = value,
            other => panic!("unknown flag: {other}"),
        }
    }
    config
}

fn main() {
    let config = parse_args();
    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands);
}
```

- [ ] **Step 2: Build it**

Run: `cargo build -p keel-controlplane`
Expected: builds cleanly.

- [ ] **Step 3: Manually confirm it serves real requests**

```bash
cargo run -p keel-controlplane -- --addr 127.0.0.1:7620 &
sleep 1
curl -s -X POST -d 'id: node-1
addr: 10.0.0.1
' http://127.0.0.1:7620/nodes/register
curl -s http://127.0.0.1:7620/nodes
kill %1
```

Expected: the register `curl` returns no body with no error; the `GET /nodes` `curl` prints YAML containing `id: node-1`, `addr: 10.0.0.1`, and `status: Alive`.

- [ ] **Step 4: Commit**

```bash
git add keel-controlplane/src/main.rs
git commit -m "Add keel-controlplane binary"
```

---

### Task 6: `keel-agentd` CLI flags for node identity and control-plane address

**Files:**
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `Config` gains `node_id: Option<String>`, `control_plane_addr: Option<String>`, `advertise_addr: Option<String>` (all default `None`). A new, independently testable `parse_args_from(args: impl Iterator<Item = String>) -> Config`, with the existing `parse_args() -> Config` becoming a thin wrapper calling `parse_args_from(std::env::args().skip(1))`. Panics at parse time if `--control-plane-addr` is given without both `--node-id` and `--advertise-addr`.

- [ ] **Step 1: Write the failing tests**

Modify `keel-agentd/src/main.rs`: add at the very end of the file (this file currently has no `#[cfg(test)]` module):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn defaults_have_no_control_plane_configuration() {
        let config = parse_args_from(args(&["--pool", "zroot"]));
        assert_eq!(config.node_id, None);
        assert_eq!(config.control_plane_addr, None);
        assert_eq!(config.advertise_addr, None);
    }

    #[test]
    fn parses_all_three_new_flags() {
        let config = parse_args_from(args(&[
            "--node-id",
            "node-2",
            "--control-plane-addr",
            "192.168.64.2:7620",
            "--advertise-addr",
            "192.168.64.2",
        ]));
        assert_eq!(config.node_id, Some("node-2".to_string()));
        assert_eq!(config.control_plane_addr, Some("192.168.64.2:7620".to_string()));
        assert_eq!(config.advertise_addr, Some("192.168.64.2".to_string()));
    }

    #[test]
    #[should_panic(expected = "--node-id and --advertise-addr are required when --control-plane-addr is set")]
    fn control_plane_addr_without_node_id_panics() {
        parse_args_from(args(&["--control-plane-addr", "192.168.64.2:7620", "--advertise-addr", "192.168.64.2"]));
    }

    #[test]
    #[should_panic(expected = "--node-id and --advertise-addr are required when --control-plane-addr is set")]
    fn control_plane_addr_without_advertise_addr_panics() {
        parse_args_from(args(&["--control-plane-addr", "192.168.64.2:7620", "--node-id", "node-2"]));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd --bin keel-agentd`
Expected: FAIL to compile — `parse_args_from` is not yet defined, `Config` has no `node_id`/`control_plane_addr`/`advertise_addr` fields.

- [ ] **Step 3: Add the fields and the testable parse function**

Modify `keel-agentd/src/main.rs`'s `Config` struct and `Default` impl:

```rust
struct Config {
    pool: String,
    state_dir: PathBuf,
    socket: PathBuf,
    node_id: Option<String>,
    control_plane_addr: Option<String>,
    advertise_addr: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pool: "zroot".to_string(),
            state_dir: PathBuf::from("/var/db/keel"),
            socket: PathBuf::from("/var/run/keel-agentd.sock"),
            node_id: None,
            control_plane_addr: None,
            advertise_addr: None,
        }
    }
}
```

Replace `fn parse_args() -> Config { ... }` with:

```rust
fn parse_args() -> Config {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Config {
    let mut config = Config::default();
    let mut args = args;
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--pool" => config.pool = value,
            "--state-dir" => config.state_dir = PathBuf::from(value),
            "--socket" => config.socket = PathBuf::from(value),
            "--node-id" => config.node_id = Some(value),
            "--control-plane-addr" => config.control_plane_addr = Some(value),
            "--advertise-addr" => config.advertise_addr = Some(value),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.control_plane_addr.is_some() && (config.node_id.is_none() || config.advertise_addr.is_none()) {
        panic!("--node-id and --advertise-addr are required when --control-plane-addr is set");
    }
    config
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd --bin keel-agentd`
Expected: all 4 new tests pass.

- [ ] **Step 5: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all existing tests still pass unchanged (this task only adds fields/flags with `None` defaults and a new free function; nothing existing calls the new flags).

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/main.rs
git commit -m "Add optional --node-id/--control-plane-addr/--advertise-addr flags to keel-agentd"
```

---

### Task 7: `keel-agentd`'s `registration.rs` — register/heartbeat against a real control plane

**Files:**
- Modify: `keel-agentd/Cargo.toml`
- Create: `keel-agentd/src/registration.rs`
- Modify: `keel-agentd/src/lib.rs`

**Interfaces:**
- Consumes: `keel_controlplane::{registry::Registry, worker, http}` (for its own tests only), `NodeRegistration`'s wire shape (`id`/`addr` YAML fields, matching `keel-controlplane`'s `POST /nodes/register`).
- Produces: `registration::spawn(node_id: String, advertise_addr: String, control_plane_addr: String, heartbeat_interval: Duration) -> JoinHandle<()>`.

- [ ] **Step 1: Add the dependency**

Modify `keel-agentd/Cargo.toml`, in `[dependencies]`:

```toml
keel-controlplane = { path = "../keel-controlplane" }
```

(Full `[dependencies]` block after this change: `keel-spec`, `keel-jail`, `keel-zfs`, `keel-net`, `keel-controlplane`, `serde`, `serde_yaml`, `thiserror`, `httparse` — all path deps first, matching the existing ordering convention in this file.)

- [ ] **Step 2: Write the failing tests, against a no-op `spawn` stub**

Create `keel-agentd/src/registration.rs`:

```rust
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub fn spawn(
    _node_id: String,
    _advertise_addr: String,
    _control_plane_addr: String,
    _heartbeat_interval: Duration,
) -> JoinHandle<()> {
    // Stub: does nothing yet. Real registration/heartbeat loop is added in
    // Step 4, once the tests below are confirmed to fail against this.
    thread::spawn(|| {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_controlplane::registry::Registry;
    use keel_controlplane::worker;
    use std::net::TcpListener;

    fn start_test_control_plane() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new());
        thread::spawn(move || keel_controlplane::http::run(listener, commands));
        addr
    }

    fn get_nodes(control_plane_addr: &str) -> String {
        let mut stream = TcpStream::connect(control_plane_addr).unwrap();
        stream
            .write_all(b"GET /nodes HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).ok();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        String::from_utf8_lossy(&response).to_string()
    }

    #[test]
    fn registers_and_then_keeps_heartbeating() {
        let control_plane_addr = start_test_control_plane();
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(body.contains("node-1"), "expected node-1 to have registered, got: {body}");
        assert!(body.contains("Alive"), "expected node-1 to be Alive, got: {body}");
    }
}
```

(The "re-registers after the control plane forgets it" behavior is real and important, but cannot be exercised by an in-process test — see this plan's Global Constraints on why, and Task 9 Step 5 for where it's actually verified, against genuine separate OS processes.)

- [ ] **Step 3: Declare the module and run the tests to verify they fail**

Modify `keel-agentd/src/lib.rs`:

```rust
pub mod backoff;
pub mod http;
pub mod record;
pub mod reconciler;
pub mod registration;
pub mod store;
pub mod wire;
pub mod worker;

pub use record::JailRecord;
pub use reconciler::{ReconcileError, Reconciler};
pub use wire::{BackoffStatus, ErrorBody, JailStatus};
```

Run: `cargo test -p keel-agentd registration`
Expected: the test compiles and runs, but FAILS — `spawn`'s stub does nothing, so `get_nodes` never shows `"node-1"` in its assertions.

- [ ] **Step 4: Implement the real registration/heartbeat loop**

Replace `registration.rs`'s `spawn` stub with the real implementation:

```rust
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id) {
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

fn register_once(control_plane_addr: &str, node_id: &str, advertise_addr: &str) -> Result<(), String> {
    let body = format!("id: {node_id}\naddr: {advertise_addr}\n");
    send_request(control_plane_addr, "POST", "/nodes/register", &body)
}

fn heartbeat_once(control_plane_addr: &str, node_id: &str) -> Result<(), String> {
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), "")
}

fn send_request(addr: &str, method: &str, path: &str, body: &str) -> Result<(), String> {
    let mut stream =
        TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
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
        httparse::Status::Partial => return Err("incomplete response from control plane".to_string()),
    };
    let status = parsed.code.unwrap_or(0);
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("control plane returned status {status}"))
    }
}
```

(This replaces only the stub `pub fn spawn` block from Step 2 — the `#[cfg(test)] mod tests` block below it is untouched.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd registration`
Expected: `registers_and_then_keeps_heartbeating` passes. This runs in well under a second thanks to the 50ms `heartbeat_interval` used in the test (see this plan's Global Constraints on why the interval is a parameter, not the design spec's hardcoded 5s, precisely to keep this fast).

- [ ] **Step 6: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all existing tests plus this 1 new one pass; nothing in Tasks 1-6 regresses.

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/Cargo.toml keel-agentd/src/registration.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd's registration.rs: register + heartbeat against keel-controlplane, self-healing on rejection"
```

---

### Task 8: Wire `registration::spawn` into `keel-agentd`'s `main.rs`

**Files:**
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_agentd::registration::spawn` (Task 7), `Config`'s `node_id`/`control_plane_addr`/`advertise_addr` (Task 6).
- Produces: no new public interface — `main()` conditionally spawns the registration background thread.

- [ ] **Step 1: Spawn the registration thread when configured**

Modify `keel-agentd/src/main.rs`'s `fn main()`, adding this block right after the existing `eprintln!("keel-agentd: starting ...")` call and before `let (_worker_handle, commands) = worker::spawn(reconciler);`:

```rust
    if let (Some(node_id), Some(control_plane_addr), Some(advertise_addr)) =
        (config.node_id.clone(), config.control_plane_addr.clone(), config.advertise_addr.clone())
    {
        eprintln!(
            "keel-agentd: registering with control plane at {control_plane_addr} as node '{node_id}' ({advertise_addr})"
        );
        keel_agentd::registration::spawn(node_id, advertise_addr, control_plane_addr, Duration::from_secs(5));
    }
```

(`Duration` is already imported at the top of `main.rs` for the existing 5-second reconcile timer, so no new `use` statement is needed.)

- [ ] **Step 2: Build and run the full workspace test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds cleanly, all tests pass (this change only adds a conditional branch that is never taken by any existing test, since none of them set `--control-plane-addr`).

- [ ] **Step 3: Manually verify against a real local `keel-controlplane`**

```bash
cargo build --workspace
./target/debug/keel-controlplane --addr 127.0.0.1:7620 &
sleep 1
./target/debug/keel-agentd --pool zroot --state-dir /tmp/keel-m7-smoke --socket /tmp/keel-m7-smoke.sock \
  --node-id node-test --control-plane-addr 127.0.0.1:7620 --advertise-addr 127.0.0.1 &
sleep 1
curl -s http://127.0.0.1:7620/nodes
kill %1 %2
rm -rf /tmp/keel-m7-smoke /tmp/keel-m7-smoke.sock
```

Expected: the `curl` output is YAML containing `id: node-test`, `addr: 127.0.0.1`, `status: Alive`. (`keel-agentd`'s own `ProcessJailRuntime`/`CliZfsManager`/`ProcessNetManager` will emit no output here since nothing calls `apply`; this step only exercises the registration path, which is all this milestone changes.)

- [ ] **Step 4: Commit**

```bash
git add keel-agentd/src/main.rs
git commit -m "Wire keel-agentd to register with a control plane when configured"
```

---

### Task 9: FreeBSD VM verification across three real nodes

**Files:** none expected (verification only), unless the VM run surfaces a real bug — if so, fix it following the same practice every prior milestone established (fix on macOS with a regression test, re-verify, then re-run the affected VM steps).

- [ ] **Step 1: Sync and build the repo on all three VMs**

```bash
ssh root@192.168.64.2 "cd kubsd && git pull && cargo build --release --workspace"
ssh root@192.168.64.4 "cd kubsd && git pull && cargo build --release --workspace"
ssh root@192.168.64.5 "cd kubsd && git pull && cargo build --release --workspace"
```

Expected: all three build successfully. (`kubsd` is every VM's actual, never-renamed clone directory — see Milestone 5's Task 8 for why.)

- [ ] **Step 2: Start `keel-controlplane` on `.2`**

```bash
ssh root@192.168.64.2 "cd kubsd && nohup ./target/release/keel-controlplane --addr 0.0.0.0:7620 > /tmp/keel-controlplane.log 2>&1 &"
sleep 1
ssh root@192.168.64.2 "cat /tmp/keel-controlplane.log"
```

Expected: log shows `keel-controlplane: starting (addr=0.0.0.0:7620)`.

- [ ] **Step 3: Start `keel-agentd` on all three nodes, each registering with `.2`**

```bash
ssh root@192.168.64.2 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-2 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.2 > /tmp/keel-agentd.log 2>&1 &"
ssh root@192.168.64.4 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-4 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.4 > /tmp/keel-agentd.log 2>&1 &"
ssh root@192.168.64.5 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-5 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.5 > /tmp/keel-agentd.log 2>&1 &"
sleep 6
ssh root@192.168.64.2 "curl -s http://127.0.0.1:7620/nodes"
```

Expected: YAML listing all three of `node-2`, `node-4`, `node-5`, each `status: Alive`, within one heartbeat interval of starting (the `sleep 6` covers the 5s heartbeat interval plus margin).

- [ ] **Step 4: Kill `keel-agentd` on `.4` and confirm it alone goes Dead**

```bash
ssh root@192.168.64.4 "pkill keel-agentd"
sleep 16
ssh root@192.168.64.2 "curl -s http://127.0.0.1:7620/nodes"
```

Expected: `node-4` shows `status: Dead` (16s comfortably exceeds the 15s `DEAD_THRESHOLD`); `node-2` and `node-5` still show `status: Alive`, confirming one node's death doesn't affect the others.

- [ ] **Step 5: Restart `keel-controlplane` on `.2` and confirm the surviving nodes repopulate it**

```bash
ssh root@192.168.64.2 "pkill keel-controlplane; sleep 1; cd kubsd && nohup ./target/release/keel-controlplane --addr 0.0.0.0:7620 > /tmp/keel-controlplane.log 2>&1 &"
sleep 1
ssh root@192.168.64.2 "curl -s http://127.0.0.1:7620/nodes"
sleep 6
ssh root@192.168.64.2 "curl -s http://127.0.0.1:7620/nodes"
```

Expected: immediately after the restart, the first `curl` returns `[]` (fresh, empty registry — no persistence, exactly as designed). The second `curl`, after a 6s wait (one heartbeat interval plus margin), shows `node-2` and `node-5` both back as `Alive` again — neither `keel-agentd` process was restarted, only the control plane, proving the self-healing re-registration behavior end-to-end on real, separate hosts.

- [ ] **Step 6: Clean up all three VMs**

```bash
ssh root@192.168.64.2 "pkill keel-agentd; pkill keel-controlplane"
ssh root@192.168.64.4 "pkill keel-agentd"
ssh root@192.168.64.5 "pkill keel-agentd"
ssh root@192.168.64.2 "jls; zfs list -r zroot/keel"
```

Expected: no leftover `keel-agentd`/`keel-controlplane` processes; no leftover jails or datasets (this milestone never applies any `JailSpec`, so there should be none to clean up in the first place — this step is a final sanity check, not real cleanup work).

- [ ] **Step 7: Record the outcome**

If every step above passed with no code changes needed, note in a follow-up commit message (or the final review) that Milestone 7 was VM-verified on this date across all three nodes, including the observed all-Alive `/nodes` output from Steps 3 and 5. If any VM step surfaced a real bug, fix it on macOS with a regression test added to the relevant task's file, re-run `cargo test --workspace`, then re-run the affected VM steps before considering the milestone done.

---

## Milestone Exit Criteria

- `keel-controlplane` is a new, independent crate (`serde`/`serde_yaml`/`thiserror`/`httparse` only) exposing `POST /nodes/register`, `POST /nodes/:id/heartbeat`, and `GET /nodes` over a plain TCP `httparse`-based HTTP server, with no persistence.
- `keel-agentd` gains three new, entirely optional CLI flags; every Milestone 1-6 behavior and test is unchanged when they're absent.
- `keel-agentd`'s `registration.rs` self-heals: any heartbeat rejection or connection failure causes it to re-register on its next tick, verified by a fast in-process test of the basic register/heartbeat path (Task 7) and, for the actual forget-and-recover behavior, by a real control-plane process restart across separate hosts (Task 9).
- `cargo test --workspace` passes with the ~26 new tests (4 wire + 7 registry + 4 worker + 6 http in `keel-controlplane`, 4 CLI-flag + 1 registration in `keel-agentd`) added by this milestone, on top of the 100 inherited from Milestone 6.
- VM-verified end-to-end across all three real nodes (`.2`/`.4`/`.5`): all-Alive membership, one node's death detected without affecting the others, and full membership recovery after a control-plane restart with no node process restarted.
