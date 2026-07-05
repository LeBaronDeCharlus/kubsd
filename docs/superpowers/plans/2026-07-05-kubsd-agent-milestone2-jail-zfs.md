# kubsd-agent Milestone 2: kubsd-jail + kubsd-zfs Crates — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 1:** it requires SSH access to a FreeBSD VM that is
> only reachable from the coordinating session's relay to the user (the
> assistant's shell cannot reach `192.168.64.2` directly — confirmed in
> Milestone 1). Every FreeBSD-integration-test task in this plan (Tasks 3,
> 4, 5, 7, 8) also needs a `git push` + relayed `git pull` + `cargo test`
> cycle on the VM — these cannot be run by an isolated subagent either, and
> must be driven by whichever session is talking to the user directly.

**Goal:** Build `kubsd-jail` (jail lifecycle + resource limits via
`jail(8)`/`jexec(8)`/`jls(8)`/`rctl(8)`) and `kubsd-zfs` (ZFS clone
provisioning via `zfs(8)`) — the two crates that give kubsd-agentd a real,
testable way to create and tear down non-networked jails from a base ZFS
image. VNET networking is explicitly out of scope (that's `kubsd-net`, a
later milestone).

**Architecture:** Each crate exposes one trait (`JailRuntime`,
`ZfsManager`) with two implementations: an in-memory `Fake*` used for
unit tests (runs anywhere, no FreeBSD needed) and a real
`Process*`/`Cli*` implementation that shells out to FreeBSD CLI tools
(only runs on the FreeBSD VM). This mirrors `kubsd-spec`'s
fake-vs-real trait pattern from Milestone 1.

**Tech Stack:** Rust (2021 edition), `thiserror` for error types,
`std::process::Command` for shelling out (no new external dependencies).

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-05-kubsd-agent-design.md` (Approved). The `JailRuntime` and `ZfsManager` trait signatures below are copied verbatim from its Architecture section — do not deviate from them.
- Target FreeBSD VM for all real (non-mocked) work: `root@192.168.64.2`, FreeBSD 15.1-RELEASE-p1 aarch64, ZFS pool `zroot`, Rust via `pkg` (not `rustup`).
- No VNET/networking in `kubsd-jail` or `kubsd-zfs` this milestone.
- `kubsd-jail` and `kubsd-zfs` take plain names/paths — neither crate knows about kubsd's `kubsd-<name>` jail-naming or `<pool>/kubsd/...` dataset-path conventions. Those are `kubsd-agentd`'s job (a later milestone).
- Every task's real (`Process*`/`Cli*`) implementation change needs a FreeBSD-VM integration test in addition to macOS-side unit tests, per the design spec's Testing Strategy section.
- No placeholders: every new function has a passing test; every FreeBSD-only integration test file starts with `#![cfg(target_os = "freebsd")]` so `cargo test --workspace` on macOS neither compiles nor runs it.

---

### Task 1: Create the FreeBSD-side test ZFS dataset

**Human-in-the-loop task**, same reasoning as Milestone 1's Task 1: every
command below must be run by the user via `! <command>` in the Claude Code
prompt.

**Files:** None (infrastructure only).

**Interfaces:**
- Produces: a ZFS dataset `zroot/kubsd/base/test` on the VM that Tasks 7
  and 8's integration tests clone from.

- [ ] **Step 1: Create the test base dataset**

Run: `! ssh root@192.168.64.2 'zfs create -p zroot/kubsd/base/test && zfs list zroot/kubsd/base/test'`

Expected: the dataset is created and `zfs list` shows it with `zroot` as
its pool.

- [ ] **Step 2: One-time repo clone on the VM**

Run: `! ssh root@192.168.64.2 'git clone git@github.com:LeBaronDeCharlus/kubsd.git ~/kubsd 2>&1 || (cd ~/kubsd && git pull)'`

Expected: the repo clones successfully into `~/kubsd` on the VM (or, if it
already exists from a prior run, pulls latest). This is the workflow every
later integration-test task in this plan reuses: push from your Mac, then
`ssh ... 'cd ~/kubsd && git pull && cargo test ...'` on the VM.

- [ ] **Step 3: Verify the toolchain can build the current tree**

Run: `! ssh root@192.168.64.2 'cd ~/kubsd && cargo test --workspace 2>&1 | tail -5'`

Expected: the 17 tests from Milestone 1 (`kubsd-spec`) pass on the VM too
— confirms the VM's Rust toolchain can build this workspace before any
FreeBSD-specific code exists in it.

- [ ] **Step 4: No commit needed**

This task only touches the remote VM's ZFS pool and its own local clone of
the repo — nothing in this repository changes.

---

