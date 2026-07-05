# kubsd-agent: Single-Node FreeBSD Jail Reconciliation Daemon

Status: Draft, pending user review
Date: 2026-07-05

## Context

kubsd is a long-term effort to build a Kubernetes-style orchestration platform
for FreeBSD, filling a gap in the FreeBSD ecosystem (existing tools like
bastille/ezjail/cbsd manage jails but don't provide declarative,
reconciliation-based orchestration comparable to Kubernetes).

The full platform (multi-node API server, scheduler, cluster networking,
storage orchestration, service discovery) is too large to design or build in
one pass. This spec covers only the first sub-project: **kubsd-agent**, the
single-node daemon that reconciles declarative jail specs against actual
system state. This is the FreeBSD analog of a kubelet, and it must be useful
and testable entirely on its own, on one machine, before any cluster-level
concerns are introduced.

## Goals (v1)

- Accept a declarative YAML spec describing a jail (image, command, network,
  resource limits, restart policy).
- Continuously reconcile actual FreeBSD jail state to match the desired
  state: create, start, stop, and destroy jails as specs are applied or
  removed.
- Enforce resource limits (CPU, memory) via `rctl(8)`.
- Enforce a restart policy (`Always` / `OnFailure` / `Never`) when a jailed
  process exits.
- Provision each jail's root filesystem as a ZFS clone of a base image
  dataset.
- Set up per-jail networking via VNET + `epair(4)` + `bridge(4)`.
- Expose a local HTTP API (over a Unix socket) and a companion CLI
  (`kubsdctl`) to apply/get/delete specs.
- Survive daemon restarts without losing or duplicating jails (state is
  reconstructed from disk + live system query, not held only in memory).

## Non-Goals (v1)

- Multi-node clustering, scheduling, or any cross-node networking.
- Health-check probes (liveness/readiness) beyond restart-on-exit.
- Log aggregation/shipping.
- bhyve VM support.
- Any BSD other than FreeBSD.
- A UI beyond the CLI.

## Architecture

Rust workspace with the following crates:

- **`kubsd-spec`** — the jail spec schema (YAML), (de)serialization, and
  validation. Pure Rust; compiles and tests on any OS.
- **`kubsd-jail`** — wraps jail lifecycle and resource-limit management
  behind a `JailRuntime` trait so callers can be tested against a fake
  implementation. For v1 it shells out to `jail(8)`/`jls(8)`/`rctl(8)`
  rather than binding `jail_set(2)`/`jail_get(2)`/`rctl` syscalls directly
  — the syscalls involve a fiddly `jailparam` array ABI, and shelling out
  first (same escape hatch as `kubsd-zfs`) gets a working v1 sooner with
  lower risk. Moving to raw syscalls later is an internal change behind
  the trait, invisible to callers.
- **`kubsd-zfs`** — wraps ZFS dataset/snapshot/clone operations used to
  provision a jail's root filesystem from a base image dataset. Exposes a
  `ZfsManager` trait for the same reason.
- **`kubsd-net`** — sets up `epair(4)` + `bridge(4)` + VNET wiring per jail.
  Exposes a `NetManager` trait.
- **`kubsd-agentd`** — the daemon binary. Hosts the local HTTP API and owns
  the reconciliation loop; composes the traits above.
- **`kubsdctl`** — CLI binary that talks to `kubsd-agentd`'s API to
  `apply`/`get`/`delete` jail specs (mirrors `kubectl` ergonomics).

The trait boundary around FreeBSD-only syscall code is the key design
decision: it lets the reconciliation logic in `kubsd-agentd` be fully unit
tested on macOS/Linux with fake `JailRuntime`/`ZfsManager`/`NetManager`
implementations, while the real implementations are only compiled and
exercised on a FreeBSD host.

## Data Model — Jail Spec

```yaml
apiVersion: kubsd/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web       # ZFS base dataset to clone from
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: kubsd0
    address: 10.0.0.5/24     # static IP assigned to the jail's epair peer
  resources:
    cpu: "2"                 # number of cores; translated to rctl's
                              # pcpu percentage as cores * 100 (so "2" -> 200)
    memory: "512M"           # rctl vmemoryuse limit
  restartPolicy: Always      # Always | OnFailure | Never
```

`kubsd-spec` owns parsing and validation (well-formed resource strings,
name uniqueness, required fields, valid CIDR for `network.address`).

IP addresses are statically assigned in the spec for v1 (no DHCP, no
agent-owned IPAM pool) — simplest option for a single-node agent, at the
cost of the user picking non-colliding addresses themselves.

**Naming and ownership.** Every resource this agent creates is tagged so
reconciliation can tell "mine" from "foreign" after a restart, and so
resources for different jails never collide:

- The FreeBSD jail itself is named `kubsd-<jail-name>` (the actual
  `jail(8)` name, distinct from arbitrary jails a human or other tool may
  have created on the same host). On startup, reconciliation only
  considers jails whose name has the `kubsd-` prefix as agent-managed;
  everything else is left untouched regardless of jid or IP overlap.
