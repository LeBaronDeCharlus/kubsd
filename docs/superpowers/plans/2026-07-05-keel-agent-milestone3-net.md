# keel-agent Milestone 3: keel-net Crate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Tasks 3, 4, 5:** each has a FreeBSD-only integration
> test step that needs the real VM (`root@192.168.64.2`). The coordinating
> session has direct SSH access to this VM (confirmed working mid-way
> through Milestone 2) — use it directly; relaying through the human via
> `!` is only a fallback if that access is ever lost again.

**Goal:** Build `keel-net` — the crate that wires a jail onto a shared
bridge with VNET + `epair(4)`, giving jails L2 connectivity to each other
and the host. No NAT/outbound internet access this milestone (deliberately
deferred — see design spec). Also fixes a gap surfaced by this work:
`keel-jail`'s `create` (shipped in Milestone 2) has no way to create a
VNET-enabled jail, which `keel-net` requires.

**Architecture:** Same fake-vs-real trait pattern as `keel-jail`/`keel-zfs`:
a `NetManager` trait, a `FakeNetManager` (in-memory, macOS-testable), and a
`ProcessNetManager` that shells out to `ifconfig(8)`/`jexec(8)`, verified
only on the FreeBSD VM. All exact command sequences below were verified
directly against the real VM before writing this plan (bridge creation,
epair creation/attachment, VNET migration, and every idempotency edge
case) — this plan does not guess at FreeBSD command syntax.

**Tech Stack:** Rust (2021 edition), `thiserror` for the error type,
`std::process::Command` for shelling out — same as `keel-jail`/`keel-zfs`.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-05-keel-agent-design.md` (Approved). The `NetManager` trait signature and the updated `JailRuntime::create` signature there must match exactly.
- Target FreeBSD VM: `root@192.168.64.2`, FreeBSD 15.1-RELEASE-p1 aarch64. `if_bridge`/`if_epair` kernel modules are already loaded (from Milestone 1) — no new VM kernel prep needed this milestone.
- No NAT/outbound internet access in `keel-net` this milestone — bridge-only L2 connectivity.
- `keel-net` takes plain bridge/epair/jail names and addresses it's given — no knowledge of keel's naming conventions.
- Every real (`ProcessNetManager`) method needs a FreeBSD-VM integration test in addition to macOS-side unit tests.
- No placeholders: every new function has a passing test.

---

### Task 1: Add `vnet` parameter to `JailRuntime::create`

VNET must be set at jail-creation time and can't be added retroactively,
so `keel-net`'s `attach_jail` (Task 4) needs jails created with
`vnet: true`. This is an early breaking change to `keel-jail`'s
already-shipped `create` signature — see the design spec's rationale.

**Files:**
- Modify: `keel-jail/src/lib.rs`
- Modify: `keel-jail/src/fake.rs`
- Modify: `keel-jail/src/process.rs`
- Modify: `keel-jail/tests/freebsd_lifecycle.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `JailRuntime::create`'s signature becomes `fn create(&self,
  name: &str, rootfs: &Path, vnet: bool) -> Result<(), JailError>`. Task 4
  of this plan is the first real caller that passes `vnet: true`.

- [ ] **Step 1: Update the trait signature**

Modify `keel-jail/src/lib.rs` — change the `create` line in the
`JailRuntime` trait to:

```rust
    fn create(&self, name: &str, rootfs: &Path, vnet: bool) -> Result<(), JailError>;
```

(Every other line in the trait and the rest of the file stays unchanged.)

- [ ] **Step 2: Update FakeJailRuntime and its tests**

Modify `keel-jail/src/fake.rs`. Change the `FakeJail` struct to add a
`vnet` field:

```rust
struct FakeJail {
    #[allow(dead_code)]
    rootfs: PathBuf,
    #[allow(dead_code)]
    vnet: bool,
    running: bool,
    pcpu_percent: Option<u32>,
    memory_bytes: Option<u64>,
}
```

Change the `create` method:

```rust
    fn create(&self, name: &str, rootfs: &Path, vnet: bool) -> Result<(), JailError> {
        self.jails.lock().unwrap().insert(
            name.to_string(),
            FakeJail { rootfs: rootfs.to_path_buf(), vnet, running: false, pcpu_percent: None, memory_bytes: None },
        );
        Ok(())
    }
