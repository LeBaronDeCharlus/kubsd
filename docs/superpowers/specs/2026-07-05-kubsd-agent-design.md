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

  Trait surface (updated in Milestone 3 to add VNET support to `create`;
  originally shipped in Milestone 2 without it):

  ```rust
  pub trait JailRuntime {
      fn create(&self, name: &str, rootfs: &Path, vnet: bool) -> Result<(), JailError>;
      fn jail_exists(&self, name: &str) -> Result<bool, JailError>;
      fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError>;
      fn destroy(&self, name: &str) -> Result<(), JailError>;
      fn is_running(&self, name: &str) -> Result<bool, JailError>;
      fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError>;
      fn remove_resource_limits(&self, name: &str) -> Result<(), JailError>;
  }
  ```

  `jail_exists` (added in Milestone 4) checks only whether the jail itself
  exists (the `jls`-based half of what `is_running` already checks,
  without the process-liveness half) — needed because `is_running`
  collapses "jail doesn't exist" and "jail exists but its process exited"
  into the same `false`, and `kubsd-agentd`'s reconciler needs to tell
  those apart: the former needs full provisioning, the latter just needs
  `start_command` again.

  `kubsd-jail` takes a plain jail name string — it has no knowledge of
  kubsd's `kubsd-<name>` naming/ownership convention; applying that prefix
  is `kubsd-agentd`'s responsibility. `create`'s `vnet` parameter controls
  whether the jail gets `jail -c ... vnet persist` (its own network stack,
  required before `kubsd-net`'s `attach_jail` can move an interface into
  it — VNET can only be set at jail-creation time, not added
  retroactively) or plain `jail -c ... persist` (shares the host's network
  stack, e.g. for a jail with no `kubsd-net` wiring). `create` and
  `start_command` are deliberately separate calls rather than one
  `jail -c ... command=<command>`: `create` instantiates an empty,
  persistent jail from `rootfs` with no command running yet, so
  `kubsd-agentd` can configure networking (via `kubsd-net`) and apply
  `rctl` limits *before* the jailed process ever runs, matching the
  Reconciliation Loop's stated order below. `start_command` then launches
  the process inside the
  already-created jail (`jexec <name> <command>`, spawned without waiting
  on it) and is also the primitive `kubsd-agentd` re-invokes on every
  restart under `restartPolicy: Always`/`OnFailure` — no separate restart
  API is needed. Neither call may block its caller for the process's
  lifetime: shelling out to `jail(8)`/`jexec(8)` synchronously and waiting
  on it would stall the single-threaded reconciler for as long as the
  command keeps running (e.g. forever, for a healthy `Always`-policy jail).
  `is_running` cannot rely on a remembered child handle either, since a
  handle wouldn't survive a `kubsd-agentd` restart and would break
  crash-only-safety; it always resolves liveness from a live system query
  (e.g. `jls` for the jail's existence plus a process-table check for
  whether its command is still running), consistent with rebuilding
  observed state from disk plus live query on every startup (see Error
  Handling). `destroy` uses `jail -r` (also kills jailed processes).
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
  Exposes a `NetManager` trait, shelling out to `ifconfig(8)`/`jexec(8)`.
  Milestone 3 scope is bridge-only L2 connectivity between jails and the
  host — no NAT/outbound internet access, which would need host-level
  `pf` rules and IP-forwarding setup and is deliberately deferred as a
  separate, larger concern.

  Trait surface (Milestone 3):

  ```rust
  pub trait NetManager {
      fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError>;
      fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError>;
      fn detach_jail(&self, epair_base: &str) -> Result<(), NetError>;
  }
  ```

  `ensure_bridge_exists` is idempotent: creates the bridge if it doesn't
  already exist and brings it up. It never destroys a bridge, since other
  jails or host config may depend on it — there is no corresponding
  `destroy_bridge`. `attach_jail` is one coherent operation covering the
  whole wiring sequence (create the epair pair from `epair_base`, e.g.
  `"epair7"` → `epair7a`/`epair7b`; add the `a` side to the bridge; move
  the `b` side into the jail's VNET; configure `address` on it from inside
  the jail via `jexec`) exposed as a single call, matching the
  Reconciliation Loop's stated order (networking is configured as one
  step between jail creation and starting the command). Because
  `epair_base` is a stable, persisted name (see Naming and ownership
  below), `attach_jail` must tolerate the pair already existing from an
  interrupted prior attempt (a crash between epair creation and the jail
  actually starting), the same way `kubsd-zfs`'s `clone_from_base`
  tolerates a snapshot that already exists: treat "already exists" as a
  signal to proceed with the remaining steps, not as failure. `detach_jail`
  tears down the epair pair and is called before jail destroy, per the
  Reconciliation Loop's existing stated order; it must treat an
  already-absent epair pair (e.g. one FreeBSD already destroyed along with
  its owning jail, or a retry after a previously crashed detach) as
  success rather than an error, mirroring `kubsd-jail`'s
  `remove_resource_limits`. Like `kubsd-jail` and `kubsd-zfs`, this crate
  takes plain bridge/epair/jail names it's given — it has no knowledge of
  kubsd's `kubsd0` bridge-naming or per-jail epair-ordinal conventions;
  that's `kubsd-agentd`'s job.
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
  `<pool>/kubsd/<image>` — the spec's `image` field already includes its
  full relative path under `kubsd/` (e.g. `image: base/14.2-web` maps to
  `<pool>/kubsd/base/14.2-web`, not `<pool>/kubsd/base/base/14.2-web`).
  The jail's rootfs filesystem path passed to `kubsd-jail`'s `create` is
  simply `/<jail-dataset-path>` (leading slash), matching ZFS's default
  mountpoint inheritance — confirmed directly against the real VM in
  Milestone 2 (`zroot/kubsd/base/test` mounts at
  `/zroot/kubsd/base/test`).

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
     image), create the jail (`kubsd-jail`'s `create`), configure
     networking (epair/bridge/VNET), apply `rctl` limits, then start the
     command (`kubsd-jail`'s `start_command`).
   - Existing but not desired → remove network interfaces (`kubsd-net`'s
     `detach_jail`), destroy the jail (`kubsd-jail`'s `destroy`, which
     itself does `jail -r` — this already terminates jailed processes), and
     destroy the ZFS clone, then remove `rctl` rules. **Milestone 4
     simplification:** a graceful SIGTERM-then-grace-period-then-SIGKILL
     shutdown (as originally described here) is deferred — `destroy`'s
     existing `jail -r` behavior is used as-is for v1. Adding a gentler
     `stop_gracefully` primitive to `kubsd-jail` is a future enhancement,
     not required for this milestone.
   - Existing and desired but the process has exited → apply
     `restartPolicy` by calling `start_command` again (see backoff below).
     **Milestone 4 simplification:** `OnFailure` and `Always` behave
     identically (both restart on any exit) — `kubsd-jail` has no way to
     distinguish a clean exit from a crash yet (no exit-code tracking).
   - Existing and matches desired → no-op.
3. Milestone 4 implements this loop as a synchronous `reconcile(now)` call
   with no timer or event loop of its own yet — a future milestone (the
   HTTP API + daemon binary) drives it on a timer and reacts to
   `SIGCHLD`-style notification for promptness; until then, restart
   promptness is whatever cadence the caller invokes `reconcile` at.
4. Restarts under `Always`/`OnFailure` use capped exponential backoff per
   jail (e.g. 1s, 2s, 4s, ... up to a 5-minute cap), reset once a jail has
   stayed up longer than a threshold (e.g. 60s). This bounds CPU waste and
   log spam from a persistently-crashing command, mirroring Kubernetes'
   `CrashLoopBackOff`. The same per-jail backoff state gates *any* failing
   action for that jail, not just command restarts — a jail stuck failing
   to provision (e.g. missing base image) is retried on the same
   escalating cadence as a crash-looping command, matching "retried with
   backoff like any other failure" below.
5. The reconciler is single-threaded. **Milestone 4 simplification:** the
   inbound work queue described here (API-triggered apply/delete and the
   periodic timer tick enqueuing work items to serialize against each
   other) is deferred until a future milestone actually has two
   concurrent trigger sources (an HTTP server and a timer thread) that
   could race — this milestone's `apply`/`delete`/`reconcile` are plain
   synchronous methods with no concurrency to serialize yet. See
   `kubsd-agentd` Implementation below for the concrete API shape.

## kubsd-agentd Implementation (Milestone 4)

`kubsd-agentd` is a **library crate only** for this milestone — no
`main.rs`/binary target yet. That arrives in a later milestone alongside
the HTTP API and daemon supervision. This milestone builds a generic
`Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager>` that composes
`kubsd-spec`, `kubsd-jail`, `kubsd-zfs`, and `kubsd-net`, so tests
instantiate it against the fakes (`FakeJailRuntime`, `FakeZfsManager`,
`FakeNetManager`) with zero FreeBSD dependency, and a later milestone
instantiates it against the real implementations.

**Files:** `kubsd-agentd/src/record.rs` (`JailRecord`, naming/path
derivation), `kubsd-agentd/src/store.rs` (persistence),
`kubsd-agentd/src/backoff.rs` (`BackoffState`),
`kubsd-agentd/src/reconciler.rs` (the `Reconciler` itself).

**Data model:**

```rust
pub struct JailRecord {
    pub spec: JailSpec,       // from kubsd-spec
    pub epair_ordinal: u32,   // assigned once at first apply, stable across restarts
}
```

Naming/path derivation (pure functions):
- Jail name: `kubsd-<spec.metadata.name>`
- Base dataset: `<pool>/kubsd/<spec.spec.image>`
- Jail's own dataset: `<pool>/kubsd/jails/<spec.metadata.name>`
- Jail rootfs path (passed to `kubsd-jail`'s `create`): `/<jail-dataset-path>`
- epair base name: `epair<epair_ordinal>`

**State store:** one YAML file per jail at `<state_dir>/<spec-name>.yaml`
(a serialized `JailRecord`), written via write-to-temp-then-rename. On
`Reconciler::new`, every file in `state_dir` is loaded into memory as the
starting desired state; the next `epair_ordinal` to assign is `1 +
max(existing ordinals)` — no separate counter file needed, since the
ordinal is always recoverable by scanning already-persisted records.

**API surface:**

```rust
impl<J: JailRuntime, Z: ZfsManager, N: NetManager> Reconciler<J, Z, N> {
    pub fn new(jails: J, zfs: Z, net: N, pool: String, state_dir: PathBuf) -> Result<Self, ReconcileError>;
    pub fn apply(&mut self, spec: JailSpec) -> Result<(), ReconcileError>;
    pub fn delete(&mut self, name: &str) -> Result<(), ReconcileError>;
    pub fn reconcile(&mut self, now: Instant) -> Result<(), ReconcileError>;
}
```

- `apply`: validates the spec (`kubsd-spec`'s existing checks, plus
  `validate_transition` against the existing record if the name is
  already known), assigns an `epair_ordinal` if new, persists the
  `JailRecord`, updates in-memory desired state. Does not touch the
  jail/network/filesystem itself — `reconcile`'s job on its next call.
- `delete`: synchronously tears down (network detach, jail destroy,
  dataset destroy — in that order, matching the Reconciliation Loop's
  stated order above) then removes the record from disk and memory.
- `reconcile(now)`: one synchronous pass over every desired jail,
  implementing the Reconciliation Loop's per-jail diff logic above
  (provision if missing, restart if crashed and backoff allows it,
  idempotently re-apply resource limits if running, no-op otherwise).
  `now` is passed in by the caller (rather than read internally via
  `Instant::now()`) so tests can simulate time passing without real
  sleeps. **Resolves a previously-open question:** the "exists, not
  running" branch always reapplies `ensure_bridge_exists` → `attach_jail`
  → `set_resource_limits` (all already idempotent) *before* calling
  `start_command` — not just in the "doesn't exist yet" branch. This
  covers the daemon-crashed-mid-provisioning case (jail created but
  networking/`rctl` never got configured before the daemon itself died)
  without needing any new persisted state: a restart naturally re-runs the
  idempotent configuration steps rather than trusting that "jail exists"
  implies "jail is fully configured."

**Backoff** (`kubsd-agentd/src/backoff.rs`):

```rust
struct BackoffState {
    current_delay: Duration,        // starts at 1s, doubles up to a 5-minute cap
    next_retry_at: Option<Instant>, // None = no cooldown, act immediately
    last_started_at: Option<Instant>,
}
```

Tracked per jail name, covering any failing action for that jail (not
just command restarts — see Reconciliation Loop item 4 above). Before
acting: if `next_retry_at` is `Some` and hasn't passed, skip this jail
this pass. After acting: if the jail had been running ≥60s before this
attempt, reset `current_delay` to 1s first (a jail that ran fine for a
while before failing once shouldn't inherit an escalated backoff from an
unrelated earlier crash-loop); set `next_retry_at = now + current_delay`;
double `current_delay` (capped at 5 minutes) for next time.

**Partial-failure rollback:** if provisioning a new jail fails partway
(e.g. dataset clone succeeds but jail creation fails), `reconcile` unwinds
whatever succeeded so far, in reverse order, before recording the
failure against that jail's backoff state — so a failed creation never
leaks a ZFS clone or a half-attached network interface. This is safe
because `detach_jail`/`destroy`/`destroy_dataset` all already tolerate
"already gone" as success (built into `kubsd-jail`/`kubsd-zfs`/`kubsd-net`
since Milestones 2-3), so rollback can call them unconditionally even for
steps that never fully completed.

## Error Handling

- All reconciliation actions are logged via structured logging (`tracing`).
  Failures do not crash the daemon; the reconciler retries with backoff on
  the next loop iteration.
- Partial failure during jail creation (e.g. ZFS clone succeeds but
  `kubsd-jail`'s `create` fails) triggers automatic cleanup of the
  partially-created resources, so failed creations don't leak ZFS clones or
  network interfaces.
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
  no real FreeBSD system required. Key scenarios: naming/path derivation
  (pure functions), state store round-trip through disk, full provisioning
  happy path (all four fakes driven in the correct order), restart-on-crash
  via `FakeJailRuntime::mark_exited` with injected `Instant`s to exercise
  backoff timing without real sleeps, partial-failure rollback (a fake
  configured to fail at a specific step), `restartPolicy: Never` leaving a
  crashed jail alone, and immutable-field rejection on re-`apply`.
- `kubsd-jail`, `kubsd-zfs`, `kubsd-net`: integration tests that exercise
  real syscalls/CLI tools; these only run on a FreeBSD host. In practice
  (from Milestone 2 onward): the repo is `git clone`d once on the FreeBSD
  VM (`git@github.com:LeBaronDeCharlus/kubsd.git`), then `git pull` +
  `cargo test` there before each round of integration testing (direct SSH
  from the coordinating assistant to the VM became available partway
  through Milestone 2; relayed-through-the-human-operator is a fallback if
  that access is ever unavailable again). A `zroot/kubsd/base/test`
  dataset (a minimal FreeBSD userland) and a `zroot/kubsd/jails` parent
  dataset must exist on the VM for `kubsd-zfs`'s clone tests — a one-time
  VM setup step, same category as Milestone 1's environment prep.
  `kubsd-net`'s integration tests additionally need a real jail to attach
  to (created via `kubsd-jail`) and must explicitly tear down any
  leftover bridge/epair interfaces from a failed prior run, since
  `ensure_bridge_exists` never removes bridges.
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

- Whether `kubsd-spec` should validate the `apiVersion`/`kind` discriminant
  fields (e.g. reject a document with `kind: Pod` or an unrecognized
  `apiVersion`) — currently unchecked; flagged as a non-blocking gap in the
  Milestone 1 final review and still open.
- Exact on-disk state store format (flat YAML files vs. embedded DB like
  `sled`) — flat files chosen for v1 simplicity; revisit if performance or
  concurrency needs arise.
- Whether `kubsd-jail` moves from shelling out to `jail(8)`/`rctl(8)` to
  raw `jail_set(2)`/`rctl` syscalls, and whether `kubsd-zfs` moves from
  `zfs(8)` to `libzfs_core` FFI — both deferred indefinitely; shelling out
  is the settled Milestone 2 decision, hidden behind each crate's trait
  either way, so a future switch is an internal change invisible to
  callers.
- ~~Whether the Reconciliation Loop's "existing and desired but the
  process has exited" branch needs to re-verify (and reapply if missing)
  networking and `rctl` limits before calling `start_command` again~~ —
  **Resolved in Milestone 4:** yes, it always reapplies both (idempotently)
  before restarting the command. See `kubsd-agentd` Implementation above.
- The single-threaded work queue serializing API-triggered and timer-
  triggered reconciliation (Reconciliation Loop item 5) — deferred until a
  later milestone actually introduces two concurrent trigger sources (an
  HTTP server and a timer thread); Milestone 4's `Reconciler` is plain
  synchronous methods with no concurrency to serialize yet.
- Graceful jailed-process shutdown (`SIGTERM`, wait up to a grace period,
  then `SIGKILL`) for the "existing but not desired" delete path —
  deferred; `kubsd-jail`'s existing `destroy` (`jail -r`) is used as-is for
  v1. Would need a new `kubsd-jail` primitive (e.g. `stop_gracefully`) if
  ever implemented.
- Distinguishing `restartPolicy: OnFailure` from `Always` — currently
  identical behavior (both restart on any exit), since `kubsd-jail` has no
  way to retrieve a jailed command's exit code. Would need
  `ProcessJailRuntime`'s child-tracking restructured from an unordered
  `Vec<Child>` to name-keyed, capturing exit status on reap.