- Each jail's `epair(4)` pair is named deterministically from a persisted
  per-jail ordinal (assigned on first creation and stored alongside the
  spec), e.g. `epair7a`/`epair7b`, so names stay stable across daemon
  restarts and never collide between jails.
- Each jail's ZFS clone lives at a fixed path derived from its name:
  `<pool>/kubsd/jails/<jail-name>`. Base images live at
  `<pool>/kubsd/base/<image-name>`.

State is persisted under `/var/db/kubsd/`: last-applied specs (plus the
assigned epair ordinal) as YAML files keyed by jail name, written via
write-to-temp-then-rename so a crash mid-write can never leave a corrupt
file that breaks state reconstruction on the next startup. No external
database dependency for v1 — the file store plus a live query of actual
jail state on startup is sufficient to rebuild in-memory state after a
daemon restart.

## Reconciliation Loop

Level-triggered, similar to a Kubernetes controller:

1. Maintain **desired state** (from applied specs, persisted to disk) and
   **observed state** (queried live via `kubsd-jail`/`rctl` on the running
   system).
2. On a timer (e.g. every 5s) and immediately after any API-triggered
   apply/delete, diff desired vs. observed per named jail:
   - Desired but not existing → provision rootfs (ZFS clone from base
     image), create the jail (`jail_set`), configure networking
     (epair/bridge/VNET), apply `rctl` limits, start the command.
   - Existing but not desired → stop the jailed process, remove network
     interfaces, destroy the jail, destroy the ZFS clone.
   - Existing and desired but the process has exited → apply
     `restartPolicy`.
   - Existing and matches desired → no-op.
3. Additionally react to `SIGCHLD`-style notification when a jailed
   process's init exits, so restart policy is applied promptly rather than
   waiting for the next timer tick.
4. Restarts under `Always`/`OnFailure` use capped exponential backoff per
   jail (e.g. 1s, 2s, 4s, ... up to a 5-minute cap), reset once a jail has
   stayed up longer than a threshold (e.g. 60s). This bounds CPU waste and
   log spam from a persistently-crashing command, mirroring Kubernetes'
   `CrashLoopBackOff`.
5. The reconciler is single-threaded with an inbound work queue: API-
   triggered apply/delete requests and the periodic timer tick both enqueue
   work items rather than acting directly, so two triggers for the same
   jail name can never race each other.

## Error Handling

- All reconciliation actions are logged via structured logging (`tracing`).
  Failures do not crash the daemon; the reconciler retries with backoff on
  the next loop iteration.
- Partial failure during jail creation (e.g. ZFS clone succeeds but
  `jail_set` fails) triggers automatic cleanup of the partially-created
  resources, so failed creations don't leak ZFS clones or network
  interfaces.
- The daemon is crash-only safe: on startup it always rebuilds desired
  state from the on-disk spec store and observed state from a live system
  query, rather than relying on any in-memory state surviving a restart.
- **Jails outlive the daemon.** Jails are not children of `kubsd-agentd`
  and are not stopped when it exits; a crash or upgrade of the daemon must
  not affect running jails. This is what makes the ownership tagging
  above (the `kubsd-` name prefix) necessary: on restart the daemon must
  distinguish jails it manages from any pre-existing/foreign jail on the
  host, and never touch the latter.
- **Unix socket trust model (v1):** the API socket is `root:wheel 0600`.
  Any local process able to reach it is trusted — there is no separate
  authn/authz layer in v1. This is a deliberate scope decision, not an
  oversight.
- **Daemon supervision:** `kubsd-agentd` runs under an `rc.d` script with
  restart-on-crash (`keep_alive`), and logs to syslog. This is what makes
  the "survives daemon restarts" goal demonstrable end-to-end, not just a
  property of the reconciliation logic in isolation.

## Testing Strategy

- `kubsd-spec`: plain unit tests, run in CI on any OS.
- `kubsd-agentd` reconciliation logic: unit tested against fake
  `JailRuntime`/`ZfsManager`/`NetManager` implementations — runs on any OS,
  no real FreeBSD system required.
- `kubsd-jail`, `kubsd-zfs`, `kubsd-net`: integration tests that exercise
  real syscalls/CLI tools; these only run on a FreeBSD host.
- End-to-end tests (apply a spec via `kubsdctl`, assert the jail is actually
  running with correct resource limits and network config) also require a
  real FreeBSD host.

## Prerequisite: FreeBSD Dev Environment

No FreeBSD environment is currently available. Before real (non-mocked)
implementation and testing can happen, a FreeBSD VM must be provisioned
(e.g. via UTM locally, or a cloud FreeBSD instance) with SSH access, a Rust
toolchain, a ZFS root, and jails enabled. This is a prerequisite task in the
implementation plan, not a runtime component of the agent itself.

## Open Questions / Deferred Decisions

- Whether `kubsd-zfs` uses `libzfs_core` FFI bindings or shells out to
  `zfs(8)` — deferred to implementation time, hidden behind the
  `ZfsManager` trait either way.
- Exact on-disk state store format (flat YAML files vs. embedded DB like
  `sled`) — flat files chosen for v1 simplicity; revisit if performance or
  concurrency needs arise.