```

Update every `.create(name, rootfs)` call in this file's test module to
`.create(name, rootfs, false)` (six call sites, all in `#[cfg(test)] mod
tests`: `create_then_is_running_is_false_until_start_command`,
`start_command_makes_is_running_true`, `destroy_removes_the_jail`,
`set_and_remove_resource_limits_do_not_error_on_known_jail`,
`mark_exited_makes_is_running_false_without_destroying` — this last one
calls `create` once — count carefully, there are 5 call sites total in
this file, one per test that calls `create` at all; `operations_on_unknown_jail_return_not_found`
doesn't call `create`). None of these tests exercise networking, so pass
`false` for all of them.

- [ ] **Step 3: Update ProcessJailRuntime**

Modify `keel-jail/src/process.rs` — replace the `create` method:

```rust
    fn create(&self, name: &str, rootfs: &Path, vnet: bool) -> Result<(), JailError> {
        let path_arg = format!("path={}", rootfs.display());
        let name_arg = format!("name={name}");
        let mut args: Vec<&str> = vec!["-c", &name_arg, &path_arg];
        if vnet {
            args.push("vnet");
        }
        args.push("persist");
        Self::run_checked("jail", &args)
    }
```

- [ ] **Step 4: Update the FreeBSD integration test's call sites**

Modify `keel-jail/tests/freebsd_lifecycle.rs` — update all four
`runtime.create(name, rootfs)` calls (in
`create_destroy_and_is_running_lifecycle`,
`start_command_makes_is_running_true`, `set_and_remove_resource_limits`,
`remove_resource_limits_on_jail_with_no_limits_set_is_a_no_op_success`) to
`runtime.create(name, rootfs, false)`. None of these tests need
networking.

- [ ] **Step 5: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 28 tests total (unchanged — this is a signature
change plus call-site updates, no new tests).

- [ ] **Step 6: Run the real integration test on the VM**

Run directly (you have SSH access):
```
ssh root@192.168.64.2 'cd ~/keel && git pull && cargo test -p keel-jail --test freebsd_lifecycle 2>&1 | tail -15'
```
(Push this task's commit first — see Step 7 — so `git pull` picks it up.)

Expected: all four tests in `freebsd_lifecycle.rs` still pass with
`vnet: false` (identical behavior to before, since `false` produces the
exact same `jail -c` invocation as previously).

- [ ] **Step 7: Commit and push**

```bash
git add keel-jail/src/lib.rs keel-jail/src/fake.rs keel-jail/src/process.rs keel-jail/tests/freebsd_lifecycle.rs
git commit -m "Add vnet parameter to JailRuntime::create"
git push origin HEAD
```

(`git push origin HEAD` pushes whatever branch is currently checked out —
push to your actual feature branch, not `master`/`main` directly.)

---

### Task 2: keel-net scaffold — NetError, NetManager trait, FakeNetManager

**Files:**
- Create: `keel-net/Cargo.toml`
- Create: `keel-net/src/error.rs`
- Create: `keel-net/src/lib.rs`
- Create: `keel-net/src/fake.rs`
- Modify: `Cargo.toml` (workspace root — add `keel-net` to `members`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub enum NetError` (variants: `Spawn(String, std::io::Error)`,
  `CommandFailed(String, std::process::ExitStatus, String)`,
  `NotFound(String)`); `pub trait NetManager` with the three methods
  below; `pub struct FakeNetManager` implementing it. Tasks 3-5 implement
  `ProcessNetManager` against this same trait.

- [ ] **Step 1: Add keel-net to the workspace**

