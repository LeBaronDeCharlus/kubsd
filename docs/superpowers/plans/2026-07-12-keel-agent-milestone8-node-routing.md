# keel Milestone 8: Routing Jail Specs to a Specific Node — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 7:** it needs the real FreeBSD VMs (`root@192.168.64.2`, `.4`, `.5`). The coordinating session has direct SSH access to these VMs and should run this task itself rather than dispatching a subagent for it — this mirrors Milestone 5's Task 8, Milestone 6's Task 4, and Milestone 7's Task 9. **Tasks 1-6 are pure file edits, verified locally (macOS) via `cargo test`, and need no FreeBSD VM interaction at all.**

**Goal:** Let a client apply/get/delete a `JailSpec` on a specific, named node by routing the request through `keel-controlplane`, which forwards it (opaque to the spec body) to that node's `keel-agentd`.

**Architecture:** `keel-controlplane` gains `Registry::resolve`/`Command::Resolve` (look up a node's address, rejecting unknown or Dead nodes before any network call) and four new HTTP routes (`PUT/GET/DELETE /nodes/{id}/jails/...`) whose handler opens a fresh `TcpStream` to the resolved address, forwards the request byte-for-byte, and relays the response back — never deserializing the body. `keel-agentd` gains a second, opt-in TCP listener (`http::run_tcp`) serving the exact same route/dispatch logic already used by its existing Unix socket, bound to `--advertise-addr` (which changes from Milestone 7's undialed display string to a real `host:port` bind address) only when the Milestone 7 control-plane trio of flags (`--node-id`, `--control-plane-addr`, `--advertise-addr`) is set — the Unix socket itself is untouched. `keelctl` gains `--control-plane-addr`/`--node` flags that route a request through the control plane instead of a local socket; omitting both preserves today's exact behavior.

**Tech Stack:** Rust (2021 edition), same four dependencies already used throughout this workspace (`serde`, `serde_yaml`, `thiserror`, `httparse`) — no new external dependencies anywhere.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-12-keel-agent-milestone8-node-routing-design.md` (Approved). Route shapes, the byte-opaque forwarding model, and the dead-node-rejected-before-dialing behavior described there must match exactly.
- **No new external dependencies.** `keel-controlplane` and `keel-agentd` already use only `serde`, `serde_yaml`, `thiserror`, `httparse`; `keel-agentd` already depends on `keel-controlplane` (added in Milestone 7) so no `Cargo.toml` change is needed there. The only manifest change in this whole plan is adding `keel-controlplane` as a **dev-dependency** of `keelctl` (Task 6), for its own integration tests — an existing workspace crate, not a new external one.
- **`--advertise-addr`'s contract changes** from Milestone 7's undialed display string (a bare IP, e.g. `"192.168.64.2"`) to a real `host:port` TCP bind address (e.g. `"192.168.64.2:7621"`) actually used by `TcpListener::bind`. Milestone 7's own unit tests that pass bare-IP strings into `parse_args_from` (e.g. `parses_all_three_new_flags`) only exercise string storage, never binding, so they remain valid and unchanged; only this plan's Task 5 (wiring) and Task 7 (real VM run) actually dial or bind this value.
- **Two timeout constants with no spec-mandated exact value:** the design spec calls for "a short connect timeout (a few seconds) and an equally short read timeout" on the control plane's outbound forwarding call, without pinning numbers. This plan fixes them as `FORWARD_CONNECT_TIMEOUT = Duration::from_secs(2)` and `FORWARD_READ_TIMEOUT = Duration::from_secs(5)` in `keel-controlplane`'s `http.rs` (Task 3).
- **A deliberate, behavior-preserving reordering in `keel-agentd`'s `main.rs`** (Task 5): `worker::spawn(reconciler)` moves earlier, before the control-plane opt-in block, so its `commands: Sender<Command>` is available there to hand to the new TCP listener. This has no effect on the timer thread, Unix socket bind, or final `http::run` call, all of which still happen in the same relative order afterward.
- Every new public type, function, and constant introduced by one task and used by a later task is named exactly as given in that task's **Produces** list — later tasks must match these names exactly.
- No placeholders: every task's deliverable is verified with `cargo build -p <crate> && cargo test -p <crate>` before its commit step.
- Current baseline entering this milestone (verified directly before writing this plan): `cargo test --workspace` → 122 passed, of which `keel-controlplane` → 21 (all lib), `keel-agentd` → 62 (58 lib + 4 bin), `keelctl` → 3 (integration tests in `tests/cli.rs`).

---

### Task 1: `Registry::resolve` — look up a node's address, rejecting Unknown/Dead

**Files:**
- Modify: `keel-controlplane/src/registry.rs`

**Interfaces:**
- Consumes: `Registry`, `NodeRecord`, `DEAD_THRESHOLD` (all already in this file).
- Produces: `ResolveError::{Unknown(String), Dead { id: String, last_seen_secs: u64 }}` (`#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]`), `Registry::resolve(&self, id: &str, now: Instant) -> Result<String, ResolveError>`.