### Task 2: kubsd-jail scaffold — JailError, JailRuntime trait, FakeJailRuntime

**Files:**
- Create: `kubsd-jail/Cargo.toml`
- Create: `kubsd-jail/src/error.rs`
- Create: `kubsd-jail/src/lib.rs`
- Create: `kubsd-jail/src/fake.rs`
- Modify: `Cargo.toml` (workspace root — add `kubsd-jail` to `members`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub enum JailError` (variants: `Spawn(String, std::io::Error)`,
  `CommandFailed(String, std::process::ExitStatus, String)`,
  `NotFound(String)`); `pub trait JailRuntime` with the five methods below;
  `pub struct FakeJailRuntime` implementing it. Tasks 3-5 implement
  `ProcessJailRuntime` against this same trait.

- [ ] **Step 1: Add kubsd-jail to the workspace**

Modify `Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = ["kubsd-spec", "kubsd-jail"]
```

- [ ] **Step 2: Create the crate manifest**

Create `kubsd-jail/Cargo.toml`:

```toml
[package]
name = "kubsd-jail"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"
```

- [ ] **Step 3: Write the error type**

Create `kubsd-jail/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JailError {
    #[error("failed to spawn `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed with exit status {1}: {2}")]
    CommandFailed(String, std::process::ExitStatus, String),
    #[error("jail '{0}' not found")]
    NotFound(String),
}
```

- [ ] **Step 4: Write the trait and the failing test for FakeJailRuntime**

Create `kubsd-jail/src/fake.rs`:

```rust
use crate::JailError;
use crate::JailRuntime;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct FakeJail {
    #[allow(dead_code)]
    rootfs: PathBuf,
    running: bool,
    pcpu_percent: Option<u32>,
    memory_bytes: Option<u64>,
}

#[derive(Default)]
pub struct FakeJailRuntime {
    jails: Mutex<HashMap<String, FakeJail>>,
}

impl FakeJailRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

impl JailRuntime for FakeJailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError> {
        self.jails.lock().unwrap().insert(
            name.to_string(),
            FakeJail { rootfs: rootfs.to_path_buf(), running: false, pcpu_percent: None, memory_bytes: None },
        );
        Ok(())
    }

    fn start_command(&self, name: &str, _command: &[String]) -> Result<(), JailError> {
        let mut jails = self.jails.lock().unwrap();
        let jail = jails.get_mut(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        jail.running = true;
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), JailError> {
        self.jails.lock().unwrap().remove(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        Ok(())
    }

    fn is_running(&self, name: &str) -> Result<bool, JailError> {
        Ok(self.jails.lock().unwrap().get(name).map(|j| j.running).unwrap_or(false))
    }

    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError> {
        let mut jails = self.jails.lock().unwrap();
        let jail = jails.get_mut(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        jail.pcpu_percent = Some(pcpu_percent);
        jail.memory_bytes = Some(memory_bytes);
        Ok(())
    }

    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError> {
        let mut jails = self.jails.lock().unwrap();
        let jail = jails.get_mut(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        jail.pcpu_percent = None;
        jail.memory_bytes = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_is_running_is_false_until_start_command() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        assert_eq!(runtime.is_running("test-1").unwrap(), false);
    }

    #[test]
    fn start_command_makes_is_running_true() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        runtime.start_command("test-1", &["/bin/sh".to_string()]).unwrap();
        assert_eq!(runtime.is_running("test-1").unwrap(), true);
    }

    #[test]
    fn destroy_removes_the_jail() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        runtime.destroy("test-1").unwrap();
        assert_eq!(runtime.is_running("test-1").unwrap(), false);
    }

    #[test]
    fn operations_on_unknown_jail_return_not_found() {
        let runtime = FakeJailRuntime::new();
        assert!(matches!(runtime.start_command("missing", &[]), Err(JailError::NotFound(_))));
        assert!(matches!(runtime.destroy("missing"), Err(JailError::NotFound(_))));
        assert!(matches!(runtime.set_resource_limits("missing", 100, 1024), Err(JailError::NotFound(_))));
        assert!(matches!(runtime.remove_resource_limits("missing"), Err(JailError::NotFound(_))));
    }

    #[test]
    fn set_and_remove_resource_limits_do_not_error_on_known_jail() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        runtime.set_resource_limits("test-1", 200, 512 * 1024 * 1024).unwrap();
        runtime.remove_resource_limits("test-1").unwrap();
    }
}
```

Create `kubsd-jail/src/lib.rs`:

```rust
pub mod error;
pub mod fake;

pub use error::JailError;
pub use fake::FakeJailRuntime;

use std::path::Path;

pub trait JailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError>;
    fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError>;
    fn destroy(&self, name: &str) -> Result<(), JailError>;
    fn is_running(&self, name: &str) -> Result<bool, JailError>;
    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError>;
    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError>;
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace -p kubsd-jail`

Expected: PASS, 5 tests (`create_then_is_running_is_false_until_start_command`,
`start_command_makes_is_running_true`, `destroy_removes_the_jail`,
`operations_on_unknown_jail_return_not_found`,
`set_and_remove_resource_limits_do_not_error_on_known_jail`).

- [ ] **Step 6: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 22 tests total (17 from `kubsd-spec` + 5 new).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml kubsd-jail/Cargo.toml kubsd-jail/src/error.rs kubsd-jail/src/lib.rs kubsd-jail/src/fake.rs
git commit -m "Add kubsd-jail crate: JailRuntime trait and FakeJailRuntime"
```

---

### Task 3: ProcessJailRuntime — create, destroy, is_running

**Files:**
- Create: `kubsd-jail/src/process.rs`
- Modify: `kubsd-jail/src/lib.rs`
- Create: `kubsd-jail/tests/freebsd_lifecycle.rs`

**Interfaces:**
- Consumes: `JailError`, `JailRuntime` (Task 2).
- Produces: `pub struct ProcessJailRuntime` implementing `create`,
  `destroy`, `is_running` for real (its `start_command`,
  `set_resource_limits`, `remove_resource_limits` are added in Tasks 4-5 —
  until then they can `unimplemented!()`, since this task's own test only
  exercises the three methods it implements).

- [ ] **Step 1: Write the implementation**

Create `kubsd-jail/src/process.rs`:

```rust
use crate::JailError;
use crate::JailRuntime;
use std::path::Path;
use std::process::{Command, Output};

pub struct ProcessJailRuntime;

impl ProcessJailRuntime {
    pub fn new() -> Self {
        Self
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, JailError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| JailError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), JailError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(JailError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

impl Default for ProcessJailRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl JailRuntime for ProcessJailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError> {
        let path_arg = format!("path={}", rootfs.display());
        Self::run_checked("jail", &["-c", &format!("name={name}"), &path_arg, "persist"])
    }

    fn destroy(&self, name: &str) -> Result<(), JailError> {
        Self::run_checked("jail", &["-r", name])
    }

    fn is_running(&self, name: &str) -> Result<bool, JailError> {
        let jls = Self::run("jls", &["-j", name, "jid"])?;
        if !jls.status.success() {
            return Ok(false);
        }
        let jid = String::from_utf8_lossy(&jls.stdout).trim().to_string();
        if jid.is_empty() {
            return Ok(false);
        }
        let ps = Self::run("ps", &["-J", &jid, "-o", "pid="])?;
        Ok(!String::from_utf8_lossy(&ps.stdout).trim().is_empty())
    }

    fn start_command(&self, _name: &str, _command: &[String]) -> Result<(), JailError> {
        unimplemented!("added in Task 4")
    }

    fn set_resource_limits(&self, _name: &str, _pcpu_percent: u32, _memory_bytes: u64) -> Result<(), JailError> {
        unimplemented!("added in Task 5")
    }

    fn remove_resource_limits(&self, _name: &str) -> Result<(), JailError> {
        unimplemented!("added in Task 5")
    }
}
```

Modify `kubsd-jail/src/lib.rs` — add `pub mod process;` and `pub use
process::ProcessJailRuntime;`:

```rust
pub mod error;
pub mod fake;
pub mod process;

pub use error::JailError;
pub use fake::FakeJailRuntime;
pub use process::ProcessJailRuntime;

use std::path::Path;

pub trait JailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError>;
    fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError>;
    fn destroy(&self, name: &str) -> Result<(), JailError>;
    fn is_running(&self, name: &str) -> Result<bool, JailError>;
    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError>;
    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError>;
}
```

- [ ] **Step 2: Write the FreeBSD-only integration test**

Create `kubsd-jail/tests/freebsd_lifecycle.rs`:

```rust
#![cfg(target_os = "freebsd")]

use kubsd_jail::{JailRuntime, ProcessJailRuntime};
use std::path::Path;

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-jail --test freebsd_lifecycle`
// (jail(8)/jls(8) require root privileges).

#[test]
fn create_destroy_and_is_running_lifecycle() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-lifecycle";
    let rootfs = Path::new("/tmp/kubsd-test-lifecycle-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    // Clean up any leftover jail from a previous failed run.
    let _ = runtime.destroy(name);

    runtime.create(name, rootfs).expect("create should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "no command started yet");

    runtime.destroy(name).expect("destroy should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "destroyed jail is not running");
}
```

- [ ] **Step 3: Run macOS-side checks (this test is skipped here, but confirm the crate still builds)**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean; test count unchanged from Task 2 (22 total) since
`freebsd_lifecycle.rs` compiles to nothing on macOS due to `#![cfg(target_os = "freebsd")]`.

- [ ] **Step 4: Push and run the real integration test on the VM**

Push this commit (see Step 5 below first, then push), then run:

`! ssh root@192.168.64.2 'cd ~/kubsd && git pull && cargo test -p kubsd-jail --test freebsd_lifecycle 2>&1 | tail -15'`

Expected: `test create_destroy_and_is_running_lifecycle ... ok`. If it
fails with a permissions error, re-run confirming the SSH session is root
(`whoami` should print `root`).

- [ ] **Step 5: Commit**

```bash
git add kubsd-jail/src/process.rs kubsd-jail/src/lib.rs kubsd-jail/tests/freebsd_lifecycle.rs
git commit -m "Add ProcessJailRuntime create/destroy/is_running"
git push origin master:main
```

(Push before Step 4's relayed VM test, so `git pull` on the VM picks it up.)

---

### Task 4: ProcessJailRuntime — start_command

**Files:**
- Modify: `kubsd-jail/src/process.rs`
- Modify: `kubsd-jail/tests/freebsd_lifecycle.rs`

**Interfaces:**
- Consumes: `JailError`, `JailRuntime`, `ProcessJailRuntime` (Task 3).
- Produces: a working `start_command` on `ProcessJailRuntime`. No new
  public interface — same trait method, now implemented for real.

- [ ] **Step 1: Implement start_command**

`start_command` runs `jexec <name> <command...>` via `Command::spawn()`
(never `.wait()` or `.output()`, since the design spec requires this call
not block for the process's lifetime). To avoid an unbounded pile-up of
zombie processes when jailed commands exit, `ProcessJailRuntime` keeps a
`Mutex<Vec<Child>>` of spawned children and does a best-effort non-blocking
reap (`try_wait()`) of finished ones on every call — this is a hygiene
measure, not a correctness dependency (per the design spec, `is_running`
always re-queries live system state and never trusts a remembered handle,
since handles don't survive a `kubsd-agentd` restart anyway).

Modify `kubsd-jail/src/process.rs` — replace the whole file:

```rust
use crate::JailError;
use crate::JailRuntime;
use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::Mutex;

pub struct ProcessJailRuntime {
    children: Mutex<Vec<Child>>,
}

impl ProcessJailRuntime {
    pub fn new() -> Self {
        Self { children: Mutex::new(Vec::new()) }
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, JailError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| JailError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), JailError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(JailError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn reap_finished_children(&self) {
        let mut children = self.children.lock().unwrap();
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
    }
}

impl Default for ProcessJailRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl JailRuntime for ProcessJailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError> {
        let path_arg = format!("path={}", rootfs.display());
        Self::run_checked("jail", &["-c", &format!("name={name}"), &path_arg, "persist"])
    }

    fn destroy(&self, name: &str) -> Result<(), JailError> {
        Self::run_checked("jail", &["-r", name])
    }

    fn is_running(&self, name: &str) -> Result<bool, JailError> {
        let jls = Self::run("jls", &["-j", name, "jid"])?;
        if !jls.status.success() {
            return Ok(false);
        }
        let jid = String::from_utf8_lossy(&jls.stdout).trim().to_string();
        if jid.is_empty() {
            return Ok(false);
        }
        let ps = Self::run("ps", &["-J", &jid, "-o", "pid="])?;
        Ok(!String::from_utf8_lossy(&ps.stdout).trim().is_empty())
    }

    fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError> {
        self.reap_finished_children();
        let mut cmd = Command::new("jexec");
        cmd.arg(name);
        cmd.args(command);
        let child = cmd.spawn().map_err(|e| JailError::Spawn("jexec".to_string(), e))?;
        self.children.lock().unwrap().push(child);
        Ok(())
    }

    fn set_resource_limits(&self, _name: &str, _pcpu_percent: u32, _memory_bytes: u64) -> Result<(), JailError> {
        unimplemented!("added in Task 5")
    }

    fn remove_resource_limits(&self, _name: &str) -> Result<(), JailError> {
        unimplemented!("added in Task 5")
    }
}
```

- [ ] **Step 2: Extend the FreeBSD integration test**

`start_command` needs something executable inside the jail's rootfs.
FreeBSD ships statically-linked binaries under `/rescue` specifically for
this kind of minimal-environment use — copy `/rescue/sh` in as the jail's
`/bin/sh` so no shared-library dependencies are needed.

Modify `kubsd-jail/tests/freebsd_lifecycle.rs` — replace the whole file:

```rust
#![cfg(target_os = "freebsd")]

use kubsd_jail::{JailRuntime, ProcessJailRuntime};
use std::path::Path;
use std::{thread, time::Duration};

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-jail --test freebsd_lifecycle`
// (jail(8)/jls(8)/jexec(8) require root privileges).

#[test]
fn create_destroy_and_is_running_lifecycle() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-lifecycle";
    let rootfs = Path::new("/tmp/kubsd-test-lifecycle-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    let _ = runtime.destroy(name);

    runtime.create(name, rootfs).expect("create should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "no command started yet");

    runtime.destroy(name).expect("destroy should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "destroyed jail is not running");
}

#[test]
fn start_command_makes_is_running_true() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-start-command";
    let rootfs = Path::new("/tmp/kubsd-test-start-command-rootfs");
    let bin_dir = rootfs.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::copy("/rescue/sh", bin_dir.join("sh")).expect("copy /rescue/sh into test rootfs");

    let _ = runtime.destroy(name);
    runtime.create(name, rootfs).expect("create should succeed");

    runtime
        .start_command(name, &["/bin/sh".to_string(), "-c".to_string(), "sleep 30".to_string()])
        .expect("start_command should succeed");

    // Give jexec a moment to actually fork/exec before checking.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(runtime.is_running(name).unwrap(), true, "sleep 30 should still be running");

    runtime.destroy(name).expect("destroy should succeed");
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 22 tests total (unchanged — this test file is
FreeBSD-only).

- [ ] **Step 4: Commit and push**

```bash
git add kubsd-jail/src/process.rs kubsd-jail/tests/freebsd_lifecycle.rs
git commit -m "Add ProcessJailRuntime start_command with best-effort child reaping"
git push origin master:main
```

- [ ] **Step 5: Run the real integration tests on the VM**

Run: `! ssh root@192.168.64.2 'cd ~/kubsd && git pull && cargo test -p kubsd-jail --test freebsd_lifecycle 2>&1 | tail -20'`

Expected: both `create_destroy_and_is_running_lifecycle` and
`start_command_makes_is_running_true` pass. If `/rescue/sh` fails to run
inside the jail (e.g. a missing-device error), it likely needs `/dev`
available inside the jail rootfs — if so, mount devfs first:
`mkdir -p /tmp/kubsd-test-start-command-rootfs/dev && mount -t devfs devfs /tmp/kubsd-test-start-command-rootfs/dev`
before re-running, and unmount it after
(`umount /tmp/kubsd-test-start-command-rootfs/dev`). Report back if this
extra step is needed so it can be folded into the test setup.

---

### Task 5: ProcessJailRuntime — resource limits via rctl

**Files:**
- Modify: `kubsd-jail/src/process.rs`
- Modify: `kubsd-jail/tests/freebsd_lifecycle.rs`

**Interfaces:**
- Consumes: `JailError`, `JailRuntime`, `ProcessJailRuntime` (Tasks 3-4).
- Produces: working `set_resource_limits`/`remove_resource_limits` on
  `ProcessJailRuntime`. This completes the `JailRuntime` trait — no method
  is left `unimplemented!()` after this task.

- [ ] **Step 1: Implement the two methods**

In `kubsd-jail/src/process.rs`, replace the two `unimplemented!()` method
bodies:

```rust
    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError> {
        Self::run_checked("rctl", &["-a", &format!("jail:{name}:pcpu:deny={pcpu_percent}")])?;
        Self::run_checked("rctl", &["-a", &format!("jail:{name}:vmemoryuse:deny={memory_bytes}")])
    }

    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError> {
        Self::run_checked("rctl", &["-r", &format!("jail:{name}")])
    }
```

- [ ] **Step 2: Extend the FreeBSD integration test**

Add to `kubsd-jail/tests/freebsd_lifecycle.rs`:

```rust
#[test]
fn set_and_remove_resource_limits() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-resource-limits";
    let rootfs = Path::new("/tmp/kubsd-test-resource-limits-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    let _ = runtime.remove_resource_limits(name);
    let _ = runtime.destroy(name);
    runtime.create(name, rootfs).expect("create should succeed");

    runtime.set_resource_limits(name, 200, 512 * 1024 * 1024).expect("set_resource_limits should succeed");

    let output = std::process::Command::new("rctl")
        .arg(format!("jail:{name}"))
        .output()
        .expect("rctl should run");
    let rules = String::from_utf8_lossy(&output.stdout);
    assert!(rules.contains("pcpu:deny=200"), "expected pcpu rule in: {rules}");
    assert!(rules.contains("vmemoryuse:deny=536870912"), "expected vmemoryuse rule in: {rules}");

    runtime.remove_resource_limits(name).expect("remove_resource_limits should succeed");
    let output = std::process::Command::new("rctl")
        .arg(format!("jail:{name}"))
        .output()
        .expect("rctl should run");
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty(), "rules should be gone after removal");

    runtime.destroy(name).expect("destroy should succeed");
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 22 tests total (unchanged).

- [ ] **Step 4: Commit and push**

```bash
git add kubsd-jail/src/process.rs kubsd-jail/tests/freebsd_lifecycle.rs
git commit -m "Add ProcessJailRuntime resource limits via rctl"
git push origin master:main
```

- [ ] **Step 5: Run the real integration tests on the VM**

Run: `! ssh root@192.168.64.2 'cd ~/kubsd && git pull && cargo test -p kubsd-jail --test freebsd_lifecycle 2>&1 | tail -20'`

Expected: all three tests in `freebsd_lifecycle.rs` pass, including the
new `set_and_remove_resource_limits`. This completes `kubsd-jail`.

---

### Task 6: kubsd-zfs scaffold — ZfsError, ZfsManager trait, FakeZfsManager

**Files:**
- Create: `kubsd-zfs/Cargo.toml`
- Create: `kubsd-zfs/src/error.rs`
- Create: `kubsd-zfs/src/lib.rs`
- Create: `kubsd-zfs/src/fake.rs`
- Modify: `Cargo.toml` (workspace root — add `kubsd-zfs` to `members`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub enum ZfsError` (variants: `Spawn(String, std::io::Error)`,
  `CommandFailed(String, std::process::ExitStatus, String)`,
  `NotFound(String)`); `pub trait ZfsManager` with the three methods
  below; `pub struct FakeZfsManager` implementing it. Task 7-8 implement
  `CliZfsManager` against this same trait.

- [ ] **Step 1: Add kubsd-zfs to the workspace**

Modify `Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = ["kubsd-spec", "kubsd-jail", "kubsd-zfs"]
```

- [ ] **Step 2: Create the crate manifest**

Create `kubsd-zfs/Cargo.toml`:

```toml
[package]
name = "kubsd-zfs"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"
```

- [ ] **Step 3: Write the error type**

Create `kubsd-zfs/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ZfsError {
    #[error("failed to spawn `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed with exit status {1}: {2}")]
    CommandFailed(String, std::process::ExitStatus, String),
    #[error("dataset '{0}' not found")]
    NotFound(String),
}
```

- [ ] **Step 4: Write the trait and FakeZfsManager with tests**

Create `kubsd-zfs/src/fake.rs`:

```rust
use crate::ZfsError;
use crate::ZfsManager;
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeZfsManager {
    datasets: Mutex<HashSet<String>>,
}

impl FakeZfsManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: seed a base dataset as if it already existed on the pool.
    pub fn seed_dataset(&self, dataset: &str) {
        self.datasets.lock().unwrap().insert(dataset.to_string());
    }
}

impl ZfsManager for FakeZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError> {
        Ok(self.datasets.lock().unwrap().contains(dataset))
    }

    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError> {
        let datasets = self.datasets.lock().unwrap();
        if !datasets.contains(base_dataset) {
            return Err(ZfsError::NotFound(base_dataset.to_string()));
        }
        drop(datasets);
        self.datasets.lock().unwrap().insert(target_dataset.to_string());
        Ok(())
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        if self.datasets.lock().unwrap().remove(dataset) {
            Ok(())
        } else {
            Err(ZfsError::NotFound(dataset.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_exists_is_false_until_seeded() {
        let zfs = FakeZfsManager::new();
        assert_eq!(zfs.dataset_exists("zroot/kubsd/base/test").unwrap(), false);
        zfs.seed_dataset("zroot/kubsd/base/test");
        assert_eq!(zfs.dataset_exists("zroot/kubsd/base/test").unwrap(), true);
    }

    #[test]
    fn clone_from_base_requires_existing_base() {
        let zfs = FakeZfsManager::new();
        assert!(matches!(
            zfs.clone_from_base("zroot/kubsd/base/test", "zroot/kubsd/jails/web-1"),
            Err(ZfsError::NotFound(_))
        ));
    }

    #[test]
    fn clone_from_base_creates_target_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/kubsd/base/test");
        zfs.clone_from_base("zroot/kubsd/base/test", "zroot/kubsd/jails/web-1").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/kubsd/jails/web-1").unwrap(), true);
    }

    #[test]
    fn destroy_dataset_removes_it() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/kubsd/jails/web-1");
        zfs.destroy_dataset("zroot/kubsd/jails/web-1").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/kubsd/jails/web-1").unwrap(), false);
    }

    #[test]
    fn destroy_dataset_on_unknown_dataset_returns_not_found() {
        let zfs = FakeZfsManager::new();
        assert!(matches!(zfs.destroy_dataset("zroot/kubsd/jails/missing"), Err(ZfsError::NotFound(_))));
    }
}
```

Create `kubsd-zfs/src/lib.rs`:

```rust
pub mod error;
pub mod fake;

pub use error::ZfsError;
pub use fake::FakeZfsManager;

pub trait ZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError>;
    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError>;
    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError>;
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace -p kubsd-zfs`

Expected: PASS, 5 tests (`dataset_exists_is_false_until_seeded`,
`clone_from_base_requires_existing_base`,
`clone_from_base_creates_target_dataset`, `destroy_dataset_removes_it`,
`destroy_dataset_on_unknown_dataset_returns_not_found`).

- [ ] **Step 6: Run the full workspace suite**

Run: `cargo test --workspace`

Expected: PASS, 27 tests total (22 from Tasks 1-5 + 5 new).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml kubsd-zfs/Cargo.toml kubsd-zfs/src/error.rs kubsd-zfs/src/lib.rs kubsd-zfs/src/fake.rs
git commit -m "Add kubsd-zfs crate: ZfsManager trait and FakeZfsManager"
```

---

### Task 7: CliZfsManager — dataset_exists, destroy_dataset

**Files:**
- Create: `kubsd-zfs/src/cli.rs`
- Modify: `kubsd-zfs/src/lib.rs`
- Create: `kubsd-zfs/tests/freebsd_zfs.rs`

**Interfaces:**
- Consumes: `ZfsError`, `ZfsManager` (Task 6), the `zroot/kubsd/base/test`
  dataset created in Task 1.
- Produces: `pub struct CliZfsManager` implementing `dataset_exists` and
  `destroy_dataset` for real (`clone_from_base` is added in Task 8 and can
  `unimplemented!()` until then).

- [ ] **Step 1: Write the implementation**

Create `kubsd-zfs/src/cli.rs`:

```rust
use crate::ZfsError;
use crate::ZfsManager;
use std::process::{Command, Output};

pub struct CliZfsManager;

impl CliZfsManager {
    pub fn new() -> Self {
        Self
    }

    fn run(args: &[&str]) -> Result<Output, ZfsError> {
        Command::new("zfs")
            .args(args)
            .output()
            .map_err(|e| ZfsError::Spawn("zfs".to_string(), e))
    }

    fn run_checked(args: &[&str]) -> Result<(), ZfsError> {
        let output = Self::run(args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ZfsError::CommandFailed(
                format!("zfs {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

impl Default for CliZfsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ZfsManager for CliZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError> {
        let output = Self::run(&["list", "-H", "-o", "name", dataset])?;
        Ok(output.status.success())
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        Self::run_checked(&["destroy", dataset])
    }

    fn clone_from_base(&self, _base_dataset: &str, _target_dataset: &str) -> Result<(), ZfsError> {
        unimplemented!("added in Task 8")
    }
}
```

Modify `kubsd-zfs/src/lib.rs` — add `pub mod cli;` and `pub use
cli::CliZfsManager;`:

```rust
pub mod cli;
pub mod error;
pub mod fake;

pub use cli::CliZfsManager;
pub use error::ZfsError;
pub use fake::FakeZfsManager;

pub trait ZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError>;
    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError>;
    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError>;
}
```

- [ ] **Step 2: Write the FreeBSD-only integration test**

Create `kubsd-zfs/tests/freebsd_zfs.rs`:

```rust
#![cfg(target_os = "freebsd")]

use kubsd_zfs::{CliZfsManager, ZfsManager};

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-zfs --test freebsd_zfs`
// Requires zroot/kubsd/base/test to already exist (created in Milestone 2 Task 1).

#[test]
fn dataset_exists_reports_true_for_the_test_base_and_false_for_garbage() {
    let zfs = CliZfsManager::new();
    assert_eq!(zfs.dataset_exists("zroot/kubsd/base/test").unwrap(), true);
    assert_eq!(zfs.dataset_exists("zroot/kubsd/does-not-exist").unwrap(), false);
}

#[test]
fn destroy_dataset_removes_a_dataset_created_for_the_test() {
    let zfs = CliZfsManager::new();
    let scratch = "zroot/kubsd/jails/destroy-test-scratch";
    let _ = std::process::Command::new("zfs").args(["destroy", scratch]).output();
    std::process::Command::new("zfs")
        .args(["create", scratch])
        .output()
        .expect("zfs create should run");

    assert_eq!(zfs.dataset_exists(scratch).unwrap(), true);
    zfs.destroy_dataset(scratch).expect("destroy_dataset should succeed");
    assert_eq!(zfs.dataset_exists(scratch).unwrap(), false);
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 27 tests total (unchanged — this test file is
FreeBSD-only).

- [ ] **Step 4: Commit and push**

```bash
git add kubsd-zfs/src/cli.rs kubsd-zfs/src/lib.rs kubsd-zfs/tests/freebsd_zfs.rs
git commit -m "Add CliZfsManager dataset_exists and destroy_dataset"
git push origin master:main
```

- [ ] **Step 5: Run the real integration tests on the VM**

Run: `! ssh root@192.168.64.2 'cd ~/kubsd && git pull && cargo test -p kubsd-zfs --test freebsd_zfs 2>&1 | tail -15'`

Expected: both `dataset_exists_reports_true_for_the_test_base_and_false_for_garbage`
and `destroy_dataset_removes_a_dataset_created_for_the_test` pass.

---

### Task 8: CliZfsManager — clone_from_base

**Files:**
- Modify: `kubsd-zfs/src/cli.rs`
- Modify: `kubsd-zfs/tests/freebsd_zfs.rs`

**Interfaces:**
- Consumes: `ZfsError`, `ZfsManager`, `CliZfsManager` (Tasks 6-7).
- Produces: a working `clone_from_base` on `CliZfsManager`. This completes
  the `ZfsManager` trait and Milestone 2.

- [ ] **Step 1: Implement clone_from_base**

In `kubsd-zfs/src/cli.rs`, replace the `unimplemented!()` method body:

```rust
    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError> {
        let snapshot = format!("{base_dataset}@kubsd");
        if !self.dataset_exists(&snapshot)? {
            Self::run_checked(&["snapshot", &snapshot])?;
        }
        Self::run_checked(&["clone", &snapshot, target_dataset])
    }
```

- [ ] **Step 2: Extend the FreeBSD integration test**

Add to `kubsd-zfs/tests/freebsd_zfs.rs`:

```rust
#[test]
fn clone_from_base_creates_a_usable_clone() {
    let zfs = CliZfsManager::new();
    let target = "zroot/kubsd/jails/clone-test-scratch";
    let _ = zfs.destroy_dataset(target);

    zfs.clone_from_base("zroot/kubsd/base/test", target).expect("clone_from_base should succeed");
    assert_eq!(zfs.dataset_exists(target).unwrap(), true);

    zfs.destroy_dataset(target).expect("cleanup destroy should succeed");
}

#[test]
fn clone_from_base_reuses_existing_snapshot_on_second_call() {
    let zfs = CliZfsManager::new();
    let target_a = "zroot/kubsd/jails/clone-test-scratch-a";
    let target_b = "zroot/kubsd/jails/clone-test-scratch-b";
    let _ = zfs.destroy_dataset(target_a);
    let _ = zfs.destroy_dataset(target_b);

    zfs.clone_from_base("zroot/kubsd/base/test", target_a).expect("first clone should succeed");
    zfs.clone_from_base("zroot/kubsd/base/test", target_b).expect("second clone should succeed and reuse the snapshot");

    zfs.destroy_dataset(target_a).expect("cleanup a should succeed");
    zfs.destroy_dataset(target_b).expect("cleanup b should succeed");
}
```

- [ ] **Step 3: Run macOS-side checks**

Run: `cargo build --workspace && cargo test --workspace`

Expected: builds clean, 27 tests total (unchanged).

- [ ] **Step 4: Commit and push**

```bash
git add kubsd-zfs/src/cli.rs kubsd-zfs/tests/freebsd_zfs.rs
git commit -m "Add CliZfsManager clone_from_base with snapshot-on-demand"
git push origin master:main
```

- [ ] **Step 5: Run the real integration tests on the VM**

Run: `! ssh root@192.168.64.2 'cd ~/kubsd && git pull && cargo test -p kubsd-zfs --test freebsd_zfs 2>&1 | tail -20'`

Expected: all four tests in `freebsd_zfs.rs` pass, including the two new
ones. This completes `kubsd-zfs` and Milestone 2.

## Milestone Exit Criteria

- `cargo test --workspace` passes with 27 tests on macOS (17 from
  Milestone 1 + 5 `kubsd-jail` unit tests + 5 `kubsd-zfs` unit tests), with
  zero FreeBSD dependencies required to run them.
- On the FreeBSD VM: `cargo test -p kubsd-jail --test freebsd_lifecycle`
  (3 tests) and `cargo test -p kubsd-zfs --test freebsd_zfs` (4 tests)
  both pass as root.
- `kubsd-jail::JailRuntime` and `kubsd-zfs::ZfsManager` are complete,
  fully-implemented traits (no `unimplemented!()` left) ready for
  `kubsd-agentd`'s reconciliation loop (a later milestone) to compose
  against fakes in its own unit tests.
