# Milestone 20: Single-Node bhyve VM Lifecycle (Sub-Project 8, First Milestone)

Status: Approved, shelved (blocked on real-hardware infra — see note below)
Date: 2026-07-22

**2026-07-22 update:** Implementation was paused before it started. The
project's FreeBSD test VM (`root@192.168.64.2`) is arm64, not amd64, which
already invalidates this doc's `-l bootrom,<UEFI firmware path>` framing —
arm64 bhyve boots via the generic `-o bootrom=<path>` option with a
U-Boot image (`u-boot-bhyve-arm64` package), not amd64's edk2/UEFI
firmware, and has no `lpc` PCI device model. Worse, a live smoke test
(`bhyve -c 1 -m 256M -s 0,hostbridge -s 3,virtio-blk,<zvol> -o
bootrom=/usr/local/share/u-boot/u-boot-bhyve-arm64/u-boot.bin <name>`)
fails at the kernel level with `vmm: Processor doesn't have support for
virtualization`: this FreeBSD guest itself runs nested under UTM/QEMU
with HVF acceleration on the Apple Silicon host, and that hypervisor
doesn't expose EL2/nested-virt to the guest, so `vmm.ko` cannot create
any VM context regardless of bhyve flags. This is a test-infrastructure
gap, not a flaw in the design below. Apple Silicon (M3+, macOS 15+) does
support nested virtualization, but only via UTM's Apple Virtualization
backend (not QEMU), and Apple's own docs only confirm it for Linux
guests — whether FreeBSD's `vmm.ko` works under it at all is unconfirmed.
Given this project's methodology of verifying every FreeBSD-specific
behavior on real hardware before locking in a plan (see README), and
that bhyve support blocks nothing else on the roadmap, this milestone is
shelved rather than implemented against unverifiable mechanics. Revisit
once real bhyve-capable FreeBSD hardware (bare metal, a cloud instance
with nested virt, or a confirmed-working Apple Virtualization setup) is
available — at which point the `-o bootrom=`/U-Boot correction above
must be folded into the Architecture section before writing an
implementation plan.

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

### `keel-vm`: a new workspace member alongside `keel-jail`

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

- How `destroy` locates the running bhyve process to stop when it's still
  alive. `JailRuntime::destroy`'s real implementation doesn't need an
  in-memory `Child` handle for this — `jail -r <name>` kills every process
  in the jail directly via the OS jail subsystem, authoritative regardless
  of what `ProcessJailRuntime` remembers, and its own `children: Mutex<Vec<
  (String, Child)>>` bookkeeping exists only to reap already-dead zombies
  afterward. bhyve has no equivalent "stop everything for this VM name"
  primitive: the process is just a plain PID with no OS-level indexing by
  VM name, so if `BhyveVmRuntime` tracks it only as an in-memory `Child`,
  that link is lost across a `keel-agentd` restart — in tension with this
  project's stated crash-safety bar ("killing the daemon...never leaves it
  confused about what it manages"). Whether this milestone's `destroy`
  needs to locate the process independent of any in-memory handle (e.g.
  `pgrep -f` matching `name` in the bhyve command line) or whether
  `bhyvectl --destroy` alone reliably force-exits the attached bhyve
  process without one is left to real-hardware discovery, same as the
  other mechanics below.
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
