# kubsd-agent Milestone 1: FreeBSD Env Prep + kubsd-spec Crate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 1:** it requires SSH access to a FreeBSD VM that is only
> reachable from the coordinating session's relay to the user (see Task 1 for
> details) — it is not runnable by an isolated subagent and must be executed by
> whichever session is talking to the user directly, regardless of which
> execution mode is chosen for the rest of this plan.

**Goal:** Stand up a FreeBSD dev VM ready for jail/ZFS/network work, and build
`kubsd-spec` — the pure-Rust crate that parses and validates the kubsd jail
YAML spec — as the first working, fully-tested piece of kubsd-agent.

**Architecture:** A Cargo workspace at the repo root with one crate for now
(`kubsd-spec`). The crate defines the `JailSpec` data model via `serde`,
parses YAML into it, and validates everything the type system can't already
guarantee: name format, CIDR well-formedness, resource string syntax, and the
immutable-field rule on re-apply. It has zero FreeBSD-specific code and
builds/tests entirely on macOS.

**Tech Stack:** Rust (2021 edition), `serde` + `serde_yaml` for
(de)serialization, `ipnet` for CIDR parsing, `thiserror` for the error type.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-05-kubsd-agent-design.md` (Approved). Every rule below traces back to it.
- Target FreeBSD VM for all real (non-mocked) work: `root@192.168.64.2`, FreeBSD 15.1-RELEASE-p1, ZFS pool `zroot`.
- Jail naming prefix: agent-managed jails are named `kubsd-<jail-name>` (not needed by this milestone, but the name-format validation added here must not conflict with it).
- `kubsd-spec` has no FreeBSD-specific code and must build and test on any OS.
- No placeholders: every validation rule implemented here has a passing and a failing test.

---

### Task 1: Prepare the FreeBSD dev VM

**Human-in-the-loop task.** The coordinating assistant's shell cannot reach
`192.168.64.2` directly (confirmed: ping/SSH from the assistant's Bash tool
time out even though the user's own terminal reaches it fine — likely because
UTM's shared-network vmnet interface isn't visible to the assistant's shell
process). Every command below must be run by the user, who pastes the output
back into the conversation, or the user can type `! <command>` in the Claude
Code prompt so the output lands directly in the transcript.

**Files:** None (infrastructure only).

**Interfaces:**
- Produces: a FreeBSD VM at `root@192.168.64.2` with `if_bridge`/`if_epair`
  kernel modules loaded, `kern.racct.enable=1` active, and `git`/`rustc`/
  `cargo` installed — the environment all later kubsd-agent milestones
  (`kubsd-jail`, `kubsd-zfs`, `kubsd-net`) will build and run against.

- [ ] **Step 1: Confirm baseline VM state**

Run: `! ssh root@192.168.64.2 'sysctl kern.features.vimage; freebsd-version; zpool status zroot; kldstat'`

Expected: `kern.features.vimage: 1`, a FreeBSD 15.x version string, `zroot`
pool `state: ONLINE`, and a `kldstat` listing without `if_bridge.ko` or
`if_epair.ko` yet. (Already confirmed in this session — re-run only if time
has passed and you want to double check nothing changed.)

- [ ] **Step 2: Enable required kernel modules and RACCT/RCTL at boot**

Run:
```
! ssh root@192.168.64.2 "printf 'if_bridge_load=\"YES\"\nif_epair_load=\"YES\"\nkern.racct.enable=\"1\"\n' >> /boot/loader.conf && cat /boot/loader.conf"
```

Expected output: the file now ends with the three lines:
```
if_bridge_load="YES"
if_epair_load="YES"
kern.racct.enable="1"
```

- [ ] **Step 3: Reboot the VM to apply the RACCT tunable**

`kern.racct.enable` only takes effect at boot (it cannot be changed with
`sysctl -w` at runtime), so a reboot is required here even though the two
`_load` modules could be `kldload`-ed live.

Run: `! ssh root@192.168.64.2 reboot`

This will drop the SSH connection immediately (expected). Wait about 30
seconds for the VM to come back up.

- [ ] **Step 4: Verify the reboot applied everything**

Run: `! ssh root@192.168.64.2 'sysctl kern.racct.enable; kldstat | grep -E "if_bridge|if_epair"'`

Expected: `kern.racct.enable: 1`, and two `kldstat` lines showing
`if_bridge.ko` and `if_epair.ko` loaded.

- [ ] **Step 5: Install git and the Rust toolchain**

Run:
```
! ssh root@192.168.64.2 'pkg install -y git curl && curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
```

Expected: `pkg` install succeeds, then the rustup installer runs and prints
`Rust is installed now. Great!` at the end.

- [ ] **Step 6: Verify the toolchain**

Run: `! ssh root@192.168.64.2 'source ~/.cargo/env && rustc --version && cargo --version && git --version'`

Expected: version strings print for all three with no errors. This is the
task's pass/fail check — if any of the three commands error, Task 1 is not
done.

- [ ] **Step 7: No commit needed**

This task changes only the remote VM, not this repository — there is
nothing to commit here.

---

### Task 2: Cargo workspace + kubsd-spec crate scaffold

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `kubsd-spec/Cargo.toml`
- Create: `kubsd-spec/src/lib.rs`

**Interfaces:**
- Consumes: nothing (first crate in the workspace).
- Produces: a `kubsd-spec` library crate that builds and has one passing
  test, ready for Task 3 to add real types to.

- [ ] **Step 1: Create the workspace root manifest**

Create `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["kubsd-spec"]
```

- [ ] **Step 2: Create the kubsd-spec crate manifest**

Create `kubsd-spec/Cargo.toml`:

```toml
[package]
name = "kubsd-spec"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
ipnet = "2"
thiserror = "1"
```

- [ ] **Step 3: Create a minimal lib.rs with one placeholder test**

Create `kubsd-spec/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds_and_tests_run() {
        assert_eq!(2 + 2, 4);
    }
}
```

- [ ] **Step 4: Run the test to verify the workspace is wired correctly**

Run: `cargo test --workspace`

Expected: PASS — `test tests::crate_builds_and_tests_run ... ok`, and
dependency resolution succeeds for `serde`, `serde_yaml`, `ipnet`, and
`thiserror` even though nothing uses them yet (they'll be used starting
Task 3).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml kubsd-spec/Cargo.toml kubsd-spec/src/lib.rs
git commit -m "Scaffold kubsd-spec crate in a new Cargo workspace"
```

