# Milestone 8: Routing Jail Specs to a Specific Node (Sub-Project 2, Second Milestone)

Status: Approved
Date: 2026-07-12

## Context

Milestone 7 gave `keel-controlplane` a node registry: nodes register,
heartbeat, and show up as `Alive`/`Dead` in `GET /nodes`. It deliberately
stopped there, an explicit non-goal: "the control plane never sees a
`JailSpec`," and `--advertise-addr` was left as an opaque, undialed
string, its real contract deferred to "the future spec-forwarding
milestone."

This is that milestone. The README's roadmap already names it precisely:
"Routing jail specs to a specific node through the control plane." A
user picks the node (no scheduler, no bin-packing, that is Milestone 9+),
and the request to apply/get/delete a jail on that node travels through
`keel-controlplane` rather than the caller connecting to that node's
`keel-agentd` directly.

Test environment: the same three real FreeBSD VMs used for Milestone 7
(`192.168.64.2`, `.4`, `.5`), with `.2` again hosting `keel-controlplane`.

## Goals (Milestone 8)

- `keel-controlplane` gains a new route family that forwards to a named
  node's `keel-agentd`, opaque to the jail-spec body it's carrying:
  `PUT /nodes/{id}/jails/{name}`, `GET /nodes/{id}/jails`,
  `GET /nodes/{id}/jails/{name}`, `DELETE /nodes/{id}/jails/{name}`.
- `keel-agentd` gains a second, network-reachable listener for its
  existing jails API (`PUT/GET/DELETE /jails/...`), entirely opt-in and
  bound to `--advertise-addr` (which changes from an undialed display
  string to a real `host:port` bind address), serving the exact same
  route/dispatch logic already used by the Unix socket. The Unix socket
  itself, and every existing single-node workflow through it, is
  unchanged.
- `keelctl` gains two new optional flags, `--control-plane-addr` and
  `--node`, used together to route a request through the control plane
  at a chosen node instead of connecting to a local socket. Omitting
  both preserves exactly today's behavior.
- End-to-end verification across the three VMs: applying a spec to node
  `.4` through `keel-controlplane` running on `.2` actually creates the
  jail on `.4` (confirmed directly on `.4`, not `.2`), and existing
  single-node Unix-socket workflows on all three nodes are unaffected.

## Non-Goals (Milestone 8)

- **Scheduling or placement logic.** The caller always names the exact
  target node id. Bin-packing across nodes is a separate future
  milestone (README roadmap: "Scheduler").
- **Cluster-wide aggregation.** There is no route that fans out to every
  node and merges results (e.g. "list every jail in the cluster").
  Every request in this milestone targets exactly one node. This is a
  deliberate scope cut, not an oversight — partial-failure handling
  across N concurrent node calls is real complexity this milestone does
  not need in order to satisfy "route to a specific node."
- **The `JailSpec` schema, or any of `keel-spec`/`keel-jail`/`keel-zfs`/
  `keel-net`/`Reconciler`.** A node is a routing-layer concept that lives
  only in the URL path (`/nodes/{id}/...`), never in spec YAML. None of
  these crates change in this milestone.
- **`keel-controlplane` learning anything about jails.** It forwards
  bytes; it never deserializes a `JailSpec`/`JailStatus` and gains no new
  dependency on `keel-spec` or `keel-agentd`'s wire types. Its own state
  (`Registry`) is completely unchanged from Milestone 7 — this milestone
  adds HTTP surface, not state.
- **Authentication/authorization**, same-network trust continues to be
  assumed end to end (client → control plane → node), matching every
  prior milestone's trust model. Not revisited here.
- **NAT / multi-homed node addressing.** `--advertise-addr` is used
  directly as both the bind address for the new listener and the address
  the control plane dials to reach it. This is correct for the project's
  actual deployment target (flat-LAN FreeBSD VMs) and an explicit known
  limitation elsewhere.
- **Moving or re-routing an already-applied jail to a different node.**
  Only initial placement via the routed apply is in scope.

## Architecture

### `keel-agentd`: new opt-in TCP listener for the jails API

