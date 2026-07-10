# Milestone 7: Node Registry (Sub-Project 2, First Milestone)

Status: Approved
Date: 2026-07-10

## Context

Sub-project 1 (Milestones 1-6) delivered a complete single-node jail
reconciliation daemon: `keel-agentd` runs as a proper FreeBSD service,
reconciling a desired-state spec against real jails/ZFS/VNET on one host,
driven locally through its Unix-socket HTTP API and `keelctl`.

The README's roadmap lists several undesigned future sub-projects,
treating "multi-node control plane (API server, cluster state store)" as
distinct from "scheduler (bin-packing jails across nodes)" and "cluster
networking (cross-node overlay, service discovery/load balancing)". This
milestone is the first slice of the control-plane sub-project, and
deliberately stops short of both scheduling and request-forwarding: it
answers only "which nodes exist, and are they alive," the foundation
everything else in that sub-project will need first.

Test environment: three real FreeBSD VMs are available for this
sub-project (`192.168.64.2`, `.4`, `.5`), consistent with every prior
milestone's discipline of verifying real behavior on real hardware rather
than assuming it. `.2` hosts both the new control plane and its own
`keel-agentd`; `.4` and `.5` run `keel-agentd` only, registering with `.2`
over the network.

## Goals (Milestone 7)

- A new crate, `keel-controlplane`, exposing an HTTP API over TCP (not a
  Unix socket, since nodes are on separate hosts) with three endpoints:
  register a node, heartbeat a node, list all known nodes with computed
  liveness.
- `keel-agentd` optionally registers itself with a control plane at
  startup and heartbeats on a fixed timer, entirely opt-in via two new
  CLI flags (`--node-id`, `--control-plane-addr`) plus a third
  (`--advertise-addr`) required alongside them.
- Self-healing membership: if the control plane restarts and forgets
  every node, each node's next heartbeat is rejected and it re-registers
  automatically, no manual intervention, no crash-safe persistence
  required on the control-plane side.
- End-to-end verification across the three real VMs: all three nodes
  showing as Alive, a killed node's agent flipping to Dead after the
  timeout without affecting the other two, and a control-plane restart
  repopulating to all-Alive within one heartbeat interval.

## Non-Goals (Milestone 7)

- Any spec-forwarding, placement, or scheduling logic. `keelctl` still
  talks directly to a single `keel-agentd`'s Unix socket, exactly as
  today; the control plane never sees a `JailSpec`.
- A connectable node address. `--advertise-addr` is an opaque string
  (see Open Questions) recorded for future milestones' use, not something
  this milestone dials into or validates. `keel-agentd`'s own API stays
  Unix-socket-only; it gains no new network listener.
- Crash-safe persistence of the registry (see Milestone 6's design spec
  for the precedent this follows: self-healing over durability, applied
  here to cluster membership instead of jail state). A control-plane
  restart is expected to briefly forget every node; this is the accepted
  behavior, not a bug to work around.
- Authentication/authorization between nodes and the control plane. Same
  trust model as the existing Unix-socket API's `0600` permission bit:
  same-network trust is assumed for this milestone; hardening this is
  explicitly deferred (see Open Questions).
- A `rc.d` script for `keel-controlplane`. It runs in the foreground on
  `.2` for this milestone's VM verification, the same way `keel-agentd`
  itself was first verified in Milestone 5 before Milestone 6 gave it a
  service wrapper.

## Architecture

### `keel-controlplane` crate

New workspace member, structured like `keel-agentd`:

- `src/registry.rs` — `Registry`, a plain `HashMap<String, NodeRecord>`
  (`NodeRecord { addr: String, last_heartbeat: Instant }`), with
  `register(id, addr, now)` (idempotent upsert — re-registering an
  existing id just refreshes `addr` and `last_heartbeat`),
  `heartbeat(id, now) -> Result<(), UnknownNode>`, and
  `list(now) -> Vec<NodeStatus>`. Status is computed in `list`, not
  stored: `Alive` if `now - last_heartbeat < DEAD_THRESHOLD` (15s, three
  missed 5s heartbeats), else `Dead`. No separate sweep thread.
