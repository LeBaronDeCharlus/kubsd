# Milestone 20: Single-Node bhyve VM Lifecycle (Sub-Project 8, First Milestone)

Status: Approved
Date: 2026-07-22

## Context

Every milestone through 19 orchestrates exactly one workload primitive: a
FreeBSD jail. The README's roadmap lists "bhyve VM workloads alongside
jails" as the only not-yet-designed future sub-project. This milestone is
the first step of that work, and it is scoped deliberately narrowly,
mirroring how Milestone 2 introduced `keel-jail` and `keel-zfs` two
milestones before `keel-net` added networking and four milestones before
`keel-agentd`'s reconciler or any HTTP/CLI surface existed. Nothing here
touches `keel-spec`, `keel-agentd`, `keel-controlplane`, or `keelctl` — this
milestone proves the bhyve primitives work in isolation, the same way
`keel-jail` proved `jail(8)`/`rctl(8)` in isolation before anything else in
the project depended on it.

Concretely: given a pre-built base VM disk image (a zvol with a guest OS
already installed on it, prepared out of band, exactly the way jail base
images are prepared out of band today), this milestone can clone that disk,
boot a bhyve VM from the clone, check whether it's running, and tear it back
down.

## Goals

- A new crate, `keel-vm`, structured identically to `keel-jail`: a
  `VmRuntime` trait, a `FakeVmRuntime` for fast cross-platform tests, and a
  `BhyveVmRuntime` that shells out to `bhyve(8)` and `bhyvectl(8)` on
  FreeBSD.
- `VmRuntime` supports exactly three operations: `create` (clone's disk path
  in, VM booting via UEFI bootrom), `is_running`, and `destroy`.
- Disk provisioning reuses `keel-zfs::ZfsManager` as-is: `clone_from_base`
  clones a VM's disk zvol from a base zvol's `@keel` snapshot with no new
  trait methods, since a zvol is a dataset as far as `zfs(8)` and the
  existing trait are concerned.
- A `freebsd_vm_lifecycle.rs` integration test, run on the real FreeBSD VM,
  mirroring `keel-jail/tests/freebsd_lifecycle.rs`: create a VM from a
  cloned disk, confirm it's running, destroy it, confirm it's gone.

## Non-Goals

- **Networking.** No tap/bridge wiring, no `NetManager` integration, no
  network config on the VM at all. A future milestone gives VMs network
  access the way Milestone 3 gave jails VNET, two milestones after their
  lifecycle primitives existed.
- **`keel-spec` integration.** No `kind: Vm`, no YAML schema, no validation.
  This milestone's tests construct disk paths and resource values directly
  in Rust, the same way `keel-jail`'s own Milestone 2 tests never touched
  `keel-spec::JailSpec`.
- **`keel-agentd`/reconciler/HTTP/CLI integration.** No desired-vs-observed
  reconciliation, no crash-loop backoff, no HTTP routes, no `keelctl` verbs.
- **Building or importing base VM images.** A base zvol with a guest OS
  already installed is assumed to exist, prepared manually, exactly like
  today's jail base images.
- **Runtime resource resize.** bhyve's `-c`/`-m` flags are fixed at launch;
  changing a running VM's CPU/memory allocation requires destroy-and-recreate,
  which is out of scope until a later milestone needs it.
- **Multi-disk VMs, VM snapshots beyond the one base-image clone, live
  migration, or interactive console access.** All deferred; none are needed
  to prove the lifecycle primitives.

## Architecture

### `keel-vm`: a new crate alongside `keel-jail`