- [ ] **Step 1: Write the failing tests**

Modify `keel-controlplane/src/registry.rs`, adding after the existing `UnknownNode` struct definition (before `impl Registry`):

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("unknown node '{0}'")]
    Unknown(String),
    #[error("node '{id}' is dead (last seen {last_seen_secs}s ago)")]
    Dead { id: String, last_seen_secs: u64 },
}
```

Add to the `#[cfg(test)] mod tests` block, after the existing `list_on_an_empty_registry_is_empty` test:

```rust
    #[test]
    fn resolve_on_an_unknown_node_returns_unknown_error() {
        let registry = Registry::new();
        let err = registry.resolve("missing", Instant::now()).unwrap_err();
        assert_eq!(err, ResolveError::Unknown("missing".to_string()));
    }

    #[test]
    fn resolve_on_an_alive_node_returns_its_address() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), now);
        assert_eq!(registry.resolve("node-1", now), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_on_a_dead_node_returns_dead_error_with_elapsed_seconds() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let at_threshold = t0 + DEAD_THRESHOLD;
        let err = registry.resolve("node-1", at_threshold).unwrap_err();
        assert_eq!(
            err,
            ResolveError::Dead { id: "node-1".to_string(), last_seen_secs: DEAD_THRESHOLD.as_secs() }
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane registry`
Expected: FAIL to compile — `resolve` is not yet defined on `Registry`.

- [ ] **Step 3: Implement `resolve`**

In `keel-controlplane/src/registry.rs`, add to the `impl Registry` block (after `list`):

```rust
    pub fn resolve(&self, id: &str, now: Instant) -> Result<String, ResolveError> {
        let record = self.nodes.get(id).ok_or_else(|| ResolveError::Unknown(id.to_string()))?;
        let elapsed = now.saturating_duration_since(record.last_heartbeat);
        if elapsed >= DEAD_THRESHOLD {
            return Err(ResolveError::Dead { id: id.to_string(), last_seen_secs: elapsed.as_secs() });
        }
        Ok(record.addr.clone())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 24 tests pass (21 inherited from Milestone 7, 3 new).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/registry.rs
git commit -m "Add Registry::resolve: look up a node's address, rejecting Unknown/Dead"
```

---

### Task 2: `worker::Command::Resolve`

**Files:**
- Modify: `keel-controlplane/src/worker.rs`

**Interfaces:**
- Consumes: `Registry::resolve`, `ResolveError` (Task 1).
- Produces: `Command::Resolve(String, Sender<Result<String, ResolveError>>)`, handled by `worker::spawn`'s existing dispatch loop.

- [ ] **Step 1: Write the failing tests**

Modify `keel-controlplane/src/worker.rs`'s import line and `Command` enum:

```rust
use crate::registry::{Registry, ResolveError, UnknownNode};
use crate::wire::NodeStatus;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub enum Command {
    Register(String, String, Sender<()>),
    Heartbeat(String, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
}
```

Add to the `#[cfg(test)] mod tests` block, after `list_command_on_a_fresh_worker_is_empty`:

```rust
    #[test]
    fn resolve_command_on_a_registered_alive_node_returns_its_address() {
        let commands = spawn(Registry::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("node-1".to_string(), resolve_tx)).unwrap();
        assert_eq!(resolve_rx.recv().unwrap(), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_command_on_an_unknown_node_returns_an_error() {
        let commands = spawn(Registry::new()).1;

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("missing".to_string(), resolve_tx)).unwrap();
        assert!(resolve_rx.recv().unwrap().is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane worker`
Expected: FAIL to compile — `Command::Resolve` is not yet handled by `handle_command`.

- [ ] **Step 3: Handle the new command**

In `keel-controlplane/src/worker.rs`'s `handle_command`, add a new match arm (after `Command::List`):

```rust
        Command::Resolve(id, reply) => {
            let result = registry.resolve(&id, Instant::now());
            let _ = reply.send(result);
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 26 tests pass (24 from Task 1, 2 new).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/worker.rs
git commit -m "Add Command::Resolve to keel-controlplane's worker"
```

---

### Task 3: `keel-controlplane` HTTP forwarding routes

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `Command::Resolve` (Task 2).
- Produces: new routes `PUT /nodes/{id}/jails/{name}`, `GET /nodes/{id}/jails`, `GET /nodes/{id}/jails/{name}`, `DELETE /nodes/{id}/jails/{name}`, each dispatched through `handle_forward`, which resolves the node then forwards raw bytes over a fresh `TcpStream` via `forward`.

- [ ] **Step 1: Write the failing tests**

Modify `keel-controlplane/src/http.rs`'s import line:

```rust
use crate::wire::{ErrorBody, NodeRegistration};
use crate::worker::Command;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;
```