`http.rs`'s existing `route()`/`handle_apply`/`handle_get`/
`handle_delete` already operate on a transport-agnostic
`ParsedRequest { method, path, body }` — nothing about them is
Unix-socket-specific. This milestone adds a sibling entry point:

```rust
pub fn run_tcp(listener: TcpListener, commands: Sender<Command>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        thread::spawn(move || { let _ = handle_connection_tcp(stream, &commands); });
    }
}
```

structurally identical to the existing `run(UnixListener, ...)`, with a
new `handle_connection_tcp` duplicating `handle_connection`'s body
against a `TcpStream` instead of a `UnixStream`. This duplicates rather
than generalizes over a `Read + Write` trait bound, matching the
project's established preference for small parallel implementations over
premature generics — the same choice already made when
`keel-controlplane`'s `http.rs` was written as a near-exact structural
copy of `keel-agentd`'s rather than sharing code with it.

`main.rs` binds this listener only when the existing opt-in trio
(`--node-id`, `--control-plane-addr`, `--advertise-addr`) is present —
the same gate Milestone 7 already uses for registration, now also
controlling this second listener. `--advertise-addr`'s contract changes
from Milestone 7's undialed display string to a real bind address
(`TcpListener::bind(&advertise_addr)`), e.g. `192.168.64.4:7621`; this is
also the exact string sent to the control plane at registration, so the
control plane's `Registry` already has the correct dialable address with
no new field. The existing Unix socket, its `0600` permissions, and
every route on it are completely unchanged — this is a second door onto
the same `Reconciler`/`worker::Command` dispatch, not a replacement.

### `keel-controlplane`: forwarding routes

`registry.rs` gains one method:

```rust
pub enum ResolveError { Unknown(String), Dead { id: String, last_seen_secs: u64 } }
pub fn resolve(&self, id: &str, now: Instant) -> Result<String, ResolveError>
```

reusing the same Alive/Dead computation `list` already does, so the two
never drift. `Unknown` and `Dead` are reported distinctly since they
warrant different error messages, both rejected before any network call
is attempted (per the design discussion, a `Dead`-marked node is treated
as not worth dialing at all).

`worker.rs`'s `Command` enum gains `Resolve(String, Sender<Result<String,
ResolveError>>)`, handled the same one-thread-owns-`Registry` way as
`Register`/`Heartbeat`/`List`.

`http.rs`'s `route()` gains four new arms:

```rust
("PUT", ["nodes", id, "jails", name]) => handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands),
("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands),
("GET", ["nodes", id, "jails", name]) => handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands),
("DELETE", ["nodes", id, "jails", name]) => handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands),
```

`handle_forward`:
1. Sends `Command::Resolve(id)`; on `Err(Unknown)` → 404
   `"unknown node '{id}'"`; on `Err(Dead{last_seen_secs})` → 404
   `"node '{id}' is dead (last seen {last_seen_secs}s ago)"`. Neither
   case opens a socket.
2. On `Ok(addr)`, opens a `TcpStream` to `addr` with a short connect
   timeout (`TcpStream::connect_timeout`, a few seconds) and an equally
   short read timeout (`set_read_timeout`), writes a hand-rolled
   HTTP/1.1 request (method + forwarded path + `Content-Length` + body
   — the same request-building shape `keel-agentd::registration` and
   `keelctl::send_request` already use over their own `TcpStream`/
   `UnixStream`), and parses the raw response with `httparse` (already a
   dependency of this crate).
3. On success, relays the node's exact status code and body back to the
   original caller, byte for byte — `keel-controlplane` never interprets
   the body.
4. Any failure before step 3 completes (connect error, timeout,
   malformed response) becomes a `keel-controlplane`-originated
   `ErrorBody` YAML response via the existing `error_response` helper,
   reported as a 500 with a descriptive message (e.g. `"failed to reach
   node 'node-2' at 192.168.64.4:7621: connection refused"`). No new
   status codes or reason phrases are introduced.

No new dependency in either crate: `httparse` already exists in both,
and the outbound-request shape is a direct reuse of a pattern the
codebase already has twice (`registration.rs`, `keelctl::send_request`).