```rust
pub trait VmRuntime {
    /// Spawns bhyve as a background process: `-l bootrom,<UEFI firmware
    /// path>` to boot the guest OS already installed on `disk` (no
    /// host-side loader step, unlike bhyveload — works for any guest OS,
    /// not just FreeBSD), `-c cpus` virtual CPUs, `-m memory_bytes` of RAM,
    /// and `disk` attached as a virtio-blk device. Returns once the
    /// process has been spawned; does not wait for the guest OS to finish
    /// booting, matching `JailRuntime::start_command`'s same
    /// launch-is-non-blocking contract.
    fn create(&self, name: &str, disk: &Path, cpus: u32, memory_bytes: u64) -> Result<(), VmError>;

    /// True once `/dev/vmm/<name>` exists — the device bhyve creates while
    /// the guest is actively running, and releases (via `destroy`) once
    /// torn down. False both before creation and after a clean or forced
    /// shutdown.
    fn is_running(&self, name: &str) -> Result<bool, VmError>;

    /// Stops the bhyve process if it's still alive, then runs `bhyvectl
    /// --vm=<name> --destroy` to release the vmm device — required even
    /// when the guest already shut itself down and the bhyve process
    /// already exited on its own, since bhyve does not release
    /// `/dev/vmm/<name>` automatically on process exit. Safe to call on a
    /// VM that was never created (mirrors `JailRuntime::destroy`'s
    /// idempotent-on-absence contract).
    fn destroy(&self, name: &str) -> Result<(), VmError>;
}
```

Unlike `JailRuntime`, there is no separate `create`-then-`start_command`
split: bhyve has no persist-without-running state, so spawning the process
*is* starting the guest. There is also no `set_resource_limits`/
`remove_resource_limits` pair — bhyve takes CPU/memory as launch flags, not
as a post-creation `rctl(8)` rule applied to an already-running instance,
so `create` takes `cpus`/`memory_bytes` directly and adjusting them means
destroying and recreating the VM.

`FakeVmRuntime` tracks created/destroyed VM names in memory, the same
pattern `FakeJailRuntime` already uses, so callers can unit-test against it
on any OS.

`BhyveVmRuntime` shells `bhyve`/`bhyvectl` via `std::process::Command`,
matching every other real implementation in this project (`ProcessJailRuntime`,
`CliZfsManager`, `CliMountManager`): no direct syscalls, no FFI, no library
beyond the standard process-spawning API.

### `keel-zfs`: no new trait methods

A zvol is still a dataset as far as `zfs(8)` — and this project's existing
`ZfsManager` trait — is concerned. `dataset_exists`, `clone_from_base`, and
`destroy_dataset` all work unchanged against a base zvol's `@keel` snapshot:
`clone_from_base(base_zvol, target_zvol)` snapshots the base (if not already
snapshotted) and clones it into `target_zvol`, identical to how a jail's
rootfs dataset is cloned today. `keel-vm` is responsible for knowing the
resulting clone's block device path convention
(`/dev/zvol/<pool>/<dataset>`) to pass to `bhyve -s`; this is a plain string
format, not a new ZFS operation, so it lives in `keel-vm` rather than
`keel-zfs`.

### Data flow

1. Test setup (standing in for a future reconciler) calls
   `ZfsManager::clone_from_base(base_zvol, vm_zvol)` directly.
2. `VmRuntime::create(name, /dev/zvol/<pool>/<vm_zvol>, cpus, memory_bytes)`
   spawns bhyve.
3. `VmRuntime::is_running(name)` confirms the vmm device exists.
4. `VmRuntime::destroy(name)` stops the process and releases the vmm
   device.
5. Test teardown calls `ZfsManager::destroy_dataset(vm_zvol)` directly to
   reclaim the disk clone.

No orchestration layer ties these steps together yet — that's the explicit
job of whichever future milestone wires `keel-vm` into `keel-agentd`.

## Error Handling

`VmError` follows the same shape as `JailError`/`ZfsError`: one variant per
failure mode surfaced by shelling out (command spawn failure, non-zero
exit with captured stderr, "VM already exists" on a `create` collision).
`destroy` on a VM that was never created is not an error, matching
`JailRuntime::destroy`'s existing idempotent-on-absence contract, so a
test's `let _ = runtime.destroy(name);` cleanup-before-run pattern (used
throughout `keel-jail`'s own tests) works unchanged for `keel-vm`.

## Testing Strategy

- Unit tests against `FakeVmRuntime`, covering `create`/`is_running`/
  `destroy` transitions, runnable on any OS.
- `keel-vm/tests/freebsd_vm_lifecycle.rs`, gated `#![cfg(target_os =
  "freebsd")]` like every other real-hardware test in this project, run as
  root on the FreeBSD VM:
  1. Clone a VM disk from a pre-existing base zvol (documented prerequisite,
     mirroring the `zroot/keel/base/test` dataset prerequisite
     `keel-jail`'s own `freebsd_lifecycle.rs` already documents).
  2. `create` the VM, assert `is_running` becomes `true`.
  3. `destroy` the VM, assert `is_running` becomes `false`.
  4. Destroy the disk clone via `keel-zfs` directly.

Getting the real mechanics right — the actual timing of `/dev/vmm/<name>`'s
appearance and release, whatever `bhyvectl --destroy` race conditions turn
up against real hardware — is expected to need the same real-VM iteration
Milestone 2 needed for `jail(8)`/`rctl(8)`; this design does not assume the
first attempt at the real implementation will be correct.

## Open Questions / Deferred Decisions

- Whether `create`'s non-blocking contract needs a way to detect a launch
  failure that happens *inside* bhyve after the process spawns successfully
  (a bad disk path, a firmware file that doesn't exist) is left to
  implementation-time discovery against real hardware, the same way
  `JailRuntime::start_command`'s own doc comment already flags this same
  class of gap for jails.
- Serial console access (`-l com1,/dev/nmdm0A`) is not part of this
  milestone's `VmRuntime` surface. If real-VM verification turns out to
  need console output to confirm a guest actually finished booting (rather
  than just checking the vmm device exists), that's an implementation-time
  addition to the real test, not a design change.