Modify `route()` to add four new arms before the catch-all `_ =>`:

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, commands),
        ("GET", ["nodes"]) => handle_list(commands),
        ("PUT", ["nodes", id, "jails", name]) => {
            handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands)
        }
        ("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands),
        ("GET", ["nodes", id, "jails", name]) => {
            handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands)
        }
        ("DELETE", ["nodes", id, "jails", name]) => {
            handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands)
        }
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}
```

Add to the `#[cfg(test)] mod tests` block, after `register_with_invalid_yaml_returns_400`:

```rust
    fn start_fake_remote_agentd(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        addr
    }

    fn register_node(cp_addr: &str, id: &str, node_addr: &str) {
        send_request(cp_addr, "POST", "/nodes/register", &format!("id: {id}\naddr: {node_addr}\n"));
    }

    #[test]
    fn forward_put_relays_status_and_body_from_the_target_node() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/nodes/node-1/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("running: true"), "expected relayed body, got: {body}");
    }

    #[test]
    fn forward_get_relays_status_and_body_from_the_target_node() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "jails: fake-list\n");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, body) = send_request(&cp_addr, "GET", "/nodes/node-1/jails", "");
        assert_eq!(status, 200);
        assert!(body.contains("fake-list"), "expected relayed body, got: {body}");
    }

    #[test]
    fn forward_delete_relays_status_from_the_target_node() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, _) = send_request(&cp_addr, "DELETE", "/nodes/node-1/jails/web-1", "");
        assert_eq!(status, 200);
    }

    #[test]
    fn forward_to_an_unknown_node_returns_404() {
        let cp_addr = start_test_server();
        let (status, body) = send_request(&cp_addr, "GET", "/nodes/missing/jails", "");
        assert_eq!(status, 404);
        assert!(body.contains("unknown node"), "expected 'unknown node' in body: {body}");
    }

    #[test]
    fn forward_to_a_node_with_nothing_listening_returns_500() {
        let cp_addr = start_test_server();
        register_node(&cp_addr, "node-1", "127.0.0.1:1");

        let (status, body) = send_request(&cp_addr, "GET", "/nodes/node-1/jails", "");
        assert_eq!(status, 500);
        assert!(body.contains("failed to reach node"), "expected forwarding failure in body: {body}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-controlplane http`
Expected: FAIL to compile — `handle_forward` is not yet defined.

- [ ] **Step 3: Implement `handle_forward` and `forward`**

In `keel-controlplane/src/http.rs`, add after `handle_list` (before `error_response`):

```rust
const FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const FORWARD_READ_TIMEOUT: Duration = Duration::from_secs(5);

fn handle_forward(id: &str, method: &str, path: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Resolve(id.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let addr = match reply_rx.recv() {
        Ok(Ok(addr)) => addr,
        Ok(Err(e)) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, method, path, body) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{id}' at {addr}: {e}")),
    }
}

fn forward(addr: &str, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "could not resolve address".to_string())?;
    let mut stream =
        TcpStream::connect_timeout(&socket_addr, FORWARD_CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(FORWARD_READ_TIMEOUT)).ok();

    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(&response).map_err(|e| e.to_string())? {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Err("incomplete response".to_string()),
    };
    let status = parsed.code.ok_or_else(|| "missing status code".to_string())?;
    Ok((status, response[header_len..].to_vec()))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-controlplane`
Expected: all 31 tests pass (26 from Tasks 1-2, 5 new in `http.rs`).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Add keel-controlplane's node-forwarding HTTP routes: /nodes/:id/jails/..."
```

---

### Task 4: `keel-agentd`'s `http::run_tcp` — the jails API over TCP

**Files:**
- Modify: `keel-agentd/src/http.rs`

**Interfaces:**
- Consumes: `route`, `reason_phrase`, `error_response`, `ParsedRequest`, `Command` (all already in this file — unchanged, transport-agnostic).
- Produces: `pub fn run_tcp(listener: TcpListener, commands: Sender<Command>)`, serving the identical route set (`PUT/GET/DELETE /jails/...`) already served by the existing Unix-socket `run`.

- [ ] **Step 1: Write the failing tests**

Modify `keel-agentd/src/http.rs`'s import line:

```rust
use crate::reconciler::ReconcileError;
use crate::wire::ErrorBody;
use crate::worker::Command;
use keel_spec::JailSpec;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender};
use std::thread;
```

Add to the `#[cfg(test)] mod tests` block, after `oversized_content_length_closes_the_connection_without_reading_the_body`:

