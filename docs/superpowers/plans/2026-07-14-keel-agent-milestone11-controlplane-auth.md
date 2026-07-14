# Milestone 11: Control-Plane Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Require a single shared-secret bearer token on every network-facing request across `keel-controlplane` and `keel-agentd`'s opt-in TCP listener, so an unauthenticated actor on the LAN can no longer register fake nodes, spoof heartbeats, or apply/delete jails.

**Architecture:** A small `auth` module (identical shape in both crates) provides `load_token`/`check`/`constant_time_eq`. `keel-controlplane`'s single `route()` and `keel-agentd`'s new `route_authenticated()` wrapper both call `auth::check` before dispatching to any handler. Every outbound call this project already makes (registration/heartbeat, control-plane-to-node forwarding, `keelctl`'s routed mode) attaches the same token as an `Authorization: Bearer <token>` header. `keel-agentd`'s Unix socket keeps calling the original, unwrapped `route()` and is completely unaffected.

**Tech Stack:** Rust, `httparse`, `serde_yaml`, std library only (no new crate dependencies).

## Global Constraints

- No new dependencies. The constant-time comparison is hand-rolled, not pulled from a crate.
- Single shared cluster secret only: no per-node/per-client tokens, no revocation, no rotation/live-reload.
- `keel-agentd`'s Unix socket (`run`/`handle_connection`/`route`) must end this plan **byte-for-byte unchanged** — verified by an explicit task step that runs its existing tests without modification.
- `401` uses reason phrase `"Unauthorized"` in both `http.rs` files' `reason_phrase()`.
- Missing/invalid auth is always a generic `401` with body `"missing or invalid auth token"` — never distinguished from a 404 or any other error, and checked before any other request validation.
- `keel-controlplane`/`keel-agentd` fail fast with `panic!` on bad startup config (missing/unreadable token file); `keelctl` returns a graceful `Err`/`ExitCode::FAILURE` instead, matching each binary's existing convention.
- Spec reference: `docs/superpowers/specs/2026-07-14-keel-agent-milestone11-controlplane-auth-design.md`.

---

### Task 1: `keel-controlplane::auth` module

**Files:**
- Create: `keel-controlplane/src/auth.rs`
- Modify: `keel-controlplane/src/lib.rs` (add `pub mod auth;`)

**Interfaces:**
- Produces: `keel_controlplane::auth::load_token(path: &std::path::Path) -> Result<String, String>`
- Produces: `keel_controlplane::auth::check(provided: Option<&str>, expected: &str) -> bool`
- Produces (crate-private): `constant_time_eq(a: &[u8], b: &[u8]) -> bool`

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/auth.rs` with only the test module (no implementation yet):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_accepts_the_correct_token_with_bearer_prefix() {
        assert!(check(Some("Bearer secret123"), "secret123"));
    }

    #[test]
    fn check_accepts_the_correct_token_without_bearer_prefix() {
        assert!(check(Some("secret123"), "secret123"));
    }

    #[test]
    fn check_rejects_a_wrong_token() {
        assert!(!check(Some("Bearer wrong"), "secret123"));
    }

    #[test]
    fn check_rejects_a_missing_header() {
        assert!(!check(None, "secret123"));
    }

    #[test]
    fn load_token_trims_trailing_whitespace_and_newline() {
        let dir = std::env::temp_dir().join(format!("keel-controlplane-auth-test-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "secret123\n").unwrap();
        assert_eq!(load_token(&path).unwrap(), "secret123");
    }

    #[test]
    fn load_token_on_an_empty_file_returns_an_empty_token() {
        let dir = std::env::temp_dir().join(format!("keel-controlplane-auth-test-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_token(&path).unwrap(), "");
    }

    #[test]
    fn load_token_on_a_missing_file_returns_an_error() {
        let path = std::env::temp_dir().join("keel-controlplane-auth-test-missing-file-does-not-exist");
        assert!(load_token(&path).is_err());
    }

    #[test]
    fn constant_time_eq_accepts_equal_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn constant_time_eq_rejects_same_length_different_content() {
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane auth::`
Expected: compile error — `cannot find function 'check'`/`'load_token'`/`'constant_time_eq'` in this scope.

- [ ] **Step 3: Write the implementation**

Add above the test module in `keel-controlplane/src/auth.rs`:

```rust
use std::path::Path;

pub fn load_token(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read auth token file {}: {e}", path.display()))
}

pub fn check(provided: Option<&str>, expected: &str) -> bool {
    let Some(provided) = provided else { return false };
    let provided = provided.strip_prefix("Bearer ").unwrap_or(provided);
    constant_time_eq(provided.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

Add `pub mod auth;` to `keel-controlplane/src/lib.rs:1` (alongside the existing `pub mod http;` etc.).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane auth::`
Expected: 9 tests pass.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/auth.rs keel-controlplane/src/lib.rs
git commit -m "Add shared-secret auth module to keel-controlplane"
```

---

### Task 2: `keel-agentd::auth` module

**Files:**
- Create: `keel-agentd/src/auth.rs`
- Modify: `keel-agentd/src/lib.rs` (add `pub mod auth;`)

**Interfaces:**
- Produces: `keel_agentd::auth::load_token(path: &std::path::Path) -> Result<String, String>`
- Produces: `keel_agentd::auth::check(provided: Option<&str>, expected: &str) -> bool`

Structurally identical to Task 1, duplicated per-crate rather than shared (matching this project's established preference for small parallel implementations, the same choice already made for `keel-controlplane`'s `http.rs` vs. `keel-agentd`'s).

- [ ] **Step 1: Write the failing tests**

Create `keel-agentd/src/auth.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_accepts_the_correct_token_with_bearer_prefix() {
        assert!(check(Some("Bearer secret123"), "secret123"));
    }

    #[test]
    fn check_accepts_the_correct_token_without_bearer_prefix() {
        assert!(check(Some("secret123"), "secret123"));
    }

    #[test]
    fn check_rejects_a_wrong_token() {
        assert!(!check(Some("Bearer wrong"), "secret123"));
    }

    #[test]
    fn check_rejects_a_missing_header() {
        assert!(!check(None, "secret123"));
    }

    #[test]
    fn load_token_trims_trailing_whitespace_and_newline() {
        let dir = std::env::temp_dir().join(format!("keel-agentd-auth-test-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "secret123\n").unwrap();
        assert_eq!(load_token(&path).unwrap(), "secret123");
    }

    #[test]
    fn load_token_on_an_empty_file_returns_an_empty_token() {
        let dir = std::env::temp_dir().join(format!("keel-agentd-auth-test-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_token(&path).unwrap(), "");
    }

    #[test]
    fn load_token_on_a_missing_file_returns_an_error() {
        let path = std::env::temp_dir().join("keel-agentd-auth-test-missing-file-does-not-exist");
        assert!(load_token(&path).is_err());
    }

    #[test]
    fn constant_time_eq_accepts_equal_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn constant_time_eq_rejects_same_length_different_content() {
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd auth::`
Expected: compile error — functions not found.

- [ ] **Step 3: Write the implementation**

Add above the test module in `keel-agentd/src/auth.rs` (identical to Task 1's implementation):

```rust
use std::path::Path;

pub fn load_token(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read auth token file {}: {e}", path.display()))
}

pub fn check(provided: Option<&str>, expected: &str) -> bool {
    let Some(provided) = provided else { return false };
    let provided = provided.strip_prefix("Bearer ").unwrap_or(provided);
    constant_time_eq(provided.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

Add `pub mod auth;` to `keel-agentd/src/lib.rs:1` (alongside the existing `pub mod backoff;` etc.).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd auth::`
Expected: 9 tests pass.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/auth.rs keel-agentd/src/lib.rs
git commit -m "Add shared-secret auth module to keel-agentd"
```

---

### Task 3: `keel-controlplane::http` — inbound enforcement

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `keel_controlplane::auth::check` from Task 1.
- Produces: `route(request: &ParsedRequest, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>)`, `run(listener: TcpListener, commands: Sender<Command>, token: std::sync::Arc<String>)` — both signatures change; `forward`/`handle_forward`/`handle_scheduled_*` keep their Task-2-era signatures for now (Task 4 threads `token` into them).

- [ ] **Step 1: Write the failing tests**

In `keel-controlplane/src/http.rs`'s `#[cfg(test)] mod tests` block, add a `TEST_TOKEN` constant near the top of the module and a new `send_request_raw` helper alongside the existing `send_request`:

```rust
const TEST_TOKEN: &str = "test-token-123";

fn send_request_raw(addr: &str, method: &str, path: &str, body: &str, auth_header: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let auth_line = match auth_header {
        Some(h) => format!("Authorization: {h}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth_line}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
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
fn register_without_auth_header_returns_401() {
    let addr = start_test_server();
    let (status, _) = send_request_raw(
        &addr,
        "POST",
        "/nodes/register",
        "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
        None,
    );
    assert_eq!(status, 401);
}

#[test]
fn heartbeat_with_wrong_auth_token_returns_401() {
    let addr = start_test_server();
    send_request(
        &addr,
        "POST",
        "/nodes/register",
        "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
    );
    let (status, _) = send_request_raw(
        &addr,
        "POST",
        "/nodes/node-1/heartbeat",
        "committed_cpu: 0\ncommitted_memory: 0\n",
        Some("Bearer wrong-token"),
    );
    assert_eq!(status, 401);
}

#[test]
fn get_nodes_without_auth_header_returns_401() {
    let addr = start_test_server();
    let (status, _) = send_request_raw(&addr, "GET", "/nodes", "", None);
    assert_eq!(status, 401);
}

#[test]
fn named_node_forward_without_auth_header_returns_401_even_for_an_unknown_node() {
    let addr = start_test_server();
    let (status, body) = send_request_raw(&addr, "GET", "/nodes/missing/jails", "", None);
    assert_eq!(status, 401, "auth must be checked before route dispatch, got body: {body}");
}

#[test]
fn scheduled_apply_without_auth_header_returns_401() {
    let addr = start_test_server();
    let (status, _) = send_request_raw(&addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n", None);
    assert_eq!(status, 401);
}
```

Also update the existing `start_test_server` and `send_request` helpers (further down in the same test module) so every *other* existing test keeps passing once auth is enforced:

```rust
fn start_test_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
    let token = Arc::new(TEST_TOKEN.to_string());
    thread::spawn(move || run(listener, commands, token));
    addr
}

fn send_request(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TEST_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --lib`
Expected: compile errors (`run` takes 2 arguments, found 3) — every existing test using `start_test_server`/`send_request` breaks until Step 3 lands, and the 5 new tests don't exist as passing behavior yet.

- [ ] **Step 3: Write the implementation**

In `keel-controlplane/src/http.rs`:

1. Add imports at the top: `use crate::auth;` and `use std::sync::Arc;`.
2. `ParsedRequest` (currently at line 21) gains a field:

```rust
struct ParsedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
    auth_header: Option<String>,
}
```

3. `read_request` (currently lines 36-83): capture the header in the same match arm that already extracts `content_length`, and thread it through the loop's break tuple:

```rust
fn read_request(stream: &mut TcpStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length, auth_header) = loop {
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
                let auth_header = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(|v| v.trim().to_string());
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length, auth_header);
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
    Ok(Some(ParsedRequest { method, path, body, auth_header }))
}
```

4. `run` and `handle_connection` (currently lines 11-34) gain `token`:

```rust
pub fn run(listener: TcpListener, commands: Sender<Command>, token: Arc<String>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let token = Arc::clone(&token);
        thread::spawn(move || {
            let _ = handle_connection(stream, &commands, &token);
        });
    }
}

fn handle_connection(mut stream: TcpStream, commands: &Sender<Command>, token: &str) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands, token);
    write_response(&mut stream, status, &body)
}
```

5. `route` (currently line 108) gains the auth check as its first statement:

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
    if !auth::check(request.auth_header.as_deref(), token) {
        return error_response(401, "missing or invalid auth token".to_string());
    }
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        // ... existing match arms, unchanged for this task ...
    }
}
```

6. `reason_phrase` (currently line 96) gains `401 => "Unauthorized",`.

Note: this task does **not** yet change `handle_forward`/`forward`/`handle_scheduled_*`'s signatures — they keep calling `forward()` without a token for now, so the outbound leg is still unauthenticated until Task 4. `route()` compiles and passes today's tests because it only checks the *inbound* header before dispatching.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --lib`
Expected: all tests in `http.rs` pass, including the 5 new ones.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Enforce shared-secret auth on all keel-controlplane inbound routes"
```

---

### Task 4: `keel-controlplane::http` — outbound forwarding attaches the token

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `token: &str` (available in `route()` since Task 3).
- Produces: `forward(addr: &str, method: &str, path: &str, body: &[u8], token: &str) -> Result<(u16, Vec<u8>), String>`, `handle_forward(id: &str, method: &str, path: &str, body: &[u8], commands: &Sender<Command>, token: &str)`, `handle_scheduled_apply(name: &str, body: &[u8], commands: &Sender<Command>, token: &str)`, `handle_scheduled_read(name: &str, commands: &Sender<Command>, token: &str)`, `handle_scheduled_delete(name: &str, commands: &Sender<Command>, token: &str)`.

- [ ] **Step 1: Write the failing tests**

Add a capturing variant of the fake remote agent and two new tests to `keel-controlplane/src/http.rs`'s test module. Add `use std::sync::Mutex;` to the test module's imports, alongside the existing `use super::*;`:

```rust
fn start_fake_remote_agentd_capturing(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<u8>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let mut received = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                }
            }
            *captured_clone.lock().unwrap() = received;
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (addr, captured)
}

#[test]
fn named_node_forward_attaches_the_control_planes_auth_token_to_the_outbound_request() {
    let cp_addr = start_test_server();
    let (node_addr, captured) = start_fake_remote_agentd_capturing(200, "running: true\n");
    register_node(&cp_addr, "node-1", &node_addr);

    send_request(&cp_addr, "PUT", "/nodes/node-1/jails/web-1", "apiVersion: keel/v1\n");

    let received = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        received.contains(&format!("Authorization: Bearer {TEST_TOKEN}")),
        "expected relayed request to carry the control plane's own auth token, got: {received}"
    );
}

#[test]
fn scheduled_apply_attaches_the_control_planes_auth_token_to_the_outbound_request() {
    let cp_addr = start_test_server();
    let (node_addr, captured) = start_fake_remote_agentd_capturing(200, "node: node-a\n");
    register_node(&cp_addr, "node-a", &node_addr);

    send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");

    let received = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        received.contains(&format!("Authorization: Bearer {TEST_TOKEN}")),
        "expected relayed request to carry the control plane's own auth token, got: {received}"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --lib forward`
Expected: the two new tests fail (`assertion failed`, the captured request has no `Authorization` header yet).

- [ ] **Step 3: Write the implementation**

In `keel-controlplane/src/http.rs`:

1. `forward` (currently line 288) gains `token` and adds the header:

```rust
fn forward(addr: &str, method: &str, path: &str, body: &[u8], token: &str) -> Result<(u16, Vec<u8>), String> {
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "could not resolve address".to_string())?;
    let mut stream =
        TcpStream::connect_timeout(&socket_addr, FORWARD_CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(FORWARD_READ_TIMEOUT)).ok();

    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
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

2. `handle_forward` (currently line 272) and the three scheduled handlers (currently lines 140-187) gain `token` and pass it through to `forward`:

```rust
fn handle_forward(id: &str, method: &str, path: &str, body: &[u8], commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Resolve(id.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let addr = match reply_rx.recv() {
        Ok(Ok(addr)) => addr,
        Ok(Err(e)) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, method, path, body, token) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_apply(name: &str, body: &[u8], commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
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
    match forward(&addr, "PUT", &format!("/jails/{name}"), body, token) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_record_placement(name, &node_id, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_read(name: &str, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "GET", &format!("/jails/{name}"), &[], token) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_delete(name: &str, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "DELETE", &format!("/jails/{name}"), &[], token) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}
```

3. `route`'s match arms (currently lines 111-137) pass `token` through to every one of the above:

```rust
match (request.method.as_str(), segments.as_slice()) {
    ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
    ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, &request.body, commands),
    ("GET", ["nodes"]) => handle_list(commands),
    ("PUT", ["nodes", id, "jails", name]) => {
        let (status, body) = handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands, token);
        if (200..300).contains(&status) {
            send_record_placement(name, id, commands);
        }
        (status, body)
    }
    ("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands, token),
    ("GET", ["nodes", id, "jails", name]) => {
        handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands, token)
    }
    ("DELETE", ["nodes", id, "jails", name]) => {
        let (status, body) = handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands, token);
        if (200..300).contains(&status) {
            send_remove_placement(name, commands);
        }
        (status, body)
    }
    ("PUT", ["jails", name]) => handle_scheduled_apply(name, &request.body, commands, token),
    ("GET", ["jails", name]) => handle_scheduled_read(name, commands, token),
    ("DELETE", ["jails", name]) => handle_scheduled_delete(name, commands, token),
    _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
}
```

(`handle_register`/`handle_heartbeat`/`handle_list` are untouched — they never forward anywhere.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --lib`
Expected: all tests pass, including the 2 new ones and every pre-existing forwarding/scheduling test (`forward_put_relays_status_and_body_from_the_target_node`, `scheduled_put_lands_on_the_lower_id_node_when_headroom_is_equal`, etc.) unmodified.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Attach the shared auth token to keel-controlplane's outbound forwarding"
```

---

### Task 5: `keel-controlplane/main.rs` — flag parsing and token wiring

**Files:**
- Modify: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `keel_controlplane::auth::load_token` (Task 1), `keel_controlplane::http::run(listener, commands, token: Arc<String>)` (Task 3).

- [ ] **Step 1: Write the failing tests**

Add a test module to `keel-controlplane/src/main.rs` (it currently has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_the_auth_token_file_flag() {
        let config = parse_args_from(args(&["--auth-token-file", "/etc/keel/token"]));
        assert_eq!(config.auth_token_file, Some(PathBuf::from("/etc/keel/token")));
    }

    #[test]
    #[should_panic(expected = "--auth-token-file is required")]
    fn missing_auth_token_file_panics() {
        parse_args_from(args(&["--addr", "0.0.0.0:7620"]));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --bin keel-controlplane`
Expected: compile error — no `parse_args_from` function, no `auth_token_file` field, no `PathBuf` import.

- [ ] **Step 3: Write the implementation**

Replace the whole of `keel-controlplane/src/main.rs` above the (new) test module with:

```rust
use keel_controlplane::placements::Placements;
use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

struct Config {
    addr: String,
    auth_token_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self { addr: "0.0.0.0:7620".to_string(), auth_token_file: None }
    }
}

fn parse_args() -> Config {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Config {
    let mut config = Config::default();
    let mut args = args;
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--addr" => config.addr = value,
            "--auth-token-file" => config.auth_token_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.auth_token_file.is_none() {
        panic!("--auth-token-file is required");
    }
    config
}

fn main() {
    let config = parse_args();
    let auth_token_file = config.auth_token_file.expect("validated as required in parse_args_from");
    let token = keel_controlplane::auth::load_token(&auth_token_file)
        .unwrap_or_else(|e| panic!("failed to load auth token: {e}"));
    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, Arc::new(token));
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --bin keel-controlplane`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/main.rs
git commit -m "Require --auth-token-file on keel-controlplane startup"
```

---

### Task 6: `keel-agentd::http` — TCP-listener enforcement, Unix socket untouched

**Files:**
- Modify: `keel-agentd/src/http.rs`

**Interfaces:**
- Consumes: `keel_agentd::auth::check` (Task 2).
- Produces: `route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>)` (unchanged behavior, used by the Unix socket only), `route_authenticated(request: &ParsedRequest, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>)` (new, used by TCP only), `run_tcp(listener: TcpListener, commands: Sender<Command>, token: std::sync::Arc<String>)`.

- [ ] **Step 1: Write the failing tests**

Update `keel-agentd/src/http.rs`'s test module: add a `TEST_TOKEN` constant, update `start_tcp_test_server`/`send_request_tcp` to use it, and add two new 401 tests. Leave every Unix-socket test (`start_test_server`, `send_request`, and all tests that use them) completely untouched.

```rust
const TEST_TOKEN: &str = "test-token-123";

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
    let token = Arc::new(TEST_TOKEN.to_string());
    thread::spawn(move || run_tcp(listener, commands, token));
    addr
}

fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TEST_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
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

fn send_request_tcp_raw(addr: &str, method: &str, path: &str, body: &str, auth_header: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let auth_line = match auth_header {
        Some(h) => format!("Authorization: {h}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth_line}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
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
fn put_over_tcp_without_auth_header_returns_401() {
    let addr = start_tcp_test_server("put_over_tcp_without_auth_header_returns_401");
    let (status, _) = send_request_tcp_raw(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"), None);
    assert_eq!(status, 401);
}

#[test]
fn get_jails_over_tcp_with_wrong_auth_token_returns_401() {
    let addr = start_tcp_test_server("get_jails_over_tcp_with_wrong_auth_token_returns_401");
    let (status, _) = send_request_tcp_raw(&addr, "GET", "/jails", "", Some("Bearer wrong-token"));
    assert_eq!(status, 401);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd --lib http::`
Expected: compile error (`run_tcp` takes 2 arguments, found 3) across the existing TCP tests, and the 2 new tests don't pass yet.

- [ ] **Step 3: Write the implementation**

In `keel-agentd/src/http.rs`:

1. Add imports: `use crate::auth;` and `use std::sync::Arc;`.
2. `ParsedRequest` (currently line 33) gains `auth_header: Option<String>`, added to **both** `read_request` and `read_request_tcp` (currently lines 57-104 and 106-153), so the struct stays a single shared type:

```rust
fn read_request(stream: &mut UnixStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length, auth_header) = loop {
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
                let auth_header = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(|v| v.trim().to_string());
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length, auth_header);
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
    Ok(Some(ParsedRequest { method, path, body, auth_header }))
}

fn read_request_tcp(stream: &mut TcpStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length, auth_header) = loop {
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
                let auth_header = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(|v| v.trim().to_string());
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length, auth_header);
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
    Ok(Some(ParsedRequest { method, path, body, auth_header }))
}
```

Both now return the same widened `ParsedRequest { method, path, body, auth_header }`.
3. `route_authenticated` is added as a new function, and `run_tcp`/`handle_connection_tcp` (currently lines 23-31 and 48-55) are updated to use it; `run`/`handle_connection` (currently lines 13-21 and 39-46), used by the Unix socket, are **not modified**:

```rust
pub fn run_tcp(listener: TcpListener, commands: Sender<Command>, token: Arc<String>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let token = Arc::clone(&token);
        thread::spawn(move || {
            let _ = handle_connection_tcp(stream, &commands, &token);
        });
    }
}

fn handle_connection_tcp(mut stream: TcpStream, commands: &Sender<Command>, token: &str) -> io::Result<()> {
    let request = match read_request_tcp(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route_authenticated(&request, commands, token);
    write_response_tcp(&mut stream, status, &body)
}
```

4. `route` (currently line 188) is left completely unchanged in body; a new `route_authenticated` wraps it:

```rust
fn route_authenticated(request: &ParsedRequest, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
    if !auth::check(request.auth_header.as_deref(), token) {
        return error_response(401, "missing or invalid auth token".to_string());
    }
    route(request, commands)
}
```

5. `reason_phrase` (currently line 177) gains `401 => "Unauthorized",`.

- [ ] **Step 4: Run tests to verify they pass, and prove the Unix socket path is untouched**

Run: `cargo test -p keel-agentd --lib http::`
Expected: all tests pass, including the existing Unix-socket tests (`put_valid_spec_returns_200_and_provisions_the_jail`, `put_with_mismatched_path_and_body_name_returns_400`, etc.) with **zero changes to their source** — confirming the Non-Goal that the Unix socket is unaffected.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/http.rs
git commit -m "Enforce shared-secret auth on keel-agentd's TCP listener only"
```

---

### Task 7: `keel-agentd::registration` — outbound header

**Files:**
- Modify: `keel-agentd/src/registration.rs`

**Interfaces:**
- Consumes: `keel_controlplane::http::run(listener, commands, token: Arc<String>)` (Task 3, used by this file's own tests), `keel_agentd::auth::check` transitively via the control plane it talks to.
- Produces: `spawn(node_id: String, advertise_addr: String, control_plane_addr: String, heartbeat_interval: Duration, capacity_cpu: f64, capacity_memory: u64, token: String, commands: Sender<crate::worker::Command>) -> JoinHandle<()>` — signature gains `token: String` as the 7th parameter, before `commands`.

- [ ] **Step 1: Write the failing test**

Update `keel-agentd/src/registration.rs`'s existing test helper `start_test_control_plane` to require a token, update the two existing tests' `spawn(...)` calls to pass one, and add a new test for a mismatched token:

```rust
fn start_test_control_plane(token: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
    let token = std::sync::Arc::new(token.to_string());
    thread::spawn(move || keel_controlplane::http::run(listener, commands, token));
    addr
}

fn get_nodes(control_plane_addr: &str, token: &str) -> String {
    let mut stream = TcpStream::connect(control_plane_addr).unwrap();
    stream
        .write_all(format!("GET /nodes HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n").as_bytes())
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).to_string()
}

#[test]
fn registration_with_a_mismatched_token_never_registers() {
    let control_plane_addr = start_test_control_plane("correct-token");
    let (_worker_handle, commands) = crate::worker::spawn(
        crate::Reconciler::new(
            keel_jail::FakeJailRuntime::new(),
            keel_zfs::FakeZfsManager::new(),
            keel_net::FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join("keel-agentd-registration-test-registration_with_a_mismatched_token_never_registers"),
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
        "wrong-token".to_string(),
        commands,
    );

    thread::sleep(Duration::from_millis(200));
    let body = get_nodes(&control_plane_addr, "correct-token");
    assert!(!body.contains("node-1"), "expected node-1 to never register with a mismatched token, got: {body}");
}
```

Update the two pre-existing tests (`registers_and_then_keeps_heartbeating`, `heartbeats_report_the_reconcilers_committed_resources`) to call `start_test_control_plane("test-token")`, `get_nodes(&control_plane_addr, "test-token")`, and `spawn(..., "test-token".to_string(), commands)` (inserting the token argument before the final `commands` argument, matching the new signature).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd --lib registration::`
Expected: compile error (`spawn` takes 7 arguments, found 8, or vice versa) until Step 3 lands; the new test doesn't exist as passing behavior yet.

- [ ] **Step 3: Write the implementation**

In `keel-agentd/src/registration.rs`:

```rust
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    token: String,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr, capacity_cpu, capacity_memory, &token) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id, &commands, &token) {
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
    token: &str,
) -> Result<(), String> {
    let body = format!(
        "id: {node_id}\naddr: {advertise_addr}\ncapacity_cpu: {capacity_cpu}\ncapacity_memory: {capacity_memory}\n"
    );
    send_request(control_plane_addr, "POST", "/nodes/register", &body, token)
}

fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
    token: &str,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, token)
}

fn send_request(addr: &str, method: &str, path: &str, body: &str, token: &str) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&response).map_err(|e| format!("malformed response: {e}"))? {
        httparse::Status::Complete(_) => {}
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

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd --lib registration::`
Expected: all tests pass, including the new mismatched-token test.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/registration.rs
git commit -m "Attach the shared auth token to keel-agentd's registration/heartbeat calls"
```

---

### Task 8: `keel-agentd/main.rs` — flag parsing and token wiring

**Files:**
- Modify: `keel-agentd/src/main.rs`

**Interfaces:**
- Consumes: `keel_agentd::auth::load_token` (Task 2), `keel_agentd::registration::spawn(..., token: String, commands)` (Task 7), `keel_agentd::http::run_tcp(listener, commands, token: Arc<String>)` (Task 6).

- [ ] **Step 1: Write the failing tests**

Update `keel-agentd/src/main.rs`'s existing test module: extend `Config`/`parse_args_from` assertions to include `auth_token_file`, and add/replace tests for the four-flag pairing requirement:

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
        assert_eq!(config.auth_token_file, None);
    }

    #[test]
    fn parses_all_four_new_flags() {
        let config = parse_args_from(args(&[
            "--node-id",
            "node-2",
            "--control-plane-addr",
            "192.168.64.2:7620",
            "--advertise-addr",
            "192.168.64.2",
            "--auth-token-file",
            "/etc/keel/token",
        ]));
        assert_eq!(config.node_id, Some("node-2".to_string()));
        assert_eq!(config.control_plane_addr, Some("192.168.64.2:7620".to_string()));
        assert_eq!(config.advertise_addr, Some("192.168.64.2".to_string()));
        assert_eq!(config.auth_token_file, Some(PathBuf::from("/etc/keel/token")));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, and --auth-token-file are required when --control-plane-addr is set")]
    fn control_plane_addr_without_node_id_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--advertise-addr", "192.168.64.2",
            "--auth-token-file", "/etc/keel/token",
        ]));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, and --auth-token-file are required when --control-plane-addr is set")]
    fn control_plane_addr_without_advertise_addr_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--node-id", "node-2",
            "--auth-token-file", "/etc/keel/token",
        ]));
    }

    #[test]
    #[should_panic(expected = "--node-id, --advertise-addr, and --auth-token-file are required when --control-plane-addr is set")]
    fn control_plane_addr_without_auth_token_file_panics() {
        parse_args_from(args(&[
            "--control-plane-addr", "192.168.64.2:7620",
            "--node-id", "node-2",
            "--advertise-addr", "192.168.64.2",
        ]));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd --bin keel-agentd`
Expected: compile error (`auth_token_file` field doesn't exist) and the old panic-message tests fail on message text once they do compile.

- [ ] **Step 3: Write the implementation**

In `keel-agentd/src/main.rs`:

1. `Config` and its `Default` gain `auth_token_file: Option<PathBuf>` (default `None`), added to the existing `node_id`/`control_plane_addr`/`advertise_addr` fields.
2. `parse_args_from`'s match gains `"--auth-token-file" => config.auth_token_file = Some(PathBuf::from(value)),`, and the trailing pairing check becomes:

```rust
if config.control_plane_addr.is_some()
    && (config.node_id.is_none() || config.advertise_addr.is_none() || config.auth_token_file.is_none())
{
    panic!("--node-id, --advertise-addr, and --auth-token-file are required when --control-plane-addr is set");
}
```

3. `main()`'s control-plane block gains token loading and passes it to both `registration::spawn` and the TCP listener's `run_tcp`:

```rust
if let (Some(node_id), Some(control_plane_addr), Some(advertise_addr), Some(auth_token_file)) = (
    config.node_id.clone(),
    config.control_plane_addr.clone(),
    config.advertise_addr.clone(),
    config.auth_token_file.clone(),
) {
    let (capacity_cpu, capacity_memory) = keel_agentd::capacity::detect()
        .unwrap_or_else(|e| panic!("failed to detect node capacity via sysctl: {e}"));
    let token = keel_agentd::auth::load_token(&auth_token_file)
        .unwrap_or_else(|e| panic!("failed to load auth token: {e}"));
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
        token.clone(),
        commands.clone(),
    );

    eprintln!("keel-agentd: serving jails API over TCP on {advertise_addr}");
    let tcp_listener = TcpListener::bind(&advertise_addr)
        .unwrap_or_else(|e| panic!("failed to bind jails-API TCP listener on {advertise_addr}: {e}"));
    let tcp_commands = commands.clone();
    thread::spawn(move || keel_agentd::http::run_tcp(tcp_listener, tcp_commands, std::sync::Arc::new(token)));
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd --bin keel-agentd`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/main.rs
git commit -m "Require --auth-token-file on keel-agentd's control-plane path"
```

---

### Task 9: `keelctl` — flag parsing and outbound header

**Files:**
- Modify: `keelctl/src/main.rs`

**Interfaces:**
- Produces: `resolve_target(socket: PathBuf, control_plane_addr: Option<String>, node: Option<String>, auth_token_file: Option<String>) -> Result<Target, String>`, `Target::ControlPlane { addr: String, node: Option<String>, token: String }` (gains a field), `send_request_tcp(addr: &str, method: &str, path: &str, body: &str, token: &str) -> Result<String, String>` (gains a parameter).

- [ ] **Step 1: Write the failing tests**

Add a test module to `keelctl/src/main.rs` (it currently has none), and derive `Debug, PartialEq` on `Target`:

```rust
#[derive(Debug, PartialEq)]
enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: Option<String>, token: String },
}
```

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_control_plane_flags_yields_socket_target() {
        let target = resolve_target(PathBuf::from("/var/run/keel-agentd.sock"), None, None, None).unwrap();
        assert_eq!(target, Target::Socket(PathBuf::from("/var/run/keel-agentd.sock")));
    }

    #[test]
    fn node_without_control_plane_addr_is_an_error() {
        let err = resolve_target(PathBuf::from("/var/run/keel-agentd.sock"), None, Some("node-1".to_string()), None)
            .unwrap_err();
        assert_eq!(err, "--node requires --control-plane-addr");
    }

    #[test]
    fn control_plane_addr_without_auth_token_file_is_an_error() {
        let err = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            Some("10.0.0.1:7620".to_string()),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, "--auth-token-file is required with --control-plane-addr");
    }

    #[test]
    fn control_plane_addr_with_auth_token_file_reads_the_token() {
        let dir = std::env::temp_dir().join(format!("keelctl-resolve-target-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_path = dir.join("token");
        std::fs::write(&token_path, "secret123\n").unwrap();

        let target = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            Some("10.0.0.1:7620".to_string()),
            Some("node-1".to_string()),
            Some(token_path.to_str().unwrap().to_string()),
        )
        .unwrap();
        assert_eq!(
            target,
            Target::ControlPlane {
                addr: "10.0.0.1:7620".to_string(),
                node: Some("node-1".to_string()),
                token: "secret123".to_string(),
            }
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keelctl`
Expected: compile error — no `resolve_target` function, `Target` doesn't derive `Debug`/`PartialEq`, `ControlPlane` has no `token` field yet.

- [ ] **Step 3: Write the implementation**

In `keelctl/src/main.rs`:

1. `Target` gains the `token` field and derives, as shown in Step 1 above.
2. New function `resolve_target`:

```rust
fn resolve_target(
    socket: PathBuf,
    control_plane_addr: Option<String>,
    node: Option<String>,
    auth_token_file: Option<String>,
) -> Result<Target, String> {
    match (control_plane_addr, node, auth_token_file) {
        (Some(addr), node, Some(path)) => {
            let token = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {path}: {e}"))?
                .trim()
                .to_string();
            Ok(Target::ControlPlane { addr, node, token })
        }
        (Some(_), _, None) => Err("--auth-token-file is required with --control-plane-addr".to_string()),
        (None, Some(_), _) => Err("--node requires --control-plane-addr".to_string()),
        (None, None, _) => Ok(Target::Socket(socket)),
    }
}
```

3. `main()`'s target-resolution block (currently lines 17-29) becomes:

```rust
fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let control_plane_addr = extract_flag(&mut args, "--control-plane-addr");
    let node = extract_flag(&mut args, "--node");
    let auth_token_file = extract_flag(&mut args, "--auth-token-file");

    let target = match resolve_target(socket, control_plane_addr, node, auth_token_file) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::FAILURE;
        }
    };

    // ... existing `match args.split_first() { ... }` dispatch, unchanged ...
}
```

4. `dispatch` and `send_request_tcp` (currently lines 73-78 and 117-127) thread the token through; `jails_path` (currently lines 65-71) needs **no change** — its `Target::ControlPlane { node: Some(node), .. }`/`{ node: None, .. }` patterns already absorb the new `token` field via `..`:

```rust
fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, token, .. } => send_request_tcp(addr, method, path, body, token),
    }
}

fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str, token: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;
    parse_response(&response)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keelctl`
Expected: all 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add keelctl/src/main.rs
git commit -m "Require --auth-token-file on keelctl's control-plane-routed mode"
```

---

### Task 10: Full workspace test run + VM verification

**Files:** none (verification only).

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: every test across all crates passes (this is the first point all nine prior tasks' changes are checked together).

- [ ] **Step 2: Generate a token and distribute it**

On the operator's machine:

```bash
openssl rand -hex 32 > /tmp/keel-cluster-token
```

Copy `/tmp/keel-cluster-token` to `192.168.64.2` (`.2`), `.4`, `.5`, and the `keelctl` client machine, e.g. `scp /tmp/keel-cluster-token freebsd@192.168.64.2:/tmp/keel-cluster-token` (repeat per host).

- [ ] **Step 3: Restart the cluster with `--auth-token-file` on every process**

On `.2`: `keel-controlplane --addr 0.0.0.0:7620 --auth-token-file /tmp/keel-cluster-token` and `keel-agentd --node-id node-2 --advertise-addr 192.168.64.2:7621 --control-plane-addr 192.168.64.2:7620 --auth-token-file /tmp/keel-cluster-token` (plus its existing pool/state-dir/socket flags). On `.4`/`.5`: `keel-agentd` with the matching `--node-id`, `--advertise-addr`, `--control-plane-addr 192.168.64.2:7620 --auth-token-file /tmp/keel-cluster-token`.

- [ ] **Step 4: Confirm normal operation is unaffected**

From the client: `keelctl apply -f web-1.yaml --control-plane-addr 192.168.64.2:7620 --auth-token-file /tmp/keel-cluster-token` (no `--node`, exercising the scheduler), then `keelctl get web-1 --control-plane-addr 192.168.64.2:7620 --auth-token-file /tmp/keel-cluster-token` and `keelctl delete web-1 --control-plane-addr 192.168.64.2:7620 --auth-token-file /tmp/keel-cluster-token`.
Expected: identical behavior to Milestone 10's verification — apply lands on a node, get/delete succeed.

- [ ] **Step 5: Confirm a missing or wrong token is rejected**

Run `keelctl get web-1 --control-plane-addr 192.168.64.2:7620 --auth-token-file /tmp/nonexistent-token` (missing file) and, separately, with a token file containing the wrong value.
Expected: the first fails locally with `error: failed to read /tmp/nonexistent-token: ...` (never reaches the network); the second connects but gets `error: missing or invalid auth token` from a `401` response.

- [ ] **Step 6: Confirm a node with a stale token can't register**

On `.4`, replace its token file with a different value and restart `keel-agentd`.
Expected: `.4`'s `eprintln!` output repeats `keel-agentd: registration failed: control plane returned status 401` every ~5 seconds; `GET /nodes` on `.2` (using the correct token) never lists `.4` until its token file is fixed and it's restarted again.

- [ ] **Step 7: Update the README**

Add Milestone 11 to the README's "The journey so far" and roadmap sections, following the existing per-milestone write-up style (see Milestones 7-10's entries), and mark item on the roadmap as done.

- [ ] **Step 8: Commit**

```bash
git add README.md
git commit -m "Document Milestone 11 completion: control-plane shared-secret auth"
```