---

### Task 3: Core spec types and YAML round-trip

**Files:**
- Create: `kubsd-spec/src/types.rs`
- Modify: `kubsd-spec/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct JailSpec { api_version, kind, metadata, spec }`,
  `pub struct Metadata { name: String }`, `pub struct Spec { image: String,
  command: Vec<String>, network: NetworkSpec, resources: ResourcesSpec,
  restart_policy: RestartPolicy }`, `pub struct NetworkSpec { vnet: bool,
  bridge: String, address: String }`, `pub struct ResourcesSpec { cpu:
  String, memory: String }`, `pub enum RestartPolicy { Always, OnFailure,
  Never }`. All derive `Debug, Clone, PartialEq, Serialize, Deserialize`.
  These are the exact names every later task (3 onward) and every future
  milestone's crates build on.

- [ ] **Step 1: Write the failing test**

Add to `kubsd-spec/src/types.rs` (new file):

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailSpec {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub image: String,
    pub command: Vec<String>,
    pub network: NetworkSpec,
    pub resources: ResourcesSpec,
    #[serde(rename = "restartPolicy")]
    pub restart_policy: RestartPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkSpec {
    pub vnet: bool,
    pub bridge: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourcesSpec {
    pub cpu: String,
    pub memory: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_YAML: &str = r#"
apiVersion: kubsd/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: kubsd0
    address: 10.0.0.5/24
  resources:
    cpu: "2"
    memory: "512M"
  restartPolicy: Always
"#;

    #[test]
    fn parses_the_design_spec_example_yaml() {
        let spec: JailSpec = serde_yaml::from_str(EXAMPLE_YAML).unwrap();
        assert_eq!(spec.api_version, "kubsd/v1");
        assert_eq!(spec.kind, "Jail");
        assert_eq!(spec.metadata.name, "web-1");
        assert_eq!(spec.spec.image, "base/14.2-web");
        assert_eq!(spec.spec.command, vec!["/usr/local/bin/myapp".to_string()]);
        assert!(spec.spec.network.vnet);
        assert_eq!(spec.spec.network.bridge, "kubsd0");
        assert_eq!(spec.spec.network.address, "10.0.0.5/24");
        assert_eq!(spec.spec.resources.cpu, "2");
        assert_eq!(spec.spec.resources.memory, "512M");
        assert_eq!(spec.spec.restart_policy, RestartPolicy::Always);
    }
}
```

Add to `kubsd-spec/src/lib.rs` (replacing its contents):

```rust
pub mod types;

pub use types::{JailSpec, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
```

- [ ] **Step 2: Run the test to verify it currently fails**

Run: `cargo test --workspace parses_the_design_spec_example_yaml`

Expected: this actually PASSES immediately since we wrote the types and the
test in the same step (there's no meaningful red state for a pure data
struct + derive macro — the "test" here is really a round-trip
verification). Confirm it prints `test types::tests::parses_the_design_spec_example_yaml ... ok`.

- [ ] **Step 3: (No separate implementation step — types and test were written together in Step 1.)**

- [ ] **Step 4: Run the full test suite to make sure nothing else broke**

Run: `cargo test --workspace`

Expected: PASS, 2 tests total (the Task 2 placeholder test plus this one).

- [ ] **Step 5: Commit**

```bash
git add kubsd-spec/src/types.rs kubsd-spec/src/lib.rs
git commit -m "Add JailSpec data model with YAML round-trip test"
```

---

### Task 4: SpecError type and jail name validation

**Files:**
- Create: `kubsd-spec/src/error.rs`
- Create: `kubsd-spec/src/validate.rs`
- Modify: `kubsd-spec/src/lib.rs`

**Interfaces:**
- Consumes: `JailSpec`, `Metadata` from `types.rs` (Task 3).
- Produces: `pub enum SpecError` (variants: `Yaml(String)`,
  `InvalidName(String)`, `InvalidAddress(String, String)`,
  `InvalidCpu(String)`, `InvalidMemory(String)`,
  `ImmutableField(&'static str)`), and `pub fn validate_name(name: &str) ->
  Result<(), SpecError>`. Later tasks (5, 6, 7) extend `SpecError` usage and
  add to `validate.rs`; Task 8 wires everything into `parse_and_validate`.

Note on why there's no separate "required field" validation: every field on
`JailSpec` is non-`Option`, so `serde` already rejects YAML missing any of
them at parse time (Task 8's `SpecError::Yaml` variant surfaces that). This
task only adds validation for things the type system can't express, i.e.
the actual *content* of `name` being well-formed.

- [ ] **Step 1: Write the failing test**

Create `kubsd-spec/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum SpecError {
    #[error("failed to parse YAML: {0}")]
    Yaml(String),
    #[error("invalid jail name '{0}': must be 1-63 lowercase alphanumeric characters or hyphens, starting and ending with an alphanumeric character")]
    InvalidName(String),
    #[error("invalid network address '{0}': {1}")]
    InvalidAddress(String, String),
    #[error("invalid cpu value '{0}': must be a positive number of cores")]
    InvalidCpu(String),
    #[error("invalid memory value '{0}': expected a number optionally followed by K, M, or G")]
    InvalidMemory(String),
    #[error("field '{0}' cannot be changed after the jail is created; delete and re-apply instead")]
    ImmutableField(&'static str),
}
```

Create `kubsd-spec/src/validate.rs`:

```rust
use crate::error::SpecError;

pub fn validate_name(name: &str) -> Result<(), SpecError> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(SpecError::InvalidName(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_names() {
        assert!(validate_name("web-1").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn rejects_malformed_names() {
        assert_eq!(validate_name(""), Err(SpecError::InvalidName("".to_string())));
        assert_eq!(
            validate_name(&"a".repeat(64)),
            Err(SpecError::InvalidName("a".repeat(64)))
        );
        assert_eq!(
            validate_name("-leading-hyphen"),
            Err(SpecError::InvalidName("-leading-hyphen".to_string()))
        );
        assert_eq!(
            validate_name("trailing-hyphen-"),
            Err(SpecError::InvalidName("trailing-hyphen-".to_string()))
        );
        assert_eq!(
            validate_name("Has_Upper_And_Underscore"),
            Err(SpecError::InvalidName("Has_Upper_And_Underscore".to_string()))
        );
    }
}
```

Modify `kubsd-spec/src/lib.rs`:

```rust
pub mod error;
pub mod types;
pub mod validate;

pub use error::SpecError;
pub use types::{JailSpec, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
```

- [ ] **Step 2: Run the tests to verify they fail before `validate.rs` compiles correctly**

Run: `cargo test --workspace validate::tests`

Expected: since the implementation was written alongside the test, this
should PASS on first run. Confirm both `accepts_well_formed_names` and
`rejects_malformed_names` show `ok`.

- [ ] **Step 3: (No separate implementation step — see Step 1.)**

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace`

Expected: PASS, 4 tests total.

- [ ] **Step 5: Commit**

```bash
git add kubsd-spec/src/error.rs kubsd-spec/src/validate.rs kubsd-spec/src/lib.rs
git commit -m "Add SpecError type and jail name validation"
```

---

### Task 5: Network address (CIDR) validation

**Files:**
- Modify: `kubsd-spec/src/validate.rs`
- Modify: `kubsd-spec/Cargo.toml` (no change needed — `ipnet` was already added in Task 2)

**Interfaces:**
- Consumes: `SpecError` (Task 4), `NetworkSpec` (Task 3), `ipnet::IpNet`.
- Produces: `pub fn validate_address(address: &str) -> Result<(), SpecError>`.

- [ ] **Step 1: Write the failing test**

Add to `kubsd-spec/src/validate.rs`, inside the existing `mod tests` block:

```rust
    #[test]
    fn accepts_well_formed_cidr_addresses() {
        assert!(validate_address("10.0.0.5/24").is_ok());
        assert!(validate_address("192.168.1.1/32").is_ok());
    }

    #[test]
    fn rejects_malformed_addresses() {
        assert!(validate_address("not-an-address").is_err());
        assert!(validate_address("10.0.0.5").is_err()); // missing prefix length
        assert!(validate_address("10.0.0.5/33").is_err()); // prefix out of range
    }
```

And add the function itself above the `#[cfg(test)]` block:

```rust
use ipnet::IpNet;

pub fn validate_address(address: &str) -> Result<(), SpecError> {
    address
        .parse::<IpNet>()
        .map(|_| ())
        .map_err(|e| SpecError::InvalidAddress(address.to_string(), e.to_string()))
}
```

(Add the `use ipnet::IpNet;` line at the top of the file next to the
existing `use crate::error::SpecError;` line.)

- [ ] **Step 2: Run the new tests**

Run: `cargo test --workspace validate::tests::accepts_well_formed_cidr_addresses validate::tests::rejects_malformed_addresses`

Expected: PASS for both.

- [ ] **Step 3: (No separate implementation step — see Step 1.)**

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace`

Expected: PASS, 6 tests total.

- [ ] **Step 5: Commit**

```bash
git add kubsd-spec/src/validate.rs
git commit -m "Add CIDR validation for jail network address"
```

---

### Task 6: Resource string parsing (cpu cores, memory size)

**Files:**
- Create: `kubsd-spec/src/resources.rs`
- Modify: `kubsd-spec/src/lib.rs`

**Interfaces:**
- Consumes: `SpecError` (Task 4).
- Produces: `pub fn parse_cpu_cores(s: &str) -> Result<f64, SpecError>`,
  `pub fn cores_to_pcpu_percent(cores: f64) -> u32`, `pub fn
  parse_memory_bytes(s: &str) -> Result<u64, SpecError>`. Task 8's final
  integration test calls these directly; a later milestone (`kubsd-jail`)
  will call `cores_to_pcpu_percent` when building the actual `rctl` rule
  string.

- [ ] **Step 1: Write the failing test**

Create `kubsd-spec/src/resources.rs`:

```rust
use crate::error::SpecError;

pub fn parse_cpu_cores(s: &str) -> Result<f64, SpecError> {
    let cores: f64 = s.parse().map_err(|_| SpecError::InvalidCpu(s.to_string()))?;
    if cores > 0.0 && cores.is_finite() {
        Ok(cores)
    } else {
        Err(SpecError::InvalidCpu(s.to_string()))
    }
}

pub fn cores_to_pcpu_percent(cores: f64) -> u32 {
    (cores * 100.0).round() as u32
}

pub fn parse_memory_bytes(s: &str) -> Result<u64, SpecError> {
    let invalid = || SpecError::InvalidMemory(s.to_string());
    let upper = s.to_ascii_uppercase();
    let (num_part, multiplier): (&str, u64) = if let Some(n) = upper.strip_suffix('K') {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('M') {
        (n, 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix('G') {
        (n, 1024 * 1024 * 1024)
    } else {
        (upper.as_str(), 1)
    };
    let value: u64 = num_part.parse().map_err(|_| invalid())?;
    if value == 0 {
        return Err(invalid());
    }
    Ok(value * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_cpu_values() {
        assert_eq!(parse_cpu_cores("2"), Ok(2.0));
        assert_eq!(parse_cpu_cores("0.5"), Ok(0.5));
    }

    #[test]
    fn rejects_invalid_cpu_values() {
        assert_eq!(parse_cpu_cores("0"), Err(SpecError::InvalidCpu("0".to_string())));
        assert_eq!(parse_cpu_cores("-1"), Err(SpecError::InvalidCpu("-1".to_string())));
        assert_eq!(parse_cpu_cores("abc"), Err(SpecError::InvalidCpu("abc".to_string())));
    }

    #[test]
    fn converts_cores_to_pcpu_percent() {
        assert_eq!(cores_to_pcpu_percent(2.0), 200);
        assert_eq!(cores_to_pcpu_percent(0.5), 50);
    }

    #[test]
    fn parses_valid_memory_values() {
        assert_eq!(parse_memory_bytes("512M"), Ok(512 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("1G"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("2048K"), Ok(2048 * 1024));
        assert_eq!(parse_memory_bytes("100"), Ok(100));
    }

    #[test]
    fn rejects_invalid_memory_values() {
        assert!(parse_memory_bytes("0M").is_err());
        assert!(parse_memory_bytes("").is_err());
        assert!(parse_memory_bytes("abc").is_err());
        assert!(parse_memory_bytes("-5M").is_err());
    }
}
```

Modify `kubsd-spec/src/lib.rs`:

```rust
pub mod error;
pub mod resources;
pub mod types;
pub mod validate;

pub use error::SpecError;
pub use resources::{cores_to_pcpu_percent, parse_cpu_cores, parse_memory_bytes};
pub use types::{JailSpec, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test --workspace resources::tests`

Expected: PASS for all 5 tests (`parses_valid_cpu_values`,
`rejects_invalid_cpu_values`, `converts_cores_to_pcpu_percent`,
`parses_valid_memory_values`, `rejects_invalid_memory_values`).

- [ ] **Step 3: (No separate implementation step — see Step 1.)**

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace`

Expected: PASS, 11 tests total.

- [ ] **Step 5: Commit**

```bash
git add kubsd-spec/src/resources.rs kubsd-spec/src/lib.rs
git commit -m "Add cpu/memory resource string parsing"
```

---

### Task 7: Immutable-field transition validation

**Files:**
- Modify: `kubsd-spec/src/validate.rs`

**Interfaces:**
- Consumes: `JailSpec` (Task 3), `SpecError` (Task 4).
- Produces: `pub fn validate_transition(old: &JailSpec, new: &JailSpec) ->
  Result<(), SpecError>`. This is what `kubsd-agentd` (a later milestone)
  will call on re-`apply` of an existing jail name, per the design spec's
  "Mutating an applied spec" section: `image` and `network.address` are
  immutable after creation; `resources` and `restartPolicy` may change
  freely.

- [ ] **Step 1: Write the failing test**

Add to `kubsd-spec/src/validate.rs`, inside the existing `mod tests` block
(needs `crate::types::*` in scope — add `use crate::types::*;` to the
existing `use super::*;` line, i.e. change it to `use super::*;\n    use crate::types::*;`):

```rust
    fn sample_spec() -> JailSpec {
        JailSpec {
            api_version: "kubsd/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: "web-1".to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec {
                    vnet: true,
                    bridge: "kubsd0".to_string(),
                    address: "10.0.0.5/24".to_string(),
                },
                resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                restart_policy: RestartPolicy::Always,
            },
        }
    }

    #[test]
    fn allows_changing_resources_and_restart_policy() {
        let old = sample_spec();
        let mut new = sample_spec();
        new.spec.resources.cpu = "4".to_string();
        new.spec.restart_policy = RestartPolicy::Never;
        assert!(validate_transition(&old, &new).is_ok());
    }

    #[test]
    fn rejects_changing_image() {
        let old = sample_spec();
        let mut new = sample_spec();
        new.spec.image = "base/14.2-other".to_string();
        assert_eq!(
            validate_transition(&old, &new),
            Err(SpecError::ImmutableField("spec.image"))
        );
    }

    #[test]
    fn rejects_changing_network_address() {
        let old = sample_spec();
        let mut new = sample_spec();
        new.spec.network.address = "10.0.0.6/24".to_string();
        assert_eq!(
            validate_transition(&old, &new),
            Err(SpecError::ImmutableField("spec.network.address"))
        );
    }
```

Add the function itself, above `#[cfg(test)]`:

```rust
pub fn validate_transition(old: &crate::types::JailSpec, new: &crate::types::JailSpec) -> Result<(), SpecError> {
    if old.spec.image != new.spec.image {
        return Err(SpecError::ImmutableField("spec.image"));
    }
    if old.spec.network.address != new.spec.network.address {
        return Err(SpecError::ImmutableField("spec.network.address"));
    }
    Ok(())
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test --workspace validate::tests::allows_changing_resources_and_restart_policy validate::tests::rejects_changing_image validate::tests::rejects_changing_network_address`

Expected: PASS for all three.

- [ ] **Step 3: (No separate implementation step — see Step 1.)**

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace`

Expected: PASS, 14 tests total.

- [ ] **Step 5: Commit**

```bash
git add kubsd-spec/src/validate.rs
git commit -m "Add immutable-field validation for spec re-apply"
```

---

### Task 8: Public parse_and_validate API and end-to-end test

**Files:**
- Modify: `kubsd-spec/src/lib.rs`
- Create: `kubsd-spec/tests/parse_and_validate.rs`

**Interfaces:**
- Consumes: everything from Tasks 3-7.
- Produces: `pub fn parse_and_validate(yaml: &str) -> Result<JailSpec,
  SpecError>` — the single entry point every later milestone
  (`kubsd-agentd`'s API handler, `kubsdctl`) will call to turn a YAML file
  into a validated `JailSpec`.

- [ ] **Step 1: Write the failing test**

Create `kubsd-spec/tests/parse_and_validate.rs` (an integration test file —
it can only see `kubsd_spec`'s public API, which is the point: it proves
`parse_and_validate` is actually usable from outside the crate):

```rust
use kubsd_spec::{parse_and_validate, RestartPolicy, SpecError};

const VALID_YAML: &str = r#"
apiVersion: kubsd/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: kubsd0
    address: 10.0.0.5/24
  resources:
    cpu: "2"
    memory: "512M"
  restartPolicy: Always
"#;

#[test]
fn parses_and_validates_the_design_spec_example() {
    let spec = parse_and_validate(VALID_YAML).expect("valid spec should parse");
    assert_eq!(spec.metadata.name, "web-1");
    assert_eq!(spec.spec.restart_policy, RestartPolicy::Always);
}

#[test]
fn rejects_an_invalid_name() {
    let yaml = VALID_YAML.replace("name: web-1", "name: Invalid_Name");
    assert!(matches!(parse_and_validate(&yaml), Err(SpecError::InvalidName(_))));
}

#[test]
fn rejects_a_malformed_address() {
    let yaml = VALID_YAML.replace("address: 10.0.0.5/24", "address: not-an-address");
    assert!(matches!(parse_and_validate(&yaml), Err(SpecError::InvalidAddress(_, _))));
}

#[test]
fn rejects_missing_required_fields() {
    let yaml = "apiVersion: kubsd/v1\nkind: Jail\n"; // missing metadata and spec entirely
    assert!(matches!(parse_and_validate(yaml), Err(SpecError::Yaml(_))));
}
```

Add to `kubsd-spec/src/lib.rs` (final version of the file):

```rust
pub mod error;
pub mod resources;
pub mod types;
pub mod validate;

pub use error::SpecError;
pub use resources::{cores_to_pcpu_percent, parse_cpu_cores, parse_memory_bytes};
pub use types::{JailSpec, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
pub use validate::{validate_address, validate_name, validate_transition};

pub fn parse_and_validate(yaml: &str) -> Result<JailSpec, SpecError> {
    let spec: JailSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    validate::validate_address(&spec.spec.network.address)?;
    resources::parse_cpu_cores(&spec.spec.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.resources.memory)?;
    Ok(spec)
}
```

- [ ] **Step 2: Run the test to verify it fails before lib.rs exports `parse_and_validate`**

Temporarily this can't be run "before" since Step 1 wrote both the test and
the implementation together (same reasoning as Tasks 3-7 — there's no
meaningful red state for wiring already-tested pieces together). Instead,
run it now and confirm it's green:

Run: `cargo test --workspace --test parse_and_validate`

Expected: PASS for all 4 tests (`parses_and_validates_the_design_spec_example`,
`rejects_an_invalid_name`, `rejects_a_malformed_address`,
`rejects_missing_required_fields`).

- [ ] **Step 3: (No separate implementation step — see Step 1.)**

- [ ] **Step 4: Run the entire workspace test suite one final time**

Run: `cargo test --workspace`

Expected: PASS, 18 tests total (14 unit tests from Tasks 3-7 plus these 4
integration tests).

- [ ] **Step 5: Commit**

```bash
git add kubsd-spec/src/lib.rs kubsd-spec/tests/parse_and_validate.rs
git commit -m "Add parse_and_validate public API with end-to-end tests"
```

---

## Milestone Exit Criteria

- `cargo test --workspace` passes with 18 tests, entirely on macOS, no
  FreeBSD VM involved.
- The FreeBSD VM at `root@192.168.64.2` has `if_bridge`/`if_epair` loaded,
  `kern.racct.enable=1`, and a working `rustc`/`cargo`/`git` — ready for the
  next milestone (`kubsd-jail`, which will shell out to `jail(8)`/`jls(8)`/
  `rctl(8)` on that VM).
- `kubsd-spec::parse_and_validate` is the agreed public entry point that
  `kubsd-agentd`'s future HTTP API handler and `kubsdctl` will both call.
