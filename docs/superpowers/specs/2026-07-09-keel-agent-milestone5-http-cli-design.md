# Milestone 5: Local HTTP API + `keelctl` CLI

Status: Approved
Date: 2026-07-09

## Context

Milestone 4 delivered `Reconciler<J, Z, N>` as a library with a full
`new`/`apply`/`delete`/`reconcile` API, tested against `Fake*` implementations,
plus (from earlier milestones) real `ProcessJailRuntime`, `CliZfsManager`, and
`ProcessNetManager` implementations that were never wired into a running
process. `keel-agentd` is currently library-only: no binary, no way to apply a
spec against a live system, no timer driving `reconcile()`, and no operator
interface. This milestone builds the daemon binary, the local HTTP API over a
Unix socket that the [top-level design spec](2026-07-05-keel-agent-design.md)
originally called for, and the `keelctl` CLI that talks to it.

Per the README roadmap, this is item 5 of the single-node sub-project:
"Local HTTP API + CLI, wired to the real jail/ZFS/net implementations."
Item 6 (`rc.d` integration and an end-to-end smoke test) is explicitly a
separate, later milestone.

## Goals (Milestone 5)

- A `keel-agentd` binary that wires the real `ProcessJailRuntime` /
  `CliZfsManager` / `ProcessNetManager` implementations into a `Reconciler`,
  runs a timer that calls `reconcile()` on a fixed cadence, and exposes a
  local HTTP API over a Unix domain socket to `apply`/`get`/`delete` jail
  specs.
- A `keelctl` CLI binary that talks to that API, mirroring `kubectl`
  ergonomics for these three verbs.
- The single-threaded work queue deferred by the Milestone 4 spec: API-
  triggered `apply`/`delete` and the periodic timer tick are serialized
  through one worker thread that owns the `Reconciler`, so there is never
  concurrent access to it.
- The HTTP/CLI wire protocol and routing logic are testable on any OS
  (macOS/CI) against `Fake*`-backed reconcilers, with zero FreeBSD
  dependency, following the same fakes-first pattern as Milestones 1-4.
- Manual verification of the full binary (real implementations, real timer,
  real socket) on the FreeBSD VM, running in the foreground.

## Non-Goals (Milestone 5)

- `rc.d` service script, daemonization/double-fork, or any process
  supervision — Milestone 6.
