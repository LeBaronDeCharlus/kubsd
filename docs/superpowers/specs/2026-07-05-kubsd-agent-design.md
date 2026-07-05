# kubsd-agent: Single-Node FreeBSD Jail Reconciliation Daemon

Status: Approved
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

  Trait surface (Milestone 2 — no VNET/networking yet; see `kubsd-net`
  below for that):

  ```rust
  pub trait JailRuntime {
      fn create(&self, name: &str, rootfs: &Path, command: &[String]) -> Result<(), JailError>;
      fn destroy(&self, name: &str) -> Result<(), JailError>;
      fn is_running(&self, name: &str) -> Result<bool, JailError>;
      fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError>;
      fn remove_resource_limits(&self, name: &str) -> Result<(), JailError>;
  }
  ```

  `kubsd-jail` takes a plain jail name string — it has no knowledge of
  kubsd's `kubsd-<name>` naming/ownership convention; applying that prefix
  is `kubsd-agentd`'s responsibility. `create` uses `jail -c ... persist
  command=<command>`; `persist` keeps the jail alive independent of the
  command's exit, since re-running a failed command per `restartPolicy` is
  `kubsd-agentd`'s reconciliation-loop job (a later milestone), not this
  crate's. `destroy` uses `jail -r` (also kills jailed processes).
  `set_resource_limits`/`remove_resource_limits` wrap `rctl -a
  jail:<name>:pcpu:deny=<percent>` / `...vmemoryuse:deny=<bytes>` and
  `rctl -r jail:<name>`, matching the explicit-removal rule under Error
  Handling below.

- **`kubsd-zfs`** — wraps ZFS dataset/snapshot/clone operations used to
  provision a jail's root filesystem from a base image dataset. Exposes a
  `ZfsManager` trait for the same reason, shelling out to `zfs(8)`.

  Trait surface (Milestone 2):

  ```rust
  pub trait ZfsManager {
      fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError>;
      fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError>;
      fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError>;
  }
  ```

  Like `kubsd-jail`, this crate takes full dataset path strings and has no
  knowledge of kubsd's path convention (`<pool>/kubsd/base/<image>`,
  `<pool>/kubsd/jails/<name>`) — that's `kubsd-agentd`'s job. `clone_from_base`
  can't clone a live dataset directly, so it ensures a canonical snapshot
  `<base_dataset>@kubsd` exists (creating it on demand if missing — the
  operator only needs to prepare the base dataset itself, not a snapshot),
  then clones from that snapshot. `destroy_dataset` only ever removes the
  target clone, never the base dataset or its snapshot.
- **`kubsd-net`** — sets up `epair(4)` + `bridge(4)` + VNET wiring per jail.
  Exposes a `NetManager` trait. It creates the jail's configured bridge
  (e.g. `kubsd0`) on first use if it doesn't already exist, so bridge setup
  isn't a separate manual prerequisite; it never destroys a bridge, since
  other jails or host config may depend on it.
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

`kubsd-spec` owns parsing and validation of a single spec document
(well-formed resource strings, name format, required fields, valid CIDR
for `network.address`). Cross-jail name uniqueness cannot be checked by a
single-document parser — it's `kubsd-agentd`'s state-store concern at
apply time, not `kubsd-spec`'s.

IP addresses are statically assigned in the spec for v1 (no DHCP, no
agent-owned IPAM pool) — simplest option for a single-node agent, at the
cost of the user picking non-colliding addresses themselves.

**Base images.** Populating `<pool>/kubsd/base/<image-name>` (e.g. via
`bsdinstall` into a dataset, or `zfs recv` of a prepared image) is out of
scope for kubsd-agent v1 — base datasets are prepared out-of-band by the
operator. Before cloning, the reconciler checks that the base dataset
exists and fails that jail's reconciliation with a clear "base image not
found" error (retried with backoff like any other failure) rather than
attempting a clone that ZFS would reject anyway.

**Mutating an applied spec.** Re-`apply`ing a spec for a jail that already
exists is only supported for fields that don't require rebuilding the
jail's identity: `resources` and `restartPolicy` are reconciled in place
(new `rctl` limits take effect on the next reconciliation pass; a
`restartPolicy` change affects only future exits). `image` and
`network.address` are immutable after first creation — a spec that changes
either is rejected by `kubsd-spec` validation with an error telling the
user to `delete` and re-`apply` instead. This avoids having to define
in-place rootfs-swap or re-addressing semantics for v1.

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
   - Existing but not desired → stop the jailed process (`SIGTERM`, wait up
     to a per-jail grace period, default 10s, then `SIGKILL` and force
     `jail -R` removal if it hasn't exited), remove network interfaces and
     `rctl` rules, destroy the jail, destroy the ZFS clone.
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
- `rctl` rules are removed explicitly on jail destroy, not left to expire
  on their own — FreeBSD recycles `jid`s, so a stale `jail:<name>` rule
  left behind could otherwise misapply its limits to an unrelated jail
  that later reuses the same jid.
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
  real syscalls/CLI tools; these only run on a FreeBSD host. In practice
  (from Milestone 2 onward): the repo is `git clone`d once on the FreeBSD
  VM (`git@github.com:LeBaronDeCharlus/kubsd.git`), then `git pull` +
  `cargo test` there before each round of integration testing, relayed
  through the human operator's terminal since the coordinating assistant's
  shell cannot reach the VM directly. A `zroot/kubsd/base/test` dataset
  (a minimal FreeBSD userland) must exist on the VM for `kubsd-zfs`'s
  clone tests to clone from — a one-time VM setup step, same category as
  Milestone 1's environment prep.
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

- Exact on-disk state store format (flat YAML files vs. embedded DB like
  `sled`) — flat files chosen for v1 simplicity; revisit if performance or
  concurrency needs arise.
- Whether `kubsd-jail` moves from shelling out to `jail(8)`/`rctl(8)` to
  raw `jail_set(2)`/`rctl` syscalls, and whether `kubsd-zfs` moves from
  `zfs(8)` to `libzfs_core` FFI — both deferred indefinitely; shelling out
  is the settled Milestone 2 decision, hidden behind each crate's trait
  either way, so a future switch is an internal change invisible to
  callers.