```rust
    fn start_tcp_test_server(name: &str) -> String {
        let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-tcp-test-state-{name}"));
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

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || run_tcp(listener, commands));
        addr
    }

    fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
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
    fn put_valid_spec_over_tcp_returns_200_and_provisions_the_jail() {
        let addr = start_tcp_test_server("put_valid_spec_over_tcp_returns_200_and_provisions_the_jail");
        let (status, _) = send_request_tcp(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        assert_eq!(status, 200);

        let (status, body) = send_request_tcp(&addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 200);
        assert!(body.contains("running: true"), "expected running: true in body: {body}");
    }

    #[test]
    fn get_jails_over_tcp_lists_all_applied_jails() {
        let addr = start_tcp_test_server("get_jails_over_tcp_lists_all_applied_jails");
        send_request_tcp(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        send_request_tcp(&addr, "PUT", "/jails/web-2", &sample_spec_yaml("web-2"));

        let (status, body) = send_request_tcp(&addr, "GET", "/jails", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-1"));
        assert!(body.contains("web-2"));
    }

    #[test]
    fn delete_over_tcp_removes_a_provisioned_jail() {
        let addr = start_tcp_test_server("delete_over_tcp_removes_a_provisioned_jail");
        send_request_tcp(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        let (status, _) = send_request_tcp(&addr, "DELETE", "/jails/web-1", "");
        assert_eq!(status, 200);

        let (status, _) = send_request_tcp(&addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 404, "deleted jail should no longer be found");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p keel-agentd --lib`
Expected: FAIL to compile — `run_tcp` is not yet defined.

- [ ] **Step 3: Implement `run_tcp`, duplicating the Unix-socket read/write loop for `TcpStream`**

In `keel-agentd/src/http.rs`, add after the existing `run(listener: UnixListener, ...)` function (before the `ParsedRequest` struct):

```rust
pub fn run_tcp(listener: TcpListener, commands: Sender<Command>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        thread::spawn(move || {
            let _ = handle_connection_tcp(stream, &commands);
        });
    }
}
```

Add after the existing `handle_connection` function (before `read_request`):

```rust
fn handle_connection_tcp(mut stream: TcpStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request_tcp(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response_tcp(&mut stream, status, &body)
}
```

Add after the existing `read_request` function (before `write_response`):

```rust
fn read_request_tcp(stream: &mut TcpStream) -> io::Result<Option<ParsedRequest>> {
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
```

Add after the existing `write_response` function (before `reason_phrase`):

```rust
fn write_response_tcp(stream: &mut TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p keel-agentd --lib`
Expected: all 61 tests pass (58 inherited, 3 new).

- [ ] **Step 5: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass; nothing in `keel-agentd`'s existing Unix-socket path regresses (it is untouched — `run_tcp` and its helpers are new, additive functions only).

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/http.rs
git commit -m "Add keel-agentd's http::run_tcp: the jails API served over TCP"
```

---

### Task 5: Wire `run_tcp` into `keel-agentd`'s `main.rs`

**Files:**
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_agentd::http::run_tcp` (Task 4), `Config`'s existing `node_id`/`control_plane_addr`/`advertise_addr` fields (Milestone 7).
- Produces: no new public interface — `main()` binds the new TCP listener and spawns `run_tcp` when the control-plane trio is set, alongside the existing registration spawn.

- [ ] **Step 1: Add the `TcpListener` import**

Modify `keel-agentd/src/main.rs`'s import block, adding `std::net::TcpListener`:

```rust
use keel_agentd::worker::{self, Command};
use keel_agentd::Reconciler;
use keel_jail::ProcessJailRuntime;
use keel_net::ProcessNetManager;
use keel_zfs::CliZfsManager;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
```

- [ ] **Step 2: Reorder `worker::spawn` earlier, and bind the new TCP listener in the control-plane block**

Replace `fn main()`'s body with:

```rust
fn main() {
    let config = parse_args();

    let reconciler = Reconciler::new(
        ProcessJailRuntime::new(),
        CliZfsManager::new(),
        ProcessNetManager::new(),
        config.pool.clone(),
        config.state_dir.clone(),
    )
    .expect("failed to initialize reconciler from on-disk state");

    eprintln!(
        "keel-agentd: starting (pool={}, state_dir={}, socket={})",
        config.pool,
        config.state_dir.display(),
        config.socket.display()
    );

    let (_worker_handle, commands) = worker::spawn(reconciler);

    if let (Some(node_id), Some(control_plane_addr), Some(advertise_addr)) =
        (config.node_id.clone(), config.control_plane_addr.clone(), config.advertise_addr.clone())
    {
        eprintln!(
            "keel-agentd: registering with control plane at {control_plane_addr} as node '{node_id}' ({advertise_addr})"
        );
        keel_agentd::registration::spawn(
            node_id,
            advertise_addr.clone(),
            control_plane_addr,
            Duration::from_secs(5),
        );

        eprintln!("keel-agentd: serving jails API over TCP on {advertise_addr}");
        let tcp_listener = TcpListener::bind(&advertise_addr)
            .unwrap_or_else(|e| panic!("failed to bind jails-API TCP listener on {advertise_addr}: {e}"));
        let tcp_commands = commands.clone();
        thread::spawn(move || keel_agentd::http::run_tcp(tcp_listener, tcp_commands));
    }

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

(Everything below `fn main()` — `parse_args`, `parse_args_from`, `Config`, and the existing `#[cfg(test)] mod tests` block — is unchanged.)