- The full end-to-end smoke test on the VM — Milestone 6.
- Authentication/authorization beyond Unix socket file permissions (per the
  top-level spec's "Unix socket trust model (v1)").
- TLS, or any transport other than a local Unix socket.
- A config file — runtime config is CLI flags with defaults.
- Graceful shutdown (`SIGTERM` handling), `keelctl` output formats other than
  YAML, `keelctl` subcommands beyond `apply`/`get`/`delete`.
- Distinguishing `restartPolicy: OnFailure` from `Always`, or any other
  Reconciler behavior change — this milestone only adds a binary and API
  surface around the existing `Reconciler`, it does not change reconciliation
  logic itself.

## Architecture

Two new pieces, both added to the existing workspace:

- **`keel-agentd`** gains a binary target (`keel-agentd/src/main.rs`,
  `[[bin]] name = "keel-agentd"`) alongside its existing library target, plus
  three new library modules:
  - `worker.rs` — a single thread that owns a `Reconciler<J, Z, N>` and
    processes `Command`s received over an `mpsc` channel; nothing else ever
    touches the `Reconciler` directly.
  - `http.rs` — a generic HTTP server function, generic over the same
    `J: JailRuntime, Z: ZfsManager, N: NetManager` bounds as `Reconciler`,
    that accepts connections on a `UnixListener`, parses requests with
    `httparse`, routes them, and sends `Command`s to the worker's channel.
  - `wire.rs` — the YAML request/response types (`JailStatus`, error body)
    shared between server and client.
- **`keelctl`** — a new workspace member, a thin binary crate that builds an
  HTTP/1.1 request by hand, writes it to a `UnixStream`, and parses the
  response with `httparse`.

New dependency: `httparse` (a small, dependency-free HTTP/1.1 parser), used
by both `keel-agentd` (parsing requests) and `keelctl` (parsing responses).
No async runtime is introduced — both binaries stay synchronous, consistent
with every other crate in the workspace shelling out to CLI tools
synchronously.

## Concurrency Model

```rust
enum Command {
    Apply(JailSpec, oneshot::Sender<Result<(), ReconcileError>>),
    Get(Option<String>, oneshot::Sender<Vec<JailStatus>>),
    Delete(String, oneshot::Sender<Result<(), ReconcileError>>),
    Tick,
}
```

One worker thread owns the `Reconciler<J, Z, N>` and processes `Command`s
from an `mpsc::Receiver<Command>` one at a time, so the `Reconciler` (which
Milestone 4 built as an explicitly single-threaded type) is never accessed
concurrently. Two other threads send into that channel:

- A **timer thread** sends `Command::Tick` every 5 seconds.
- **HTTP handler threads** (one spawned per accepted connection on the
  `UnixListener`) send `Apply`/`Get`/`Delete` and block on the paired
  `oneshot` receiver for the reply.

Handling `Apply` or `Delete` in the worker calls the corresponding
`Reconciler` method and then immediately calls `reconcile(Instant::now())`
before replying, so a client's `apply`/`delete` call observes the effects of
that reconciliation pass by the time it gets its response — matching the
top-level spec's "on a timer... and immediately after any API-triggered
apply/delete." Handling `Tick` just calls `reconcile(Instant::now())` and
discards the result (failures are per-jail and already retried with backoff
on the next tick; there's no client waiting on a `Tick`'s outcome, so
nowhere useful to report them — a future milestone that adds logging output
for the daemon could log them here, but that's not part of this milestone's
listed goals).

## Wire Protocol

Real HTTP/1.1 semantics (methods, paths, status codes) over a Unix socket,
YAML bodies (reusing `keel-spec`'s existing `serde_yaml` dependency — no new
JSON dependency, and no format conversion between the YAML spec files an
operator writes and the wire format).

| Method | Path | Request body | Response |
|---|---|---|---|
| `PUT` | `/jails/{name}` | YAML `JailSpec` | `200` empty body; `400` if the path name doesn't match `spec.metadata.name` or spec validation fails; `409` if the spec changes an immutable field on an existing jail |
| `GET` | `/jails` | none | `200`, YAML list of `JailStatus` |
| `GET` | `/jails/{name}` | none | `200` YAML `JailStatus`; `404` if unknown |
| `DELETE` | `/jails/{name}` | none | `200` empty body; `404` if unknown |

```rust
struct JailStatus {
    record: JailRecord,       // spec + epair_ordinal, from keel-agentd::record
    running: bool,            // fresh JailRuntime::is_running query
    backoff: BackoffStatus,
}

struct BackoffStatus {
    next_retry_at: Option<String>,   // RFC3339, absent if no cooldown armed
    current_delay_secs: Option<u64>,
}
```

Non-2xx responses carry a YAML body `{ error: <string> }`, built from
`Display` on `ReconcileError`. Status code mapping from `ReconcileError`
variants: `InvalidSpec` → `400` (except when the underlying validation
failure is specifically an immutable-field-change rejection, which maps to
`409` since that's a more precise code for "conflicts with existing
server-side state" than a generic bad request); `NotFound` → `404`;
`Store`/`Jail`/`Zfs`/`Net` → `500`. `BaseImageNotFound` is never returned
synchronously from `apply`/`delete` (it only occurs inside `reconcile`,
which this API only calls indirectly) — it surfaces to an operator via
`keelctl get`'s `running: false` plus an armed backoff, not as an HTTP error.

`Content-Length` framing (no chunked transfer encoding) is sufficient given
the request/response sizes involved (single jail specs, small status lists).

## `keelctl` CLI

```
keelctl apply -f spec.yaml       # reads the file, PUT /jails/{name}
keelctl get [name]                # GET /jails or /jails/{name}; prints YAML
keelctl delete <name>              # DELETE /jails/{name}
```

All three take a global `--socket <path>` flag, default
`/var/run/keel-agentd.sock`. `apply` reads `metadata.name` out of the parsed
YAML file to build the request path (using `keel-spec`'s existing parser, so
a malformed file is rejected client-side with the same validation `keel-spec`
already provides, before any request is sent). `get`/`delete` take the jail
name as a positional argument (`get` with no argument lists all jails).
Non-2xx responses print the error body's `error` string to stderr and exit
non-zero.

## `keel-agentd` Binary

```
keel-agentd --pool zroot --state-dir /var/db/keel --socket /var/run/keel-agentd.sock
```

Defaults match the conventions already established in the Milestone 4 spec
and tests (`zroot` pool, `/var/db/keel` state directory). Flags are parsed by
hand (three flags, each with a default — not enough surface to justify a
`clap` dependency). Startup sequence in `main.rs`:

1. Parse flags.
2. Construct `ProcessJailRuntime::new()`, `CliZfsManager`, `ProcessNetManager`
   (all already implemented in earlier milestones).
3. `Reconciler::new(jails, zfs, net, pool, state_dir)` — rebuilds desired
   state from the on-disk store, per the crash-only-safety design already in
   place.
4. Spawn the worker thread (owns the `Reconciler`, holds the `mpsc::Receiver`).
5. Spawn the timer thread (sends `Tick` every 5s).
6. Bind the `UnixListener` at the socket path; since `keel-agentd` runs as
   root, the socket is created `root`-owned. Set its permissions to `0600`
   explicitly after binding (the process umask alone isn't a reliable
   guarantee), matching the top-level spec's Unix socket trust model.
7. Accept loop: for each connection, spawn a thread running `http.rs`'s
   per-connection handler, which parses one request, sends the matching
   `Command`, waits for the `oneshot` reply, writes the HTTP response, and
   closes the connection (no keep-alive — one request per connection is
   sufficient for `keelctl`'s usage pattern and keeps the handler simple).

## Testing Strategy

- `http.rs` and `worker.rs` are generic over `J`/`Z`/`N`. Tests instantiate
  the worker with `FakeJailRuntime`/`FakeZfsManager`/`FakeNetManager`, bind a
  real `UnixListener` on a temp-directory socket path, and drive it with real
  HTTP requests — including full `keelctl`-binary-against-live-server round
  trips (spawning the built `keelctl` binary as a subprocess against the
  test server). This runs on macOS/CI with zero FreeBSD dependency, covering:
  routing for all three verbs, YAML (de)serialization, status/error code
  mapping (including the `InvalidSpec`-vs-`409` immutable-field-change split
  and `404` for unknown names), the `Tick`/`Apply`/`Delete` serialization
  through the worker channel, and the "apply reconciles immediately" behavior
  (a `GET` right after a `PUT` observes `running: true` without waiting for
  the timer).
- `main.rs`'s wiring to the real implementations, the real 5-second timer
  cadence under real jail/ZFS/net operations, and socket permissions are
  verified manually on the FreeBSD VM: build the binary, run it in the
  foreground, drive it with the real `keelctl` binary, confirm jails are
  actually created/running/deleted. No `rc.d` integration or automated
  smoke test yet — that's Milestone 6.

## Open Questions / Deferred Decisions

- Whether `keel-agentd` should log reconciliation failures from `Tick`
  handling anywhere (stderr, `tracing`, syslog) — no logging framework is in
  the dependency tree yet; deferred until a milestone that actually needs
  operator-visible logs (likely Milestone 6, alongside `rc.d`'s syslog
  integration mentioned in the top-level spec).
- Whether `keelctl get` should support a `--watch`-style follow mode —
  out of scope for v1; `kubectl`-style one-shot `get` is sufficient for now.
- Socket path/state-dir/pool as CLI flags is a stopgap; a proper config file
  is deferred (see top-level spec's existing open question on this).