Modify `Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = ["keel-spec", "keel-jail", "keel-zfs", "keel-net"]
```

- [ ] **Step 2: Create the crate manifest**

Create `keel-net/Cargo.toml`:

```toml
[package]
name = "keel-net"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"

[dev-dependencies]
keel-jail = { path = "../keel-jail" }
```

(The `keel-jail` dev-dependency is for Task 4's integration test, which
needs a real jail to attach networking to — it is NOT a runtime
dependency of `keel-net` itself, which stays FreeBSD-tool-agnostic about
how jails get created.)

- [ ] **Step 3: Write the error type**

Create `keel-net/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetError {
    #[error("failed to spawn `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed with exit status {1}: {2}")]
    CommandFailed(String, std::process::ExitStatus, String),
    #[error("bridge '{0}' not found")]
    NotFound(String),
}
```

- [ ] **Step 4: Write the trait and FakeNetManager with tests**

Create `keel-net/src/fake.rs`:

```rust
use crate::NetError;
use crate::NetManager;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeNetManager {
    bridges: Mutex<HashSet<String>>,
    attachments: Mutex<HashMap<String, (String, String, String)>>,
}

impl FakeNetManager {
    pub fn new() -> Self {
        Self::default()
    }
}

impl NetManager for FakeNetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError> {
        self.bridges.lock().unwrap().insert(bridge.to_string());
        Ok(())
    }

    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError> {
        if !self.bridges.lock().unwrap().contains(bridge) {
            return Err(NetError::NotFound(bridge.to_string()));
        }
        self.attachments.lock().unwrap().insert(
            epair_base.to_string(),
            (jail_name.to_string(), bridge.to_string(), address.to_string()),
        );
        Ok(())
    }

    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError> {
        self.attachments.lock().unwrap().remove(epair_base);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_bridge_exists_is_idempotent() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.ensure_bridge_exists("keel0").unwrap();
    }

    #[test]
    fn attach_jail_requires_bridge_to_exist_first() {
        let net = FakeNetManager::new();
        assert!(matches!(
            net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24"),
            Err(NetError::NotFound(_))
        ));
    }

    #[test]
    fn attach_jail_succeeds_after_ensure_bridge_exists() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
    }

    #[test]
    fn detach_jail_on_unknown_epair_is_a_no_op_success() {
        let net = FakeNetManager::new();
        net.detach_jail("epair-never-attached").unwrap();
    }

    #[test]
    fn detach_then_reattach_works() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
        net.detach_jail("epair7").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
    }
}
```

Create `keel-net/src/lib.rs`:

```rust
pub mod error;
pub mod fake;

pub use error::NetError;
pub use fake::FakeNetManager;

pub trait NetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError>;
    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError>;
    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError>;
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace -p keel-net`

Expected: PASS, 5 tests (`ensure_bridge_exists_is_idempotent`,
`attach_jail_requires_bridge_to_exist_first`,
`attach_jail_succeeds_after_ensure_bridge_exists`,
`detach_jail_on_unknown_epair_is_a_no_op_success`,
`detach_then_reattach_works`).

- [ ] **Step 6: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 33 tests total (28 from before this milestone + 5 new).

- [ ] **Step 7: Commit and push**

```bash
git add Cargo.toml keel-net/Cargo.toml keel-net/src/error.rs keel-net/src/lib.rs keel-net/src/fake.rs
git commit -m "Add keel-net crate: NetManager trait and FakeNetManager"
git push origin HEAD
```

---

### Task 3: ProcessNetManager — ensure_bridge_exists

**Files:**
- Create: `keel-net/src/process.rs`
- Modify: `keel-net/src/lib.rs`
- Create: `keel-net/tests/freebsd_net.rs`

**Interfaces:**
- Consumes: `NetError`, `NetManager` (Task 2).
- Produces: `pub struct ProcessNetManager` implementing `ensure_bridge_exists`
  for real (`attach_jail`/`detach_jail` are added in Tasks 4-5 and can
  `unimplemented!()` until then).

**Verified exact behavior** (confirmed live on the FreeBSD VM before
writing this task): checking a nonexistent named interface (`ifconfig
keel0`) exits 1 with stderr `interface keel0 does not exist`. Creating a
bridge (`ifconfig bridge create`) prints the assigned name (e.g.
`bridge0`) to stdout and exits 0 — the name is NOT predictable in advance
(it depends on what bridge numbers are already in use), so it must be
captured from the command's actual output, never assumed. Renaming
(`ifconfig bridge0 name keel0`) and bringing up (`ifconfig keel0 up`)
both exit 0 on success.

- [ ] **Step 1: Write the implementation**

Create `keel-net/src/process.rs`:

```rust
use crate::NetError;
use crate::NetManager;
use std::process::{Command, Output};