- [ ] **Step 3: Build and run the full workspace test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds cleanly, all tests pass. This change only adds a conditional branch inside the existing control-plane opt-in block; no existing test sets `--control-plane-addr`, so none of them take it, and the `worker::spawn` reordering has no observable effect on any existing test (none depend on binding order between the control-plane block and the Unix socket).

- [ ] **Step 4: Manually verify the new TCP listener actually serves the jails API**

```bash
cargo build --workspace
./target/debug/keel-controlplane --addr 127.0.0.1:7620 &
sleep 1
./target/debug/keel-agentd --pool zroot --state-dir /tmp/keel-m8-smoke --socket /tmp/keel-m8-smoke.sock \
  --node-id node-test --control-plane-addr 127.0.0.1:7620 --advertise-addr 127.0.0.1:7621 &
sleep 1
curl -s http://127.0.0.1:7620/nodes
curl -s -X PUT -d 'not: valid: yaml: [' http://127.0.0.1:7621/jails/web-1
curl -s http://127.0.0.1:7621/jails
kill %1 %2
rm -rf /tmp/keel-m8-smoke /tmp/keel-m8-smoke.sock
```

Expected: the first `curl` shows `id: node-test`, `addr: 127.0.0.1:7621`, `status: Alive` (proving registration still uses the same value, now a real bind address). The second `curl` (an intentionally invalid spec, chosen so this step needs no FreeBSD-only tooling) returns YAML containing `invalid YAML`, proving the request reached `handle_apply` over the new TCP listener. The third `curl` returns `[]` (no jail was actually provisioned, since the previous request was rejected), proving `GET /jails` also works over TCP.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/main.rs
git commit -m "Wire keel-agentd's jails API onto a second, opt-in TCP listener"
```

---

### Task 6: `keelctl` — route through the control plane

**Files:**
- Modify: `keelctl/Cargo.toml`
- Modify: `keelctl/src/main.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `keel_controlplane::{Registry, worker::{self, Command as ControlPlaneCommand}, http}` (for its own tests only), `keel_agentd::http::run_tcp` (Task 4).
- Produces: `Target::{Socket(PathBuf), ControlPlane { addr: String, node: String }}`, `jails_path(target: &Target, suffix: &str) -> String`, `dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String>`, `send_request_tcp(addr: &str, method: &str, path: &str, body: &str) -> Result<String, String>`, `parse_response(response: &[u8]) -> Result<String, String>` (shared response-parsing logic, factored out of the existing `send_request`). New CLI flags `--control-plane-addr`/`--node`, used together; omitting both preserves today's exact `--socket`-based behavior.

- [ ] **Step 1: Add the dev-dependency**

Modify `keelctl/Cargo.toml`'s `[dev-dependencies]`:

```toml
[dev-dependencies]
keel-jail = { path = "../keel-jail" }
keel-zfs = { path = "../keel-zfs" }
keel-net = { path = "../keel-net" }
keel-controlplane = { path = "../keel-controlplane" }
```

- [ ] **Step 2: Write the failing tests**

Modify `keelctl/tests/cli.rs`, adding after the existing `run_keelctl` helper (before `apply_get_delete_round_trip`):