- `src/worker.rs` — same ownership pattern as `keel-agentd`'s
  `worker.rs`: one thread owns the `Registry` exclusively; a `Command`
  enum (`Register`, `Heartbeat`, `List`, each carrying a reply channel)
  is the only way in, mirroring `keel-agentd`'s `Command::Apply/Get/
  Delete`.
- `src/http.rs` — the same hand-rolled `httparse`-over-a-blocking-
  listener server as `keel-agentd`'s `http.rs`, with the listener type
  swapped from `UnixListener`/`UnixStream` to `TcpListener`/`TcpStream`
  (nodes are on separate hosts, so a filesystem-scoped socket can't
  work here). Routes:
  - `POST /nodes/register`, YAML body `{id, addr}` → `Command::Register`,
    200 empty body.
  - `POST /nodes/:id/heartbeat` → `Command::Heartbeat`; 200 empty body,
    or 404 if the id is unknown (the signal a node uses to know it must
    re-register).
  - `GET /nodes` → `Command::List`, 200 with a YAML list of
    `NodeStatus { id, addr, status: Alive | Dead, last_seen_secs: u64 }`.
- `src/main.rs` — binary, one flag: `--addr` (default
  `0.0.0.0:7620`), binds the `TcpListener`, spawns the worker, calls
  `http::run`.

### `keel-agentd` changes

- Three new, entirely optional `Config` fields / CLI flags: `--node-id`,
  `--control-plane-addr`, `--advertise-addr`. None have defaults; if
  `--control-plane-addr` is absent, `keel-agentd` starts exactly as it
  does today (Milestones 1-6 behavior and the existing smoke test are
  unaffected). If it's present, `--node-id` and `--advertise-addr` become
  required (a startup-time `panic!` with a clear message if either is
  missing, matching `parse_args`'s existing style of failing fast on bad
  config rather than guessing).
- New module `src/registration.rs`, spawned from `main.rs` only when
  `--control-plane-addr` is set. A single background thread, structured
  as one unconditional loop:

  ```
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
                  registered = false; // re-register next iteration, whether
                                      // rejected (404, control plane forgot
                                      // us) or unreachable (connection error)
              }
          }
      }
      thread::sleep(HEARTBEAT_INTERVAL); // 5s, same constant used for both
  }
  ```

  `register_once`/`heartbeat_once` are plain hand-rolled HTTP calls over
  a `TcpStream`, the exact pattern `keelctl::send_request` already uses
  over a `UnixStream` (build the request line by hand, write it, read to
  EOF, parse the response with `httparse`) — no new HTTP client
  dependency. Treating *any* heartbeat failure (404 or I/O error) the
  same way (fall back to re-registering) keeps this loop's logic
  uniform; the worst case is one extra `register` call that the control
  plane's idempotent upsert handles for free.

### Deployment for this milestone

`.2` runs `keel-controlplane --addr 0.0.0.0:7620` and its own
`keel-agentd --node-id node-2 --advertise-addr 192.168.64.2
--control-plane-addr 192.168.64.2:7620` (plus its existing pool/state-
dir/socket flags). `.4` and `.5` run `keel-agentd` with the same three
new flags, `--node-id node-4`/`node-5`, `--advertise-addr` their own IP,
`--control-plane-addr 192.168.64.2:7620`.

## Error Handling

- The control plane never fails a request because of a *different*
  node's state — same "one bad actor doesn't block the others" principle
  `Reconciler::reconcile` already follows. `Register`/`Heartbeat`/`List`
  each only ever touch the one relevant entry (or none, for `List`).
- A node that can't reach the control plane at startup doesn't fail to
  start — `registration.rs`'s loop just keeps retrying every 5s
  indefinitely, matching `--control-plane-addr` being optional, additive
  config rather than a hard dependency.
- No backoff escalation on registration/heartbeat failures (unlike
  `BackoffState`'s jail-restart cooldown) — a fixed 5s retry is
  sufficient here since the failure modes (control plane briefly down or
  restarting) are expected to be short, and a control plane handling
  three nodes' worth of retries has no scaling concern this milestone
  needs to guard against.

## Testing Strategy

- `keel-controlplane`, unit tests (no FreeBSD needed, same as Milestones
  1-4's fakes-based tests): `Registry::register` (fresh and re-
  registration/idempotency), `heartbeat` on a known vs. unknown id,
  `list`'s Alive/Dead computation around the `DEAD_THRESHOLD` boundary.
  `http.rs` tests analogous to `keel-agentd`'s (real `TcpListener` bound
  to an ephemeral `127.0.0.1:0` port, subprocess-level requests).
- `keel-agentd`'s `registration.rs`: tested against a real, in-process
  `keel-controlplane` test server (the same `http::run` used in
  production, bound to an ephemeral port) — no FreeBSD or VM needed,
  since none of this touches jails/ZFS/VNET. Covers: successful register
  + heartbeat updates `last_heartbeat`; dropping that listener, starting
  a fresh one (simulating a control-plane restart), and confirming the
  background thread re-registers within one `HEARTBEAT_INTERVAL`.
- VM verification (the real proof, same discipline as every prior
  milestone) across all three VMs: `GET /nodes` on `.2` shows all three
  as `Alive` within one heartbeat interval of starting them; killing
  `keel-agentd` on `.4` shows it flip to `Dead` after 15s with `.2`/`.5`
  unaffected; restarting `keel-controlplane` on `.2` shows the registry
  repopulate to all-three-Alive within 5s without restarting any node.

## Open Questions / Deferred Decisions

- What `--advertise-addr` actually needs to be (a bare IP? a full
  `host:port` for a future TCP API?) is intentionally left underspecified
  here — this milestone never dials it. The future spec-forwarding
  milestone (option 2 considered and deferred during this milestone's
  design) will need to decide whether `keel-agentd` gains a second,
  network-reachable API listener alongside its existing Unix socket, and
  that decision should set `--advertise-addr`'s real contract, not this
  one.
- Authentication between nodes and the control plane is unaddressed
  (same-network trust assumed). Worth revisiting once the control plane
  does anything more consequential than track liveness.
- Whether `keel-controlplane` eventually needs its own `rc.d` script is
  deferred until it's doing enough to warrant running as a permanent
  service; this milestone verifies it running in the foreground only.