pub struct ProcessNetManager;

impl ProcessNetManager {
    pub fn new() -> Self {
        Self
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, NetError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| NetError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), NetError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn stderr_contains(output: &Output, needle: &str) -> bool {
        String::from_utf8_lossy(&output.stderr).contains(needle)
    }
}

impl Default for ProcessNetManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NetManager for ProcessNetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError> {
        let check = Self::run("ifconfig", &[bridge])?;
        if check.status.success() {
            return Ok(());
        }
        let created = Self::run("ifconfig", &["bridge", "create"])?;
        if !created.status.success() {
            return Err(NetError::CommandFailed(
                "ifconfig bridge create".to_string(),
                created.status,
                String::from_utf8_lossy(&created.stderr).into_owned(),
            ));
        }
        let created_name = String::from_utf8_lossy(&created.stdout).trim().to_string();
        Self::run_checked("ifconfig", &[&created_name, "name", bridge])?;
        Self::run_checked("ifconfig", &[bridge, "up"])
    }

    fn attach_jail(&self, _jail_name: &str, _bridge: &str, _epair_base: &str, _address: &str) -> Result<(), NetError> {
        unimplemented!("added in Task 4")
    }

    fn detach_jail(&self, _epair_base: &str) -> Result<(), NetError> {
        unimplemented!("added in Task 5")
    }
}
```

Modify `keel-net/src/lib.rs` — add `pub mod process;` and `pub use
process::ProcessNetManager;`:

```rust
pub mod error;
pub mod fake;
pub mod process;

pub use error::NetError;
pub use fake::FakeNetManager;
pub use process::ProcessNetManager;

pub trait NetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError>;
    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError>;
    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError>;
}
```

- [ ] **Step 2: Write the FreeBSD-only integration test**

Create `keel-net/tests/freebsd_net.rs`:

```rust
#![cfg(target_os = "freebsd")]

use keel_net::{NetManager, ProcessNetManager};
use std::process::Command;

// Run as root on the FreeBSD VM: `sudo cargo test -p keel-net --test freebsd_net`

fn destroy_interface_if_exists(name: &str) {
    let _ = Command::new("ifconfig").args([name, "destroy"]).output();
}