```rust
fn start_test_agentd_tcp(name: &str) -> String {
    let state_dir = std::env::temp_dir().join(format!("keelctl-routed-test-state-{name}"));
    let _ = std::fs::remove_dir_all(&state_dir);
    let zfs = FakeZfsManager::new();
    zfs.seed_dataset("zroot/keel/base/14.2-web");
    let reconciler =
        Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), "zroot".to_string(), state_dir)
            .unwrap();
    let (_worker_handle, commands) = worker::spawn(reconciler);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || keel_agentd::http::run_tcp(listener, commands));
    addr
}

fn start_test_control_plane_with_node(node_id: &str, node_addr: &str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) = keel_controlplane::worker::spawn(keel_controlplane::Registry::new());

    let (reg_tx, reg_rx) = std::sync::mpsc::channel();
    commands
        .send(keel_controlplane::worker::Command::Register(node_id.to_string(), node_addr.to_string(), reg_tx))
        .unwrap();
    reg_rx.recv().unwrap();

    thread::spawn(move || keel_controlplane::http::run(listener, commands));
    addr
}

fn run_keelctl_routed(control_plane_addr: &str, node: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(args)
        .arg("--control-plane-addr")
        .arg(control_plane_addr)
        .arg("--node")
        .arg(node)
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn apply_get_delete_round_trip_through_the_control_plane() {
    let node_addr = start_test_agentd_tcp("routed_round_trip");
    let control_plane_addr = start_test_control_plane_with_node("node-1", &node_addr);
    let spec_path = write_spec_file("routed_round_trip", "web-1");

    let (ok, _, stderr) =
        run_keelctl_routed(&control_plane_addr, "node-1", &["apply", "-f", spec_path.to_str().unwrap()]);
    assert!(ok, "apply failed: {stderr}");

    let (ok, stdout, stderr) = run_keelctl_routed(&control_plane_addr, "node-1", &["get", "web-1"]);
    assert!(ok, "get failed: {stderr}");
    assert!(stdout.contains("running: true"), "expected running: true, got: {stdout}");

    let (ok, _, stderr) = run_keelctl_routed(&control_plane_addr, "node-1", &["delete", "web-1"]);
    assert!(ok, "delete failed: {stderr}");
}

#[test]
fn apply_through_the_control_plane_to_an_unknown_node_fails() {
    let control_plane_addr = start_test_control_plane_with_node("node-1", "127.0.0.1:1");
    let spec_path = write_spec_file("routed_unknown_node", "web-1");

    let (ok, _, stderr) =
        run_keelctl_routed(&control_plane_addr, "node-missing", &["apply", "-f", spec_path.to_str().unwrap()]);
    assert!(!ok);
    assert!(stderr.contains("unknown node"), "expected 'unknown node' in stderr, got: {stderr}");
}

#[test]
fn control_plane_addr_without_node_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(["get"])
        .arg("--control-plane-addr")
        .arg("127.0.0.1:1")
        .output()
        .expect("failed to run keelctl binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--control-plane-addr and --node must be given together"),
        "got: {stderr}"
    );
}

#[test]
fn node_without_control_plane_addr_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(["get"])
        .arg("--node")
        .arg("node-1")
        .output()
        .expect("failed to run keelctl binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--control-plane-addr and --node must be given together"),
        "got: {stderr}"
    );
}
```

Modify `keelctl/tests/cli.rs`'s import line to add `keel_agentd::http` (already re-exported via `keel_agentd::{worker, Reconciler}` plus the module path) and `thread` (already imported) — no new imports are actually needed beyond what Step 1's `Cargo.toml` change makes available, since `keel_controlplane::` and `keel_agentd::http::run_tcp` are referenced fully-qualified in the new helpers above.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p keelctl`
Expected: FAIL to compile — `keelctl` (the binary under test) has no `--control-plane-addr`/`--node` flags yet, and the compile itself fails since `keel_controlplane` isn't a dev-dependency until Step 1 lands (already applied above) but the binary's flag handling doesn't exist until Step 4.

- [ ] **Step 4: Rewrite `keelctl/src/main.rs`**

Replace the entire contents of `keelctl/src/main.rs`:

```rust
use keel_agentd::ErrorBody;
use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_SOCKET: &str = "/var/run/keel-agentd.sock";

enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: String },
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let control_plane_addr = extract_flag(&mut args, "--control-plane-addr");
    let node = extract_flag(&mut args, "--node");

    let target = match (control_plane_addr, node) {
        (Some(addr), Some(node)) => Target::ControlPlane { addr, node },
        (None, None) => Target::Socket(socket),
        _ => {
            eprintln!("error: --control-plane-addr and --node must be given together");
            return ExitCode::FAILURE;
        }
    };

    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "apply" => run_apply(&target, rest),
        Some((cmd, rest)) if cmd == "get" => run_get(&target, rest),
        Some((cmd, rest)) if cmd == "delete" => run_delete(&target, rest),
        _ => {
            eprintln!(
                "usage: keelctl <apply -f FILE|get [name]|delete NAME> [--socket PATH|--control-plane-addr ADDR --node ID]"
            );
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

fn extract_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.remove(index);
    Some(args.remove(index))
}

fn extract_socket_flag(args: &mut Vec<String>) -> Option<PathBuf> {
    extract_flag(args, "--socket").map(PathBuf::from)
}

fn jails_path(target: &Target, suffix: &str) -> String {
    match target {
        Target::Socket(_) => suffix.to_string(),
        Target::ControlPlane { node, .. } => format!("/nodes/{node}{suffix}"),
    }
}

fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, .. } => send_request_tcp(addr, method, path, body),
    }
}

