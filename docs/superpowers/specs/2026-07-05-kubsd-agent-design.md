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
- **`kubsd-jail`** — FFI wrapper around `jail_set(2)`, `jail_get(2)`,
  `jail_remove(2)`, and the `rctl` syscalls. FreeBSD-only. Exposes a
  `JailRuntime` trait so callers can be tested against a fake implementation.
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
  resources:
    cpu: "2"                 # rctl pcpu limit
    memory: "512M"           # rctl vmemoryuse limit
  restartPolicy: Always      # Always | OnFailure | Never
```

`kubsd-spec` owns parsing and validation (well-formed resource strings,
name uniqueness, required fields).

State is persisted under `/var/db/kubsd/`: last-applied specs as YAML files
keyed by jail name. No external database dependency for v1 — the file store
plus a live query of actual jail state on startup is sufficient to rebuild
in-memory state after a daemon restart.

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