#[test]
fn ensure_bridge_exists_creates_and_is_idempotent() {
    let net = ProcessNetManager::new();
    let bridge = "keel-test-br0";
    destroy_interface_if_exists(bridge);

    net.ensure_bridge_exists(bridge).expect("first call should create the bridge");
    let check = Command::new("ifconfig").arg(bridge).output().expect("ifconfig should run");
    assert!(check.status.success(), "bridge should exist after ensure_bridge_exists");

    net.ensure_bridge_exists(bridge).expect("second call should be a no-op success");

    destroy_interface_if_exists(bridge);
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 33 tests total (unchanged — this test file is
FreeBSD-only).

- [ ] **Step 4: Commit and push**

```bash
git add keel-net/src/process.rs keel-net/src/lib.rs keel-net/tests/freebsd_net.rs
git commit -m "Add ProcessNetManager ensure_bridge_exists"
git push origin HEAD
```

- [ ] **Step 5: Run the real integration test on the VM**

Run: `ssh root@192.168.64.2 'cd ~/keel && git pull && cargo test -p keel-net --test freebsd_net 2>&1 | tail -15'`

Expected: `test ensure_bridge_exists_creates_and_is_idempotent ... ok`.

---

### Task 4: ProcessNetManager — attach_jail

**Files:**
- Modify: `keel-net/src/process.rs`
- Modify: `keel-net/tests/freebsd_net.rs`

**Interfaces:**
- Consumes: `NetError`, `NetManager`, `ProcessNetManager` (Task 3);
  `keel_jail::{JailRuntime, ProcessJailRuntime}` (Milestone 2, with
  Task 1's `vnet` parameter) for the integration test's jail fixture.
- Produces: a working `attach_jail` on `ProcessNetManager`.

**Verified exact behavior** (confirmed live on the VM): `ifconfig
<epair_base> create` creates both `<epair_base>a` and `<epair_base>b` in
one call (prints the `a` name to stdout); retrying it when the pair
already exists fails with stderr containing `already exists`. `ifconfig
<bridge> addm <epair_base>a` adds the interface to the bridge; retrying
when it's already a member fails with stderr containing `File exists`
(message: `BRDGADD ...: File exists (Interface is already a member of
this bridge)`). `ifconfig <epair_base>b vnet <jail_name>` moves that
interface into the jail's VNET — confirmed the interface disappears from
`ifconfig -l` on the host and becomes queryable via `jexec <jail_name>
ifconfig <epair_base>b` instead. Configuring the address must happen
*inside* the jail via `jexec` (`jexec <jail_name> /sbin/ifconfig
<epair_base>b inet <address>` then `... up`) since the interface now lives
in the jail's own network stack — the base image needs `/sbin/ifconfig`
present (out of scope for this crate; the test below provisions its own
minimal rootfs with it, copied from `/rescue/ifconfig`, same pattern as
Milestone 2's `/rescue/sh`/`/rescue/sleep`).

- [ ] **Step 1: Implement attach_jail**

In `keel-net/src/process.rs`, replace the `unimplemented!()` body:

```rust
    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError> {
        let epair_a = format!("{epair_base}a");
        let epair_b = format!("{epair_base}b");

        let create = Self::run("ifconfig", &[epair_base, "create"])?;
        if !create.status.success() && !Self::stderr_contains(&create, "already exists") {
            return Err(NetError::CommandFailed(
                format!("ifconfig {epair_base} create"),
                create.status,
                String::from_utf8_lossy(&create.stderr).into_owned(),
            ));
        }

        let addm = Self::run("ifconfig", &[bridge, "addm", &epair_a])?;
        if !addm.status.success() && !Self::stderr_contains(&addm, "File exists") {
            return Err(NetError::CommandFailed(
                format!("ifconfig {bridge} addm {epair_a}"),
                addm.status,
                String::from_utf8_lossy(&addm.stderr).into_owned(),
            ));
        }

        Self::run_checked("ifconfig", &[&epair_a, "up"])?;

        let vnet_move = Self::run("ifconfig", &[&epair_b, "vnet", jail_name])?;
        if !vnet_move.status.success() {
            // Might already be moved from an interrupted prior attempt —
            // check whether it's already correctly placed in the target jail.
            let already_there = Self::run("jexec", &[jail_name, "/sbin/ifconfig", &epair_b])?;
            if !already_there.status.success() {
                return Err(NetError::CommandFailed(
                    format!("ifconfig {epair_b} vnet {jail_name}"),
                    vnet_move.status,
                    String::from_utf8_lossy(&vnet_move.stderr).into_owned(),
                ));
            }
        }

        Self::run_checked("jexec", &[jail_name, "/sbin/ifconfig", &epair_b, "inet", address])?;
        Self::run_checked("jexec", &[jail_name, "/sbin/ifconfig", &epair_b, "up"])
    }
```

- [ ] **Step 2: Extend the FreeBSD integration test**

Add to `keel-net/tests/freebsd_net.rs`:

```rust
use keel_jail::{JailRuntime, ProcessJailRuntime};
use std::path::Path;

fn make_test_jail(name: &str) -> ProcessJailRuntime {
    let jails = ProcessJailRuntime::new();
    let _ = jails.destroy(name);
    let rootfs = Path::new("/tmp").join(format!("{name}-rootfs"));
    let bin_dir = rootfs.join("sbin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::copy("/rescue/ifconfig", bin_dir.join("ifconfig")).expect("copy /rescue/ifconfig into test rootfs");
    jails.create(name, &rootfs, true).expect("create should succeed"); // vnet: true
    jails
}

#[test]
fn attach_jail_wires_up_epair_and_configures_address() {
    let net = ProcessNetManager::new();
    let bridge = "keel-test-br1";
    let jail_name = "keel-net-test-attach";
    let epair_base = "epair50";

    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
    let jails = make_test_jail(jail_name);

    net.ensure_bridge_exists(bridge).expect("bridge should be created");
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.5/24")
        .expect("attach_jail should succeed");

    let inside = Command::new("jexec")
        .args([jail_name, "/sbin/ifconfig", &format!("{epair_base}b")])
        .output()
        .expect("jexec ifconfig should run");
    let inside_output = String::from_utf8_lossy(&inside.stdout);
    assert!(inside_output.contains("10.99.0.5"), "expected configured address in: {inside_output}");

    jails.destroy(jail_name).expect("cleanup destroy should succeed");
    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
}

#[test]
fn attach_jail_tolerates_retry_after_epair_already_created() {
    let net = ProcessNetManager::new();
    let bridge = "keel-test-br2";
    let jail_name = "keel-net-test-retry";
    let epair_base = "epair51";

    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
    let jails = make_test_jail(jail_name);

    net.ensure_bridge_exists(bridge).expect("bridge should be created");
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.6/24")
        .expect("first attach_jail should succeed");

    // Simulate a retry after an interrupted prior attempt: calling
    // attach_jail again for the same epair_base (now fully wired into the
    // jail) must not error.
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.6/24")
        .expect("retried attach_jail should tolerate already-attached state");

    jails.destroy(jail_name).expect("cleanup destroy should succeed");
    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 33 tests total (unchanged — these test files are
FreeBSD-only).

- [ ] **Step 4: Commit and push**

```bash
git add keel-net/src/process.rs keel-net/tests/freebsd_net.rs keel-net/Cargo.toml
git commit -m "Add ProcessNetManager attach_jail"
git push origin HEAD
```

- [ ] **Step 5: Run the real integration tests on the VM**

Run: `ssh root@192.168.64.2 'cd ~/keel && git pull && cargo test -p keel-net --test freebsd_net 2>&1 | tail -20'`

Expected: all three tests pass, including the two new ones. If
`attach_jail_tolerates_retry_after_epair_already_created` fails on the
`addm` step (bridge member re-add), double check the exact stderr text
against what this task's code checks for (`File exists`) — re-verify with
a manual `ifconfig <bridge> addm <if>` twice on the VM if needed, since
FreeBSD versions can phrase this differently.

---

### Task 5: ProcessNetManager — detach_jail

**Files:**
- Modify: `keel-net/src/process.rs`
- Modify: `keel-net/tests/freebsd_net.rs`

**Interfaces:**
- Consumes: `NetError`, `NetManager`, `ProcessNetManager` (Tasks 3-4).
- Produces: a working `detach_jail` on `ProcessNetManager`. This
  completes the `NetManager` trait and Milestone 3.

**Verified exact behavior**: destroying the `a` side of an epair pair
(`ifconfig <epair_base>a destroy`) destroys both sides together, even
while the `b` side is currently inside a *running* jail's VNET (confirmed:
the jail keeps running fine afterward). Also confirmed: destroying a jail
does **not** automatically clean up an epair that was moved into it — the
`b` side reappears on the host, orphaned — so `detach_jail` doing this
explicitly, before jail destroy, is required, not optional. Retrying
`destroy` on an already-gone interface fails with stderr containing `does
not exist`.

- [ ] **Step 1: Implement detach_jail**

In `keel-net/src/process.rs`, replace the `unimplemented!()` body:

```rust
    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError> {
        let epair_a = format!("{epair_base}a");
        let output = Self::run("ifconfig", &[&epair_a, "destroy"])?;
        if output.status.success() || Self::stderr_contains(&output, "does not exist") {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("ifconfig {epair_a} destroy"),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
```

- [ ] **Step 2: Extend the FreeBSD integration test**

Add to `keel-net/tests/freebsd_net.rs`:

```rust
#[test]
fn detach_jail_removes_epair_and_is_idempotent() {
    let net = ProcessNetManager::new();
    let bridge = "keel-test-br3";
    let jail_name = "keel-net-test-detach";
    let epair_base = "epair52";

    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
    let jails = make_test_jail(jail_name);

    net.ensure_bridge_exists(bridge).expect("bridge should be created");
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.7/24")
        .expect("attach_jail should succeed");

    net.detach_jail(epair_base).expect("detach_jail should succeed");

    let check = Command::new("ifconfig").arg(format!("{epair_base}a")).output().expect("ifconfig should run");
    assert!(!check.status.success(), "epair should no longer exist on the host after detach");

    // Idempotent: detaching an already-detached epair must not error.
    net.detach_jail(epair_base).expect("second detach_jail call should be a no-op success");

    jails.destroy(jail_name).expect("cleanup destroy should succeed");
    destroy_interface_if_exists(bridge);
}

#[test]
fn detach_before_destroy_works_while_jail_is_still_running() {
    let net = ProcessNetManager::new();
    let bridge = "keel-test-br4";
    let jail_name = "keel-net-test-detach-order";
    let epair_base = "epair53";

    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
    let jails = make_test_jail(jail_name);

    net.ensure_bridge_exists(bridge).expect("bridge should be created");
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.8/24")
        .expect("attach_jail should succeed");

    // Detach while the jail is still running, matching the Reconciliation
    // Loop's stated order (detach network, then destroy the jail).
    net.detach_jail(epair_base).expect("detach_jail should succeed on a running jail");
    assert_eq!(jails.is_running(jail_name).unwrap(), false, "no command was ever started in this jail");

    jails.destroy(jail_name).expect("destroy after detach should still succeed");
    destroy_interface_if_exists(bridge);
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 33 tests total (unchanged).

- [ ] **Step 4: Commit and push**

```bash
git add keel-net/src/process.rs keel-net/tests/freebsd_net.rs
git commit -m "Add ProcessNetManager detach_jail"
git push origin HEAD
```

- [ ] **Step 5: Run the real integration tests on the VM**

Run: `ssh root@192.168.64.2 'cd ~/keel && git pull && cargo test -p keel-net --test freebsd_net 2>&1 | tail -25'`

Expected: all five tests in `freebsd_net.rs` pass. This completes
`keel-net` and Milestone 3.

## Milestone Exit Criteria

- `cargo test --workspace` passes with 33 tests on macOS (28 from before
  this milestone + 5 `keel-net` unit tests), with zero FreeBSD
  dependencies required to run them.
- On the FreeBSD VM: `cargo test -p keel-jail --test freebsd_lifecycle`
  (4 tests, using the new `vnet` parameter), and `cargo test -p keel-net
  --test freebsd_net` (5 tests) both pass as root.
- `keel-net::NetManager` is a complete, fully-implemented trait (no
  `unimplemented!()` left) ready for `keel-agentd`'s reconciliation loop
  (a later milestone) to compose against a fake in its own unit tests,
  alongside `keel-jail` and `keel-zfs`.