### `keelctl`: routed mode

Two new optional flags, parsed the same way `--socket` already is:
`--control-plane-addr <addr>` and `--node <id>`. Providing exactly one of
the two is a usage error (mirroring `keel-agentd::parse_args`'s existing
`--node-id`/`--advertise-addr` pairing check). When both are present,
`run_apply`/`run_get`/`run_delete` build the path as
`/nodes/{node}/jails/...` (instead of `/jails/...`) and dispatch through
a new `send_request_tcp` (a `TcpStream`-based sibling of the existing
`send_request`, identical request-building/`httparse`-parsing logic,
just a different stream type and target address) connecting to
`--control-plane-addr` instead of the Unix socket. When neither flag is
present, behavior is byte-for-byte identical to today.

## Error Handling

- A forwarding failure at the control plane (unknown node, dead node,
  unreachable node) never touches `Registry` state — `Resolve` is a pure
  read, same as `List`, so a bad forward attempt can't corrupt or affect
  any other node's entry, continuing the "one bad actor doesn't block
  the others" principle already established by `Reconciler::reconcile`
  and Milestone 7's `Register`/`Heartbeat`/`List`.
- The connect/read timeout on the forwarding hop exists specifically for
  the gap between "a node goes unreachable" and "the registry's
  `DEAD_THRESHOLD` (15s) notices" — without it, a request to a node that
  died moments ago would hang for the OS's default TCP timeout instead
  of failing fast with a clear error.
- `keel-agentd`'s existing `handle_apply` path-name-consistency check
  (`spec.metadata.name` must match the path segment) fires exactly as it
  does today, unmodified, on whichever listener (Unix socket or new TCP
  listener) received the request — `keel-controlplane` forwards the body
  opaquely, so this validation still happens exactly once, at the node.

## Testing Strategy

- `Registry::resolve`: unit tests for the fresh-Unknown, Alive, and Dead
  cases, injecting `Instant`s the same way `list`'s existing
  `DEAD_THRESHOLD`-boundary tests already do — no networking involved.
- `keel-controlplane`'s `handle_forward`: in-process tests that bind a
  real local `TcpListener` as a stand-in "fake remote `keel-agentd`"
  returning a canned response, register it in the `Registry` at that
  ephemeral address, and verify: a successful forward relays status and
  body correctly; an address nothing is listening on produces a fast
  timeout error; unknown and dead node ids are rejected without any
  connection attempt.
- `keel-agentd`'s `run_tcp`: the same in-process pattern already used for
  its Unix-socket `http.rs` tests (a real bound listener, hit with real
  socket connections, not direct function calls), just with
  `TcpListener`/`TcpStream` in place of `UnixListener`/`UnixStream`,
  confirming it serves the identical route set.
- `keelctl`'s new flag-pairing validation: unit tests mirroring
  `keel-agentd::parse_args`'s existing `--node-id`/`--advertise-addr`
  pairing tests.
- VM verification (the real proof, same discipline as every prior
  milestone), across all three VMs: apply a spec to node `.4` through
  `keel-controlplane` on `.2`, confirm the jail exists on `.4` specifically
  (not `.2` or `.5`); `get`/`delete` the same way; attempt to route to a
  node that's been killed and confirm the immediate Dead-node rejection;
  attempt to route to an unknown node id; confirm existing single-node
  Unix-socket `keelctl` usage on all three nodes is completely
  unaffected throughout.

## Open Questions / Deferred Decisions

- Cluster-wide listing (aggregating `GET` across every `Alive` node) was
  considered and explicitly deferred — a natural fit for a later
  milestone, likely alongside whatever surfaces cluster state to a human
  (a dashboard, or the scheduler's own view of the world), once there's
  a real need to reason about placement across nodes rather than just
  reach one.
- Authentication between client, control plane, and node remains
  unaddressed, same as Milestone 7's deferral — worth revisiting once
  the control plane is doing something more consequential than
  forwarding to a single named node.
- Whether `keel-controlplane` needs its own `rc.d` script is still
  deferred (Milestone 7's same open question); it continues to run in
  the foreground for this milestone's VM verification.