fn run_apply(target: &Target, args: &[String]) -> Result<String, String> {
    let index = args.iter().position(|a| a == "-f").ok_or("apply requires -f FILE")?;
    let file = args.get(index + 1).ok_or("apply requires -f FILE")?;
    let yaml = std::fs::read_to_string(file).map_err(|e| format!("failed to read {file}: {e}"))?;
    let spec = keel_spec::parse_and_validate(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
    let path = jails_path(target, &format!("/jails/{}", spec.metadata.name));
    dispatch(target, "PUT", &path, &yaml).map(|_| String::new())
}

fn run_get(target: &Target, args: &[String]) -> Result<String, String> {
    let suffix = match args.first() {
        Some(name) => format!("/jails/{name}"),
        None => "/jails".to_string(),
    };
    let path = jails_path(target, &suffix);
    dispatch(target, "GET", &path, "")
}

fn run_delete(target: &Target, args: &[String]) -> Result<String, String> {
    let name = args.first().ok_or("delete requires a jail name")?;
    let path = jails_path(target, &format!("/jails/{name}"));
    dispatch(target, "DELETE", &path, "").map(|_| String::new())
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
    parse_response(&response)
}

fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> Result<String, String> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(response).map_err(|e| format!("malformed response: {e}"))? {
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

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p keelctl`
Expected: all 7 tests pass (3 inherited unchanged, 4 new routed-mode tests).

- [ ] **Step 6: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass; the pre-existing `apply_get_delete_round_trip`, `apply_rejects_a_file_with_an_invalid_spec`, and `get_lists_multiple_applied_jails` tests are unaffected (they pass no `--control-plane-addr`/`--node`, exercising `Target::Socket` exactly as `Target` behaved implicitly before this change).

- [ ] **Step 7: Commit**

```bash
git add keelctl/Cargo.toml keelctl/src/main.rs keelctl/tests/cli.rs
git commit -m "Add keelctl --control-plane-addr/--node: route requests through keel-controlplane"
```

---

### Task 7: FreeBSD VM verification across three real nodes

**Files:** none expected (verification only), unless the VM run surfaces a real bug — if so, fix it following the same practice every prior milestone established (fix on macOS with a regression test, re-verify, then re-run the affected VM steps).

- [ ] **Step 1: Sync and build the repo on all three VMs**

```bash
ssh root@192.168.64.2 "cd kubsd && git pull && cargo build --release --workspace"
ssh root@192.168.64.4 "cd kubsd && git pull && cargo build --release --workspace"
ssh root@192.168.64.5 "cd kubsd && git pull && cargo build --release --workspace"
```

Expected: all three build successfully.

- [ ] **Step 2: Start `keel-controlplane` on `.2`**

```bash
ssh root@192.168.64.2 "cd kubsd && nohup ./target/release/keel-controlplane --addr 0.0.0.0:7620 > /tmp/keel-controlplane.log 2>&1 &"
sleep 1
ssh root@192.168.64.2 "cat /tmp/keel-controlplane.log"
```

Expected: log shows `keel-controlplane: starting (addr=0.0.0.0:7620)`.

- [ ] **Step 3: Start `keel-agentd` on all three nodes with real, dialable `--advertise-addr` values**

```bash
ssh root@192.168.64.2 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-2 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.2:7621 > /tmp/keel-agentd.log 2>&1 &"
ssh root@192.168.64.4 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-4 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.4:7621 > /tmp/keel-agentd.log 2>&1 &"
ssh root@192.168.64.5 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-5 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.5:7621 > /tmp/keel-agentd.log 2>&1 &"
sleep 6
ssh root@192.168.64.2 "curl -s http://127.0.0.1:7620/nodes"
```

Expected: YAML listing all three of `node-2`, `node-4`, `node-5`, each `status: Alive`, with `addr` now showing `host:7621` (the new, real bind address, not the Milestone 7 bare-IP display string).

- [ ] **Step 4: Apply a jail spec to `node-4` through the control plane on `.2`**

```bash
ssh root@192.168.64.2 "cat > /tmp/web-route-test.yaml <<'YAML'
apiVersion: keel/v1
kind: Jail
metadata:
  name: web-route-test
spec:
  image: base/14.2-web
  command: [\"/bin/sh\", \"-c\", \"sleep 3600\"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.9.5/24
  resources:
    cpu: \"1\"
    memory: 128M
  restartPolicy: Always
YAML"
ssh root@192.168.64.2 "cd kubsd && ./target/release/keelctl --control-plane-addr 127.0.0.1:7620 --node node-4 apply -f /tmp/web-route-test.yaml"
```

Expected: apply succeeds with no error output.

- [ ] **Step 5: Confirm the jail landed on `.4` specifically, not `.2` or `.5`**

```bash
ssh root@192.168.64.4 "jls | grep keel-web-route-test && echo FOUND_ON_4"
ssh root@192.168.64.2 "jls | grep keel-web-route-test; echo exit=$?"
ssh root@192.168.64.5 "jls | grep keel-web-route-test; echo exit=$?"
ssh root@192.168.64.2 "cd kubsd && ./target/release/keelctl --control-plane-addr 127.0.0.1:7620 --node node-4 get web-route-test"
```

Expected: `.4` shows `FOUND_ON_4`; `.2` and `.5` both show `exit=1` (grep found nothing); the routed `get` returns YAML containing `running: true`.

- [ ] **Step 6: Delete it through the control plane and confirm it's gone from `.4`**

```bash
ssh root@192.168.64.2 "cd kubsd && ./target/release/keelctl --control-plane-addr 127.0.0.1:7620 --node node-4 delete web-route-test"
ssh root@192.168.64.4 "jls | grep keel-web-route-test; echo exit=$?"
```

Expected: delete succeeds with no error output; `jls` on `.4` shows `exit=1` (no match).

- [ ] **Step 7: Confirm unknown-node and dead-node rejection**

```bash
ssh root@192.168.64.2 "cd kubsd && ./target/release/keelctl --control-plane-addr 127.0.0.1:7620 --node node-missing get"
```

Expected: fails; stderr contains `unknown node`.

```bash
ssh root@192.168.64.4 "pkill keel-agentd"
sleep 16
ssh root@192.168.64.2 "cd kubsd && ./target/release/keelctl --control-plane-addr 127.0.0.1:7620 --node node-4 get"
```

Expected: fails (16s comfortably exceeds the 15s `DEAD_THRESHOLD`); stderr contains `is dead`.

- [ ] **Step 8: Confirm existing single-node Unix-socket `keelctl` usage is completely unaffected**

```bash
ssh root@192.168.64.5 "cd kubsd && cat > /tmp/web-local-test.yaml <<'YAML'
apiVersion: keel/v1
kind: Jail
metadata:
  name: web-local-test
spec:
  image: base/14.2-web
  command: [\"/bin/sh\", \"-c\", \"sleep 3600\"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.9.6/24
  resources:
    cpu: \"1\"
    memory: 128M
  restartPolicy: Always
YAML
./target/release/keelctl apply -f /tmp/web-local-test.yaml
./target/release/keelctl get web-local-test
./target/release/keelctl delete web-local-test"
```

Expected: all three plain `keelctl` calls (no `--control-plane-addr`/`--node`) succeed exactly as in every prior milestone, on `.5`, whose `keel-agentd` was never touched by Step 7's kill.

- [ ] **Step 9: Restart `node-4`'s `keel-agentd` and confirm it re-registers**

```bash
ssh root@192.168.64.4 "cd kubsd && nohup ./target/release/keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock --node-id node-4 --control-plane-addr 192.168.64.2:7620 --advertise-addr 192.168.64.4:7621 > /tmp/keel-agentd.log 2>&1 &"
sleep 6
ssh root@192.168.64.2 "curl -s http://127.0.0.1:7620/nodes"
```

Expected: `node-4` shows `status: Alive` again, confirming Milestone 7's self-healing registration still works unaffected by this milestone's changes.

- [ ] **Step 10: Clean up all three VMs**

```bash
ssh root@192.168.64.2 "pkill keel-agentd; pkill keel-controlplane; rm -f /tmp/web-route-test.yaml"
ssh root@192.168.64.4 "pkill keel-agentd"
ssh root@192.168.64.5 "pkill keel-agentd; rm -f /tmp/web-local-test.yaml"
ssh root@192.168.64.2 "jls; zfs list -r zroot/keel"
ssh root@192.168.64.4 "jls; zfs list -r zroot/keel"
ssh root@192.168.64.5 "jls; zfs list -r zroot/keel"
```

Expected: no leftover `keel-agentd`/`keel-controlplane` processes on any VM; no leftover jails or datasets (both test jails were deleted in Steps 6 and 8).

- [ ] **Step 11: Record the outcome**

If every step above passed with no code changes needed, note in a follow-up commit message (or the final review) that Milestone 8 was VM-verified on this date across all three nodes: routed apply/get/delete landing specifically on `node-4`, unknown-node and dead-node rejection, and unaffected single-node Unix-socket usage on `.5`. If any VM step surfaced a real bug, fix it on macOS with a regression test added to the relevant task's file, re-run `cargo test --workspace`, then re-run the affected VM steps before considering the milestone done.

---

## Milestone Exit Criteria

- `keel-controlplane` resolves node ids to addresses (`Registry::resolve`/`Command::Resolve`), rejecting Unknown and Dead nodes before any network call, and forwards `PUT/GET/DELETE /nodes/{id}/jails/...` byte-for-byte to the resolved node's `keel-agentd`, relaying its exact response back — with no new dependency on `keel-spec` or `keel-agentd`'s wire types.
- `keel-agentd` serves its existing jails API over a second, opt-in TCP listener (bound to `--advertise-addr`, now a real bind address) whenever the Milestone 7 control-plane trio of flags is set; its Unix socket and every Milestone 1-7 behavior and test are unchanged when they're absent.
- `keelctl` routes through the control plane when given `--control-plane-addr`/`--node` together, and behaves exactly as before when given neither.
- `cargo test --workspace` passes with the 17 new tests added by this milestone (3 registry + 2 worker + 5 http in `keel-controlplane`, 3 http in `keel-agentd`, 4 in `keelctl`), on top of the 122 inherited from Milestone 7 — 139 total.
- VM-verified end-to-end across all three real nodes (`.2`/`.4`/`.5`): a spec applied to `node-4` through `keel-controlplane` on `.2` lands specifically on `.4` (confirmed absent on `.2`/`.5`), routed `get`/`delete` work, unknown-node and dead-node targets are rejected with clear errors, and plain single-node `keelctl` usage remains completely unaffected throughout.
