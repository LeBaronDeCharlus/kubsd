# Milestone 15: Service Discovery via Replica Sets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a new `kind: Service` spec that produces `N` deterministically-named jail replicas (`<name>-0`..`<name>-{N-1}`) spread across nodes, auto-addressed within each target node's Milestone-14 `pod_cidr`, discoverable via `GET /services/<name>`, and self-healed (missing replicas rescheduled, excess replicas torn down) by piggybacking on the existing 5-second heartbeat traffic — with zero changes to `keel-agentd`'s own jail-hosting behavior.

**Architecture:** `keel-spec` gains `ServiceSpec`/`ServiceSpecBody`/`JailTemplate` types (a `Spec` minus `network.address`) plus `parse_and_validate_service`/`sniff_kind`. `keel-controlplane` gains a new `Services` registry (desired replica count + template, structurally parallel to `Placements`) and a new `addresses::UsedAddresses` map (next to `Placements`), reuses the *existing* `Placements` map for "where is replica X right now" via a deterministic `<service>-<index>` naming convention, and gains a same-service node-spreading wrapper around the *unchanged* `scheduler::pick_node`. `keel-agentd`'s existing 5-second heartbeat body gains one field (per-jail running status); `keel-controlplane`'s HTTP layer computes reconciliation actions (schedule missing / tear down excess replicas) once per incoming heartbeat and once per `Service` apply, executing them via the *same* resolve-then-forward-then-record pattern the control plane already uses for plain `kind: Jail` scheduling — no new thread, no new timer. `keelctl` sniffs `kind` before choosing `/jails/<name>` vs. `/services/<name>`, and falls back from a `404` on the former to the latter for `get`/`delete`.

**Tech Stack:** Rust (2021 edition), `ipnet` (CIDR arithmetic, `Ipv4Net::hosts()`), `serde`/`serde_yaml` (wire types), `rustls` (existing mTLS transport, unchanged), hand-rolled HTTP parsing (`httparse`) — no new dependencies; `keel-controlplane` gains its first-ever dependency on `keel-spec` (previously it forwarded raw `kind: Jail` YAML bytes without parsing them at all — `Service` scheduling requires the control plane to construct per-replica `JailSpec`s itself, so this coupling is unavoidable and new).

## Global Constraints

- `keel-agentd` gains **zero** behavioral changes beyond the heartbeat body's one new field — a replica is not a distinct concept to the node hosting it (Goal).
- No traffic load-balancing/proxying, no DNS server, no rolling updates, no control-plane persistence of `Service` definitions, no cross-service scheduling awareness, no anti-affinity guarantee stronger than "prefer," no IPv6, no new wire protocol beyond plain HTTP+YAML (Non-Goals — satisfied by absence, not by code, except where a task below says otherwise).
- `replicas: 0` is valid ("scaled to zero"), never an error.
- Changing `spec.template` on an existing `Service` is rejected with `409` (`SpecError`-shaped precedent); only `spec.replicas` may change.
- A name collision between a `Service`'s derived replica name and an existing jail owned by a *different* service, or an unmanaged `kind: Jail`, is rejected at apply time with `400`, naming the conflicting owner, before any scheduling.
- A `Service` that can't currently place all its desired replicas is never a hard failure — `apply` succeeds, places what it can, and reconciliation closes the gap on a later heartbeat tick.
- Design reference: `docs/superpowers/specs/2026-07-16-keel-agent-milestone15-service-discovery-design.md` (Approved). Follow it exactly; every place this plan makes an implementation decision the spec left open is called out inline with its rationale.

---

## Facts about the current codebase this plan relies on

Gathered by reading the actual current source (not assumed from an earlier milestone's shape — Milestone 11's shared-secret auth was fully superseded by Milestone 12's mTLS and does not exist anywhere in the current tree):

- `keel-controlplane` has **no dependency on `keel-spec` today**. `PUT /jails/<name>` (`handle_scheduled_apply`, `keel-controlplane/src/http.rs:161-186`) forwards the client's raw YAML bytes to the chosen node unparsed; the control plane has never needed to construct or inspect a `JailSpec` itself. This milestone is the first time it must.
- `Command::Heartbeat` (`keel-controlplane/src/worker.rs:27`) is exactly `Heartbeat(String, f64, u64, Sender<Result<(), UnknownNode>>)`. Its 4 call sites are: `worker.rs:52` (the match arm itself), `worker.rs:148`, `worker.rs:169`, `worker.rs:222` (its own tests), and `http.rs:281` (`handle_heartbeat`).
- `worker::spawn` (`keel-controlplane/src/worker.rs:36`) is `spawn(mut registry: Registry, mut placements: Placements) -> (JoinHandle<()>, Sender<Command>)`. Its 6 call sites across the workspace: `keel-controlplane/src/main.rs:87`, `keel-controlplane/src/http.rs:420,978,1020`, `keel-agentd/src/registration.rs:299`, `keelctl/tests/cli.rs:92`.
- `Placements` (`keel-controlplane/src/placements.rs`) is `{ by_jail: HashMap<String, String> }` with only `get`/`set`/`remove` — no iteration exposed today.
- `scheduler::NodeResources` (`keel-controlplane/src/scheduler.rs:7-13`) derives no traits at all today (not even `Clone`). `scheduler::pick_node(&[NodeResources]) -> Result<String, ScheduleError>` is a pure function this plan must not change the logic of.
- `Registry`'s internal `NodeRecord` (`keel-controlplane/src/registry.rs:10-18`) already stores a typed `pod_cidr: Ipv4Net`, but nothing outside the module can read it directly — `list()` only exposes it stringified on `NodeStatus.pod_cidr`.
- `keel-agentd/src/registration.rs`'s `heartbeat_once` (lines 107-121) builds the outbound heartbeat body by hand with `format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n")` — it does **not** use `keel_controlplane::wire::Heartbeat`'s `Serialize` impl today, even though that impl already exists and is exercised by `keel-controlplane`'s own `wire.rs` tests.
- `keel-net::bridge_gateway` (`keel-net/src/lib.rs:15-21`, used by `attach_jail`) always assigns network-plus-1 as a jail's bridge gateway. `ipnet::Ipv4Net::hosts()` already excludes the network and broadcast addresses, so `pod_cidr.hosts().skip(1)` is exactly "every address except network, network+1, and broadcast" — no manual arithmetic needed.
- `keelctl/src/main.rs`'s `dispatch`/`send_request`/`send_request_tcp`/`parse_response` today collapse every response into `Result<String, String>` — the HTTP status code is inspected only internally (`parse_response`) and then discarded. Implementing the `get`/`delete` 404-fallback requires threading the status code out to the caller (Task 9 below); done carelessly, this breaks the existing `apply_get_delete_round_trip` test's expectation that a deleted-then-re-queried jail's error still says "not found" — the fallback design in Task 9 accounts for this explicitly.

---

### Task 1: `keel-spec` — `ServiceSpec`/`ServiceSpecBody`/`JailTemplate` types, validation, and kind-sniffing

**Files:**
- Modify: `keel-spec/src/types.rs`
- Modify: `keel-spec/src/lib.rs`
- Test: same files' `#[cfg(test)]` modules

**Interfaces:**
- Produces: `keel_spec::ServiceSpec { api_version, kind, metadata: Metadata, spec: ServiceSpecBody }`; `ServiceSpecBody { replicas: u32, template: JailTemplate }`; `JailTemplate { image: String, command: Vec<String>, network: TemplateNetworkSpec, resources: ResourcesSpec, restart_policy: RestartPolicy }`; `TemplateNetworkSpec { vnet: bool, bridge: String }` (rejects an `address` field via `#[serde(deny_unknown_fields)]`); `JailTemplate::to_jail_spec(&self, name: &str, address: &str) -> JailSpec`; `keel_spec::parse_and_validate_service(yaml: &str) -> Result<ServiceSpec, SpecError>`; `keel_spec::sniff_kind(yaml: &str) -> Result<String, SpecError>`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-spec/src/types.rs`'s `#[cfg(test)] mod tests` (after the existing `parses_the_design_spec_example_yaml` test, before the closing `}` at line 84):

```rust
    const SERVICE_EXAMPLE_YAML: &str = r#"
apiVersion: keel/v1
kind: Service
metadata:
  name: web
spec:
  replicas: 3
  template:
    image: base/14.2-web
    command: ["/usr/local/bin/myapp"]
    network:
      vnet: true
      bridge: keel0
    resources:
      cpu: "1"
      memory: "256M"
    restartPolicy: Always
"#;

    #[test]
    fn parses_the_service_example_yaml() {
        let spec: ServiceSpec = serde_yaml::from_str(SERVICE_EXAMPLE_YAML).unwrap();
        assert_eq!(spec.api_version, "keel/v1");
        assert_eq!(spec.kind, "Service");
        assert_eq!(spec.metadata.name, "web");
        assert_eq!(spec.spec.replicas, 3);
        assert_eq!(spec.spec.template.image, "base/14.2-web");
        assert!(spec.spec.template.network.vnet);
        assert_eq!(spec.spec.template.network.bridge, "keel0");
        assert_eq!(spec.spec.template.resources.cpu, "1");
        assert_eq!(spec.spec.template.restart_policy, RestartPolicy::Always);
    }

    #[test]
    fn rejects_a_template_with_an_embedded_network_address() {
        let yaml = SERVICE_EXAMPLE_YAML.replace(
            "    network:\n      vnet: true\n      bridge: keel0\n",
            "    network:\n      vnet: true\n      bridge: keel0\n      address: 10.0.0.5/24\n",
        );
        assert!(
            serde_yaml::from_str::<ServiceSpec>(&yaml).is_err(),
            "template.network.address is not a valid field and must be rejected"
        );
    }

    #[test]
    fn to_jail_spec_builds_a_replica_spec_from_the_template_plus_name_and_address() {
        let service: ServiceSpec = serde_yaml::from_str(SERVICE_EXAMPLE_YAML).unwrap();
        let jail = service.spec.template.to_jail_spec("web-0", "10.0.60.2/24");
        assert_eq!(jail.api_version, "keel/v1");
        assert_eq!(jail.kind, "Jail");
        assert_eq!(jail.metadata.name, "web-0");
        assert_eq!(jail.spec.image, "base/14.2-web");
        assert_eq!(jail.spec.command, vec!["/usr/local/bin/myapp".to_string()]);
        assert!(jail.spec.network.vnet);
        assert_eq!(jail.spec.network.bridge, "keel0");
        assert_eq!(jail.spec.network.address, "10.0.60.2/24");
        assert_eq!(jail.spec.resources.cpu, "1");
        assert_eq!(jail.spec.restart_policy, RestartPolicy::Always);
    }
```

Add to `keel-spec/src/lib.rs` a new `#[cfg(test)] mod tests` block at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SERVICE_YAML: &str = r#"
apiVersion: keel/v1
kind: Service
metadata:
  name: web
spec:
  replicas: 2
  template:
    image: base/14.2-web
    command: ["/usr/local/bin/myapp"]
    network:
      vnet: true
      bridge: keel0
    resources:
      cpu: "1"
      memory: "256M"
    restartPolicy: Always
"#;

    #[test]
    fn parse_and_validate_service_accepts_a_well_formed_service() {
        let spec = parse_and_validate_service(VALID_SERVICE_YAML).unwrap();
        assert_eq!(spec.metadata.name, "web");
        assert_eq!(spec.spec.replicas, 2);
    }

    #[test]
    fn parse_and_validate_service_rejects_an_invalid_name() {
        let yaml = VALID_SERVICE_YAML.replace("name: web", "name: Invalid_Name");
        assert!(matches!(parse_and_validate_service(&yaml), Err(SpecError::InvalidName(_))));
    }

    #[test]
    fn parse_and_validate_service_rejects_invalid_resources() {
        let yaml = VALID_SERVICE_YAML.replace("cpu: \"1\"", "cpu: \"0\"");
        assert!(matches!(parse_and_validate_service(&yaml), Err(SpecError::InvalidCpu(_))));
    }

    #[test]
    fn sniff_kind_reads_jail() {
        let yaml = "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: web-1\n";
        assert_eq!(sniff_kind(yaml).unwrap(), "Jail");
    }

    #[test]
    fn sniff_kind_reads_service_without_needing_the_rest_of_the_document_to_parse_as_a_jail() {
        assert_eq!(sniff_kind(VALID_SERVICE_YAML).unwrap(), "Service");
    }

    #[test]
    fn sniff_kind_on_malformed_yaml_is_an_error() {
        assert!(sniff_kind("not: valid: yaml: [").is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keel-spec 2>&1 | tail -40`
Expected: FAIL to compile — `ServiceSpec`, `parse_and_validate_service`, `sniff_kind` not found.

- [ ] **Step 3: Add the types to `keel-spec/src/types.rs`**

Insert after `RestartPolicy`'s closing brace (line 45), before the `#[cfg(test)]` line:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ServiceSpecBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpecBody {
    pub replicas: u32,
    pub template: JailTemplate,
}

/// The same fields `kind: Jail`'s `Spec` has, minus `network.address` — a
/// replica's address is always auto-assigned (see `keel-controlplane`'s
/// `addresses` module), never given directly in a `Service`'s template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailTemplate {
    pub image: String,
    pub command: Vec<String>,
    pub network: TemplateNetworkSpec,
    pub resources: ResourcesSpec,
    #[serde(rename = "restartPolicy")]
    pub restart_policy: RestartPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateNetworkSpec {
    pub vnet: bool,
    pub bridge: String,
}

impl JailTemplate {
    /// Builds the concrete `JailSpec` for one replica: the template's fields
    /// plus the deterministic replica `name` and its auto-assigned `address`
    /// (already formatted as a CIDR string, e.g. `"10.0.60.2/24"`).
    pub fn to_jail_spec(&self, name: &str, address: &str) -> JailSpec {
        JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: self.image.clone(),
                command: self.command.clone(),
                network: NetworkSpec {
                    vnet: self.network.vnet,
                    bridge: self.network.bridge.clone(),
                    address: address.to_string(),
                },
                resources: self.resources.clone(),
                restart_policy: self.restart_policy,
            },
        }
    }
}
```

- [ ] **Step 4: Add `parse_and_validate_service`/`sniff_kind` and export the new types in `keel-spec/src/lib.rs`**

Replace the file's `pub use types::{...}` line and add the new function after `parse_and_validate`:

```rust
pub mod error;
pub mod resources;
pub mod types;
pub mod validate;

pub use error::SpecError;
pub use resources::{cores_to_pcpu_percent, parse_cpu_cores, parse_memory_bytes};
pub use types::{
    JailSpec, JailTemplate, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, ServiceSpec,
    ServiceSpecBody, Spec, TemplateNetworkSpec,
};
pub use validate::{validate_address, validate_name, validate_transition};

pub fn parse_and_validate(yaml: &str) -> Result<JailSpec, SpecError> {
    let spec: JailSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    validate::validate_address(&spec.spec.network.address)?;
    resources::parse_cpu_cores(&spec.spec.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.resources.memory)?;
    Ok(spec)
}

pub fn parse_and_validate_service(yaml: &str) -> Result<ServiceSpec, SpecError> {
    let spec: ServiceSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    resources::parse_cpu_cores(&spec.spec.template.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.template.resources.memory)?;
    Ok(spec)
}

/// Reads just the `kind` field out of a YAML document, without requiring the
/// rest of it to parse as any particular spec type — used by `keelctl` to
/// decide whether to parse the rest as a `JailSpec` or a `ServiceSpec`.
pub fn sniff_kind(yaml: &str) -> Result<String, SpecError> {
    #[derive(serde::Deserialize)]
    struct KindOnly {
        kind: String,
    }
    let sniff: KindOnly = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    Ok(sniff.kind)
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p keel-spec 2>&1 | tail -60`
Expected: PASS, all tests including the new ones.

- [ ] **Step 6: Commit**

```bash
git add keel-spec/src/types.rs keel-spec/src/lib.rs
git commit -m "Add ServiceSpec/JailTemplate types and kind-sniffing to keel-spec"
```

---

### Task 2: `keel-controlplane` — `Services` registry, replica naming, and ownership

**Files:**
- Modify: `keel-controlplane/Cargo.toml`
- Create: `keel-controlplane/src/services.rs`
- Modify: `keel-controlplane/src/placements.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Consumes: `keel_spec::JailTemplate` (Task 1).
- Produces: `services::ServiceRecord { pub desired_replicas: u32, pub template: keel_spec::JailTemplate }`; `services::Services::{new, get, names, list, apply, remove}`; `services::Owner` (`Unmanaged` | `Service(String)`, `Display`); `services::owner_of(name: &str, placements: &Placements, services: &Services) -> Option<Owner>`; `services::ApplyServiceError` (`NameConflict{name,owner}` / `TemplateChanged(String)`); `services::UnknownService(pub String)`; `services::replica_name(service_name: &str, index: u32) -> String`; `services::replica_index(service_name: &str, jail_name: &str) -> Option<u32>`; `services::diff_replicas(desired: u32, healthy_indices: &BTreeSet<u32>) -> (Vec<u32>, Vec<u32>)`; `Placements::iter(&self) -> impl Iterator<Item = (&str, &str)>`.

- [ ] **Step 1: Add the `keel-spec` dependency**

In `keel-controlplane/Cargo.toml`, add to `[dependencies]` (after `ipnet = "2"`):

```toml
keel-spec = { path = "../keel-spec" }
```

- [ ] **Step 2: Write the failing tests**

Create `keel-controlplane/src/services.rs`:

```rust
use crate::placements::Placements;
use keel_spec::JailTemplate;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceRecord {
    pub desired_replicas: u32,
    pub template: JailTemplate,
}

#[derive(Debug, Default)]
pub struct Services {
    by_name: HashMap<String, ServiceRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Unmanaged,
    Service(String),
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Owner::Unmanaged => write!(f, "an unmanaged jail"),
            Owner::Service(name) => write!(f, "service '{name}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyServiceError {
    #[error("name '{name}' is already in use by {owner}")]
    NameConflict { name: String, owner: Owner },
    #[error("service '{0}' template is immutable once created; delete and re-apply instead")]
    TemplateChanged(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown service '{0}'")]
pub struct UnknownService(pub String);

impl Services {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&ServiceRecord> {
        self.by_name.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(|s| s.as_str())
    }

    pub fn list(&self) -> Vec<(&str, &ServiceRecord)> {
        let mut entries: Vec<(&str, &ServiceRecord)> =
            self.by_name.iter().map(|(k, v)| (k.as_str(), v)).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        entries
    }

    /// Creates the service if `name` is new, or updates `desired_replicas`
    /// if it already exists and `template` is unchanged. Rejects a template
    /// change on an existing service (only `replicas` may change).
    pub fn apply(&mut self, name: String, desired_replicas: u32, template: JailTemplate) -> Result<(), ApplyServiceError> {
        if let Some(existing) = self.by_name.get(&name) {
            if existing.template != template {
                return Err(ApplyServiceError::TemplateChanged(name));
            }
        }
        self.by_name.insert(name, ServiceRecord { desired_replicas, template });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Option<ServiceRecord> {
        self.by_name.remove(name)
    }
}

/// `"<service_name>-<index>"`.
pub fn replica_name(service_name: &str, index: u32) -> String {
    format!("{service_name}-{index}")
}

/// If `jail_name` is `"<service_name>-<index>"` for a plain non-negative
/// integer index, returns that index.
pub fn replica_index(service_name: &str, jail_name: &str) -> Option<u32> {
    jail_name.strip_prefix(service_name)?.strip_prefix('-')?.parse::<u32>().ok()
}

/// Returns the current owner of a name already present in `placements`, or
/// `None` if it has no existing placement at all. A name belongs to a
/// service if it matches that service's deterministic replica pattern;
/// otherwise, if it's placed at all, it's an unmanaged plain `kind: Jail`.
pub fn owner_of(name: &str, placements: &Placements, services: &Services) -> Option<Owner> {
    if placements.get(name).is_none() {
        return None;
    }
    for service_name in services.names() {
        if replica_index(service_name, name).is_some() {
            return Some(Owner::Service(service_name.to_string()));
        }
    }
    Some(Owner::Unmanaged)
}

/// Given a service's desired replica count and the set of indices currently
/// healthy (Alive node + running jail, per the caller), returns the indices
/// to schedule (lowest missing first) and the indices to tear down (highest
/// healthy first), matching `diff_replicas(3, {}) == ([0,1,2], [])` and
/// `diff_replicas(1, {0,1,2}) == ([], [2,1])`.
pub fn diff_replicas(desired: u32, healthy_indices: &BTreeSet<u32>) -> (Vec<u32>, Vec<u32>) {
    let healthy_count = healthy_indices.len() as u32;
    if healthy_count < desired {
        let missing = desired - healthy_count;
        let to_add = (0u32..).filter(|i| !healthy_indices.contains(i)).take(missing as usize).collect();
        (to_add, Vec::new())
    } else if healthy_count > desired {
        let excess = healthy_count - desired;
        let to_remove = healthy_indices.iter().rev().take(excess as usize).copied().collect();
        (Vec::new(), to_remove)
    } else {
        (Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{ResourcesSpec, RestartPolicy, TemplateNetworkSpec};

    fn template() -> JailTemplate {
        JailTemplate {
            image: "base/14.2-web".to_string(),
            command: vec!["/usr/local/bin/myapp".to_string()],
            network: TemplateNetworkSpec { vnet: true, bridge: "keel0".to_string() },
            resources: ResourcesSpec { cpu: "1".to_string(), memory: "256M".to_string() },
            restart_policy: RestartPolicy::Always,
        }
    }

    #[test]
    fn apply_creates_a_new_service() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 3);
    }

    #[test]
    fn apply_again_with_the_same_template_scales_up_or_down() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        services.apply("web".to_string(), 5, template()).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 5);
        services.apply("web".to_string(), 0, template()).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 0);
    }

    #[test]
    fn apply_with_a_changed_template_is_rejected() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        let mut changed = template();
        changed.image = "base/different-image".to_string();
        assert_eq!(
            services.apply("web".to_string(), 3, changed),
            Err(ApplyServiceError::TemplateChanged("web".to_string()))
        );
    }

    #[test]
    fn remove_deletes_the_service() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        assert!(services.remove("web").is_some());
        assert!(services.get("web").is_none());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let mut services = Services::new();
        services.apply("web".to_string(), 1, template()).unwrap();
        services.apply("api".to_string(), 1, template()).unwrap();
        let names: Vec<&str> = services.list().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["api", "web"]);
    }

    #[test]
    fn replica_name_and_index_round_trip() {
        assert_eq!(replica_name("web", 2), "web-2");
        assert_eq!(replica_index("web", "web-2"), Some(2));
        assert_eq!(replica_index("web", "other-2"), None);
        assert_eq!(replica_index("web", "web-not-a-number"), None);
    }

    #[test]
    fn owner_of_an_unplaced_name_is_none() {
        let placements = Placements::new();
        let services = Services::new();
        assert_eq!(owner_of("web-0", &placements, &services), None);
    }

    #[test]
    fn owner_of_a_placed_name_matching_a_known_service_is_that_service() {
        let mut placements = Placements::new();
        placements.set("web-0".to_string(), "node-1".to_string());
        let mut services = Services::new();
        services.apply("web".to_string(), 1, template()).unwrap();
        assert_eq!(owner_of("web-0", &placements, &services), Some(Owner::Service("web".to_string())));
    }

    #[test]
    fn owner_of_a_placed_name_matching_no_service_is_unmanaged() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        let services = Services::new();
        assert_eq!(owner_of("web-1", &placements, &services), Some(Owner::Unmanaged));
    }

    #[test]
    fn diff_replicas_with_nothing_placed_adds_from_zero() {
        assert_eq!(diff_replicas(3, &BTreeSet::new()), (vec![0, 1, 2], vec![]));
    }

    #[test]
    fn diff_replicas_reuses_only_the_missing_index() {
        let healthy = BTreeSet::from([0, 1]);
        assert_eq!(diff_replicas(3, &healthy), (vec![2], vec![]));
    }

    #[test]
    fn diff_replicas_skips_an_index_that_is_unhealthy_and_reschedules_it() {
        let healthy = BTreeSet::from([1]);
        assert_eq!(diff_replicas(2, &healthy), (vec![0], vec![]));
    }

    #[test]
    fn diff_replicas_scales_down_from_the_highest_index() {
        let healthy = BTreeSet::from([0, 1, 2]);
        assert_eq!(diff_replicas(1, &healthy), (vec![], vec![2, 1]));
    }

    #[test]
    fn diff_replicas_scaled_to_zero_tears_down_everything() {
        let healthy = BTreeSet::from([0, 1, 2]);
        assert_eq!(diff_replicas(0, &healthy), (vec![], vec![2, 1, 0]));
    }

    #[test]
    fn diff_replicas_at_the_desired_count_does_nothing() {
        let healthy = BTreeSet::from([0, 1]);
        assert_eq!(diff_replicas(2, &healthy), (vec![], vec![]));
    }
}
```

Add to `keel-controlplane/src/placements.rs`'s `impl Placements` block (after `remove`, before the closing brace at line 24):

```rust
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_jail.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
```

And a test in its `#[cfg(test)] mod tests` (after `remove_clears_the_placement`):

```rust
    #[test]
    fn iter_yields_every_entry() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.set("web-2".to_string(), "node-2".to_string());
        let mut entries: Vec<(&str, &str)> = placements.iter().collect();
        entries.sort();
        assert_eq!(entries, vec![("web-1", "node-1"), ("web-2", "node-2")]);
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p keel-controlplane 2>&1 | tail -40`
Expected: FAIL to compile — `services` module not declared, `keel_spec` crate not found as a dependency yet applied (should already be fixed by Step 1), `Placements::iter` not found.

- [ ] **Step 4: Wire the new module into `keel-controlplane/src/lib.rs`**

Replace the file's contents:

```rust
pub mod http;
pub mod placements;
pub mod registry;
pub mod scheduler;
pub mod services;
pub mod subnet;
pub mod tls;
pub mod wire;
pub mod worker;

pub use placements::Placements;
pub use registry::Registry;
pub use services::Services;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p keel-controlplane 2>&1 | tail -80`
Expected: PASS, all tests including the new `services.rs` and `placements.rs` ones.

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/Cargo.toml keel-controlplane/src/services.rs keel-controlplane/src/placements.rs keel-controlplane/src/lib.rs
git commit -m "Add the Services registry, replica naming, and ownership tracking to keel-controlplane"
```

---

### Task 3: `keel-controlplane` — same-service node spreading

**Files:**
- Modify: `keel-controlplane/src/scheduler.rs`
- Modify: `keel-controlplane/src/services.rs`

**Interfaces:**
- Consumes: `Placements::iter` (Task 2), `scheduler::pick_node`/`NodeResources` (existing, untouched logic).
- Produces: `services::nodes_hosting_service(service_name: &str, placements: &Placements) -> HashSet<String>`; `services::pick_node_for_service(candidates: Vec<scheduler::NodeResources>, busy_nodes: &HashSet<String>) -> Result<String, scheduler::ScheduleError>`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-controlplane/src/services.rs`'s `#[cfg(test)] mod tests` (the module needs `use crate::scheduler::{self, NodeResources};` and `use std::collections::HashSet;` added to its `use` list at the top):

```rust
    fn node(id: &str, capacity_cpu: f64, capacity_memory: u64) -> NodeResources {
        NodeResources { id: id.to_string(), capacity_cpu, capacity_memory, committed_cpu: 0.0, committed_memory: 0 }
    }

    #[test]
    fn nodes_hosting_service_finds_only_this_services_placements() {
        let mut placements = Placements::new();
        placements.set("web-0".to_string(), "node-1".to_string());
        placements.set("web-1".to_string(), "node-2".to_string());
        placements.set("other-jail".to_string(), "node-3".to_string());
        let busy = nodes_hosting_service("web", &placements);
        assert_eq!(busy, HashSet::from(["node-1".to_string(), "node-2".to_string()]));
    }

    #[test]
    fn pick_node_for_service_avoids_a_busy_node_when_an_alternative_exists() {
        let candidates = vec![node("node-1", 4.0, 100), node("node-2", 4.0, 100)];
        let busy = HashSet::from(["node-1".to_string()]);
        assert_eq!(pick_node_for_service(candidates, &busy), Ok("node-2".to_string()));
    }

    #[test]
    fn pick_node_for_service_falls_back_to_bin_packing_once_every_node_is_busy() {
        let candidates = vec![node("node-1", 4.0, 100), node("node-2", 4.0, 100)];
        let busy = HashSet::from(["node-1".to_string(), "node-2".to_string()]);
        // Both busy: falls back to the unfiltered candidate list, which
        // `pick_node`'s own tie-break (ascending id) picks node-1.
        assert_eq!(pick_node_for_service(candidates, &busy), Ok("node-1".to_string()));
    }

    #[test]
    fn pick_node_for_service_with_no_candidates_at_all_is_no_available_nodes() {
        assert_eq!(pick_node_for_service(vec![], &HashSet::new()), Err(scheduler::ScheduleError::NoAvailableNodes));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keel-controlplane --lib services:: 2>&1 | tail -40`
Expected: FAIL to compile — `nodes_hosting_service`/`pick_node_for_service` not found, `NodeResources` doesn't implement `Clone`/`PartialEq`/`Debug` (needed for the test's `assert_eq!` and for `pick_node_for_service`'s internal `.clone()`).

- [ ] **Step 3: Derive `Clone`/`Debug`/`PartialEq` on `NodeResources`**

In `keel-controlplane/src/scheduler.rs`, change line 7:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct NodeResources {
```

(A pure additive derive — `pick_node`'s own logic and its existing tests at the bottom of this file are completely untouched, matching the design spec's explicit requirement that `scheduler.rs`'s pure function stay untouched. The derive is needed because the spreading wrapper below must try `pick_node` against a filtered candidate list and, on failure, retry against the original unfiltered list, without being able to reconstruct `NodeResources` values that were moved into the first attempt.)

- [ ] **Step 4: Add `nodes_hosting_service`/`pick_node_for_service` to `keel-controlplane/src/services.rs`**

Add `use crate::scheduler::{self, NodeResources};` and `use std::collections::HashSet;` to the top of the file (alongside the existing `use` lines), then add after `diff_replicas`:

```rust
/// Every node currently hosting at least one of `service_name`'s replicas,
/// per `Placements` (matching replica names by the `<service_name>-`
/// prefix via `replica_index`).
pub fn nodes_hosting_service(service_name: &str, placements: &Placements) -> HashSet<String> {
    placements
        .iter()
        .filter(|(jail_name, _)| replica_index(service_name, jail_name).is_some())
        .map(|(_, node_id)| node_id.to_string())
        .collect()
}

/// Prefers a node not in `busy_nodes` (same-service spreading); falls back
/// to `pick_node`'s plain headroom-based bin-packing over the *unfiltered*
/// `candidates` once every candidate is busy. `scheduler::pick_node` itself
/// is unchanged -- this is purely a filter applied by its caller.
pub fn pick_node_for_service(
    candidates: Vec<NodeResources>,
    busy_nodes: &HashSet<String>,
) -> Result<String, scheduler::ScheduleError> {
    let filtered: Vec<NodeResources> = candidates.iter().cloned().filter(|n| !busy_nodes.contains(&n.id)).collect();
    if !filtered.is_empty() {
        scheduler::pick_node(&filtered)
    } else {
        scheduler::pick_node(&candidates)
    }
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p keel-controlplane 2>&1 | tail -80`
Expected: PASS, all tests including `scheduler.rs`'s pre-existing ones (unaffected) and the new spreading tests.

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/scheduler.rs keel-controlplane/src/services.rs
git commit -m "Add same-service node-spreading on top of the unchanged scheduler"
```

---

### Task 4: `keel-controlplane` — address auto-assignment within a node's `pod_cidr`

**Files:**
- Create: `keel-controlplane/src/addresses.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Produces: `addresses::UsedAddresses` (`Clone`, `Default`) with `new`, `is_used(&self, node_id: &str, addr: Ipv4Addr) -> bool`, `record(&mut self, jail_name: String, node_id: String, addr: Ipv4Addr)`, `release(&mut self, jail_name: &str)`, `address_of(&self, jail_name: &str) -> Option<Ipv4Addr>`; `addresses::first_free_address(pod_cidr: Ipv4Net, node_id: &str, used: &UsedAddresses) -> Option<Ipv4Addr>`.

- [ ] **Step 1: Write the failing tests**

Create `keel-controlplane/src/addresses.rs`:

```rust
use ipnet::Ipv4Net;
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

/// Which addresses are currently assigned to a replica, per node -- lives
/// next to `Placements`/`Services`: no persistence, forgotten on restart,
/// populated when a replica is scheduled and freed when it's torn down.
#[derive(Debug, Default, Clone)]
pub struct UsedAddresses {
    used_by_node: HashMap<String, HashSet<Ipv4Addr>>,
    by_jail: HashMap<String, (String, Ipv4Addr)>,
}

impl UsedAddresses {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_used(&self, node_id: &str, addr: Ipv4Addr) -> bool {
        self.used_by_node.get(node_id).is_some_and(|set| set.contains(&addr))
    }

    pub fn record(&mut self, jail_name: String, node_id: String, addr: Ipv4Addr) {
        self.used_by_node.entry(node_id.clone()).or_default().insert(addr);
        self.by_jail.insert(jail_name, (node_id, addr));
    }

    pub fn release(&mut self, jail_name: &str) {
        if let Some((node_id, addr)) = self.by_jail.remove(jail_name) {
            if let Some(set) = self.used_by_node.get_mut(&node_id) {
                set.remove(&addr);
            }
        }
    }

    pub fn address_of(&self, jail_name: &str) -> Option<Ipv4Addr> {
        self.by_jail.get(jail_name).map(|(_, addr)| *addr)
    }
}

/// The first address in `pod_cidr` not already used on `node_id`, starting
/// from network-plus-2. `Ipv4Net::hosts()` already excludes the network and
/// broadcast addresses; the first host address (network-plus-1) is further
/// skipped here because `keel-net`'s `bridge_gateway` (Milestone 14)
/// permanently reserves it as the node's `keel0` bridge gateway.
pub fn first_free_address(pod_cidr: Ipv4Net, node_id: &str, used: &UsedAddresses) -> Option<Ipv4Addr> {
    pod_cidr.hosts().skip(1).find(|addr| !used.is_used(node_id, *addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn addr(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn first_free_address_skips_network_and_network_plus_one() {
        let used = UsedAddresses::new();
        assert_eq!(first_free_address(cidr("10.0.60.0/24"), "node-1", &used), Some(addr("10.0.60.2")));
    }

    #[test]
    fn first_free_address_skips_addresses_already_recorded_used_on_that_node() {
        let mut used = UsedAddresses::new();
        used.record("web-0".to_string(), "node-1".to_string(), addr("10.0.60.2"));
        assert_eq!(first_free_address(cidr("10.0.60.0/24"), "node-1", &used), Some(addr("10.0.60.3")));
    }

    #[test]
    fn first_free_address_on_a_different_node_is_unaffected_by_another_nodes_usage() {
        let mut used = UsedAddresses::new();
        used.record("web-0".to_string(), "node-1".to_string(), addr("10.0.60.2"));
        assert_eq!(first_free_address(cidr("10.0.60.0/24"), "node-2", &used), Some(addr("10.0.60.2")));
    }

    #[test]
    fn record_then_release_frees_the_address_again() {
        let mut used = UsedAddresses::new();
        used.record("web-0".to_string(), "node-1".to_string(), addr("10.0.60.2"));
        used.release("web-0");
        assert_eq!(first_free_address(cidr("10.0.60.0/24"), "node-1", &used), Some(addr("10.0.60.2")));
    }

    #[test]
    fn address_of_returns_none_for_an_unrecorded_jail() {
        let used = UsedAddresses::new();
        assert_eq!(used.address_of("web-0"), None);
    }

    #[test]
    fn address_of_returns_the_recorded_address() {
        let mut used = UsedAddresses::new();
        used.record("web-0".to_string(), "node-1".to_string(), addr("10.0.60.2"));
        assert_eq!(used.address_of("web-0"), Some(addr("10.0.60.2")));
    }

    #[test]
    fn a_full_pod_cidr_returns_none() {
        // A /30 has exactly 2 host addresses (per `hosts()`): network+1 and
        // network+2. Skipping network+1 leaves only network+2; once that's
        // used, nothing remains.
        let mut used = UsedAddresses::new();
        used.record("web-0".to_string(), "node-1".to_string(), addr("10.0.60.2"));
        assert_eq!(first_free_address(cidr("10.0.60.0/30"), "node-1", &used), None);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keel-controlplane --lib addresses:: 2>&1 | tail -40`
Expected: FAIL to compile — module not declared in `lib.rs` yet.

- [ ] **Step 3: Declare the module in `keel-controlplane/src/lib.rs`**

Add `pub mod addresses;` (alphabetically, after the existing `pub mod http;` and before `pub mod placements;`):

```rust
pub mod addresses;
pub mod http;
pub mod placements;
pub mod registry;
pub mod scheduler;
pub mod services;
pub mod subnet;
pub mod tls;
pub mod wire;
pub mod worker;

pub use placements::Placements;
pub use registry::Registry;
pub use services::Services;
pub use wire::{ErrorBody, NodeRegistration, NodeState, NodeStatus};
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p keel-controlplane 2>&1 | tail -60`
Expected: PASS, all tests.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/addresses.rs keel-controlplane/src/lib.rs
git commit -m "Add per-node used-address tracking and free-address auto-assignment"
```

---

### Task 5: Heartbeat wire-format extension — per-jail running status

**Files:**
- Modify: `keel-controlplane/src/wire.rs`
- Modify: `keel-controlplane/src/registry.rs`
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/http.rs` (only `handle_heartbeat`; route wiring for services comes in Task 8)
- Modify: `keel-agentd/src/registration.rs`

**Interfaces:**
- Produces: `wire::JailHealth { pub name: String, pub running: bool }`; `wire::Heartbeat` gains `pub jails: Vec<JailHealth>` (`#[serde(default)]`); `Registry::heartbeat` gains a `jails: Vec<JailHealth>` parameter (5 args total before `now`); `Registry::is_jail_running(&self, node_id: &str, jail_name: &str) -> bool`; `Command::Heartbeat` becomes `Heartbeat(String, f64, u64, Vec<wire::JailHealth>, Sender<Result<(), UnknownNode>>)` — the reply type is unchanged, only an input field is added, per the design spec's explicit interface contract.

- [ ] **Step 1: Write the failing tests**

Add to `keel-controlplane/src/wire.rs`'s `#[cfg(test)] mod tests` (after `heartbeat_round_trips_through_yaml`):

```rust
    #[test]
    fn heartbeat_with_jails_round_trips_through_yaml() {
        let heartbeat = Heartbeat {
            committed_cpu: 2.0,
            committed_memory: 1024 * 1024 * 1024,
            jails: vec![
                JailHealth { name: "web-0".to_string(), running: true },
                JailHealth { name: "web-1".to_string(), running: false },
            ],
        };
        let yaml = serde_yaml::to_string(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, heartbeat);
    }

    #[test]
    fn heartbeat_without_a_jails_field_defaults_to_empty() {
        let parsed: Heartbeat = serde_yaml::from_str("committed_cpu: 1\ncommitted_memory: 2\n").unwrap();
        assert_eq!(parsed.jails, vec![]);
    }
```

Add to `keel-controlplane/src/registry.rs`'s `#[cfg(test)] mod tests` (after `heartbeat_updates_committed_resources_without_changing_capacity`), and update the existing three `registry.heartbeat(...)` call sites (lines 220, 229, 310) to pass an extra `vec![]` argument:

```rust
    #[test]
    fn heartbeat_records_per_jail_running_status() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();

        registry
            .heartbeat(
                "node-1",
                0.0,
                0,
                vec![
                    crate::wire::JailHealth { name: "web-0".to_string(), running: true },
                    crate::wire::JailHealth { name: "web-1".to_string(), running: false },
                ],
                now,
            )
            .unwrap();

        assert!(registry.is_jail_running("node-1", "web-0"));
        assert!(!registry.is_jail_running("node-1", "web-1"));
    }

    #[test]
    fn is_jail_running_on_an_unreported_jail_is_false() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();
        assert!(!registry.is_jail_running("node-1", "web-0"));
    }

    #[test]
    fn is_jail_running_on_an_unknown_node_is_false() {
        let registry = Registry::new(test_cluster_cidr());
        assert!(!registry.is_jail_running("missing", "web-0"));
    }

    #[test]
    fn a_later_heartbeat_replaces_the_previous_jail_health_report_wholesale() {
        let mut registry = Registry::new(test_cluster_cidr());
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0).unwrap();
        registry
            .heartbeat("node-1", 0.0, 0, vec![crate::wire::JailHealth { name: "web-0".to_string(), running: true }], t0)
            .unwrap();

        let t1 = t0 + Duration::from_secs(5);
        registry.heartbeat("node-1", 0.0, 0, vec![], t1).unwrap();

        assert!(!registry.is_jail_running("node-1", "web-0"), "a heartbeat with no jails must clear the previous report");
    }

    #[test]
    fn pod_cidr_returns_the_registered_nodes_block() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();
        assert_eq!(registry.pod_cidr("node-1"), Some("10.0.131.0/24".parse().unwrap()));
    }

    #[test]
    fn pod_cidr_on_an_unknown_node_is_none() {
        let registry = Registry::new(test_cluster_cidr());
        assert_eq!(registry.pod_cidr("missing"), None);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keel-controlplane 2>&1 | tail -60`
Expected: FAIL to compile — `Heartbeat.jails` field doesn't exist, `Registry::heartbeat` arity mismatch, `is_jail_running`/`pod_cidr` not found.

- [ ] **Step 3: Extend `keel-controlplane/src/wire.rs`**

Replace lines 11-15 (the `Heartbeat` struct):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub committed_cpu: f64,
    pub committed_memory: u64,
    #[serde(default)]
    pub jails: Vec<JailHealth>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailHealth {
    pub name: String,
    pub running: bool,
}
```

Update the existing `heartbeat_round_trips_through_yaml` test (line 90-95) to include the new field so it stays representative:

```rust
    #[test]
    fn heartbeat_round_trips_through_yaml() {
        let heartbeat = Heartbeat { committed_cpu: 2.0, committed_memory: 1024 * 1024 * 1024, jails: vec![] };
        let yaml = serde_yaml::to_string(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, heartbeat);
    }
```

- [ ] **Step 4: Extend `keel-controlplane/src/registry.rs`**

Add `running_jails: HashMap<String, bool>` to `NodeRecord` (line 10-18):

```rust
#[derive(Debug, Clone)]
struct NodeRecord {
    addr: String,
    last_heartbeat: Instant,
    capacity_cpu: f64,
    capacity_memory: u64,
    committed_cpu: f64,
    committed_memory: u64,
    pod_cidr: Ipv4Net,
    running_jails: HashMap<String, bool>,
}
```

Initialize it in `register` (the `NodeRecord { ... }` literal at line 71-80):

```rust
        self.nodes.insert(
            id,
            NodeRecord {
                addr,
                last_heartbeat: now,
                capacity_cpu,
                capacity_memory,
                committed_cpu: 0.0,
                committed_memory: 0,
                pod_cidr,
                running_jails: HashMap::new(),
            },
        );
```

Replace `heartbeat` (lines 84-100) to accept and store the jails list, and add `is_jail_running`/`pod_cidr` after it:

```rust
    pub fn heartbeat(
        &mut self,
        id: &str,
        committed_cpu: f64,
        committed_memory: u64,
        jails: Vec<crate::wire::JailHealth>,
        now: Instant,
    ) -> Result<(), UnknownNode> {
        match self.nodes.get_mut(id) {
            Some(record) => {
                record.last_heartbeat = now;
                record.committed_cpu = committed_cpu;
                record.committed_memory = committed_memory;
                record.running_jails = jails.into_iter().map(|j| (j.name, j.running)).collect();
                Ok(())
            }
            None => Err(UnknownNode(id.to_string())),
        }
    }

    /// Whether `node_id`'s most recent heartbeat reported `jail_name` as
    /// running. `false` for an unknown node, an unknown jail, or a node
    /// that has never heartbeated with jail health at all.
    pub fn is_jail_running(&self, node_id: &str, jail_name: &str) -> bool {
        self.nodes.get(node_id).and_then(|r| r.running_jails.get(jail_name)).copied().unwrap_or(false)
    }

    /// The node's assigned `pod_cidr`, typed -- for consumers (address
    /// auto-assignment) that need to do arithmetic on it rather than just
    /// display it, unlike `list()`'s stringified `NodeStatus.pod_cidr`.
    pub fn pod_cidr(&self, node_id: &str) -> Option<Ipv4Net> {
        self.nodes.get(node_id).map(|r| r.pod_cidr)
    }
```

Update the three existing test call sites: line 220 (`heartbeat_on_a_known_node_updates_its_last_heartbeat`) becomes `registry.heartbeat("node-1", 0.0, 0, vec![], t1).unwrap();`; line 229 (`heartbeat_on_an_unknown_node_returns_unknown_node_error`) becomes `registry.heartbeat("missing", 0.0, 0, vec![], Instant::now())`; line 310 (`heartbeat_updates_committed_resources_without_changing_capacity`) becomes `registry.heartbeat("node-1", 2.0, 1024 * 1024 * 1024, vec![], t1).unwrap();`.

- [ ] **Step 5: Update `Command::Heartbeat` in `keel-controlplane/src/worker.rs`**

Change the variant declaration (line 27):

```rust
    Heartbeat(String, f64, u64, Vec<crate::wire::JailHealth>, Sender<Result<(), UnknownNode>>),
```

Change the match arm (lines 52-55):

```rust
        Command::Heartbeat(id, committed_cpu, committed_memory, jails, reply) => {
            let result = registry.heartbeat(&id, committed_cpu, committed_memory, jails, Instant::now());
            let _ = reply.send(result);
        }
```

Update the three test call sites: line 148 becomes `commands.send(Command::Heartbeat("missing".to_string(), 0.0, 0, vec![], hb_tx)).unwrap();`; line 169 becomes `commands.send(Command::Heartbeat("node-1".to_string(), 0.0, 0, vec![], hb_tx)).unwrap();`; line 222 (the `heartbeat_node` test helper) becomes `commands.send(Command::Heartbeat(id.to_string(), committed_cpu, committed_memory, vec![], hb_tx)).unwrap();`.

- [ ] **Step 6: Update `handle_heartbeat` in `keel-controlplane/src/http.rs`**

Replace lines 274-291:

```rust
fn handle_heartbeat(id: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let heartbeat: Heartbeat = match serde_yaml::from_slice(body) {
        Ok(h) => h,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands
        .send(Command::Heartbeat(id.to_string(), heartbeat.committed_cpu, heartbeat.committed_memory, heartbeat.jails, reply_tx))
        .is_err()
    {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}
```

(Task 8 will further extend this function to also trigger `Command::ReconcileServices` after a successful ack; this step keeps it minimal and passing on its own.)

- [ ] **Step 7: Update `keel-agentd/src/registration.rs`'s `heartbeat_once` to report per-jail health**

Replace `heartbeat_once` (lines 107-121):

```rust
fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let (resources_tx, resources_rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(resources_tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = resources_rx.recv().map_err(|_| "worker did not respond".to_string())?;

    let (jails_tx, jails_rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::Get(None, jails_tx))
        .map_err(|_| "worker is not running".to_string())?;
    let statuses = jails_rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let jails: Vec<keel_controlplane::wire::JailHealth> = statuses
        .into_iter()
        .map(|s| keel_controlplane::wire::JailHealth { name: s.record.spec.metadata.name, running: s.running })
        .collect();

    let heartbeat = keel_controlplane::wire::Heartbeat { committed_cpu, committed_memory, jails };
    let body = serde_yaml::to_string(&heartbeat).map_err(|e| format!("failed to serialize heartbeat: {e}"))?;
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, client_config)?;
    Ok(())
}
```

Add `use keel_agentd::worker::Command as AgentCommand;`? No — `heartbeat_once` already lives inside `keel-agentd`'s own `registration.rs`, so `crate::worker::Command::Get` refers to `keel-agentd`'s own reconciler worker `Command` (already imported via `crate::worker` used elsewhere in this same file for `CommittedResources`/`AddRoute`/`RemoveRoute`) — no new `use` needed. Also add `serde_yaml` is already a dependency of `keel-agentd` (confirmed in `Cargo.toml`).

- [ ] **Step 8: Run the full affected set to verify pass**

Run: `cargo test -p keel-controlplane -p keel-agentd 2>&1 | tail -100`
Expected: PASS, all tests including `registers_and_then_keeps_heartbeating` and `heartbeats_report_the_reconcilers_committed_resources` in `registration.rs` (unaffected by the body-format change since they only assert on `committed_cpu`/`committed_memory` substrings, which remain present in the new YAML body's output).

- [ ] **Step 9: Commit**

```bash
git add keel-controlplane/src/wire.rs keel-controlplane/src/registry.rs keel-controlplane/src/worker.rs keel-controlplane/src/http.rs keel-agentd/src/registration.rs
git commit -m "Extend the heartbeat wire format with per-jail running status"
```

---

### Task 6: `keel-controlplane` — `Command::ApplyService` and `Command::OwnerOf`

**Files:**
- Modify: `keel-controlplane/src/worker.rs`

**Interfaces:**
- Consumes: `services::{owner_of, Owner, ApplyServiceError}` (Task 2), `keel_spec::JailTemplate` (Task 1).
- Produces: `Command::ApplyService(String, u32, keel_spec::JailTemplate, Sender<Result<(), services::ApplyServiceError>>)`; `Command::OwnerOf(String, Sender<Option<services::Owner>>)`; `worker::spawn` gains two parameters: `spawn(registry: Registry, placements: Placements, services: Services, used_addresses: UsedAddresses) -> (JoinHandle<()>, Sender<Command>)`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-controlplane/src/worker.rs`'s `#[cfg(test)] mod tests` (add `use crate::addresses::UsedAddresses; use crate::services::{ApplyServiceError, Owner, Services};` and `use keel_spec::{JailTemplate, ResourcesSpec, RestartPolicy, TemplateNetworkSpec};` to its `use` list), and update every existing `spawn(Registry::new(...), Placements::new())` call site in this file (there are 15 of them, one per test) to `spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new())`:

```rust
    fn template() -> JailTemplate {
        JailTemplate {
            image: "base/14.2-web".to_string(),
            command: vec!["/usr/local/bin/myapp".to_string()],
            network: TemplateNetworkSpec { vnet: true, bridge: "keel0".to_string() },
            resources: ResourcesSpec { cpu: "1".to_string(), memory: "256M".to_string() },
            restart_policy: RestartPolicy::Always,
        }
    }

    #[test]
    fn apply_service_command_creates_a_new_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, template(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(()));
    }

    #[test]
    fn apply_service_command_rejects_a_template_change_on_an_existing_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx1, rx1) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, template(), tx1)).unwrap();
        rx1.recv().unwrap().unwrap();

        let mut changed = template();
        changed.image = "base/different-image".to_string();
        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, changed, tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Err(ApplyServiceError::TemplateChanged("web".to_string())));
    }

    #[test]
    fn apply_service_command_rejects_a_name_already_used_by_an_unmanaged_jail() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), tx)).unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            Err(ApplyServiceError::NameConflict { name: "web-0".to_string(), owner: Owner::Unmanaged })
        );
    }

    #[test]
    fn apply_service_command_reapplying_the_same_service_with_more_replicas_does_not_conflict_with_itself() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx1, rx1) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), tx1)).unwrap();
        rx1.recv().unwrap().unwrap();

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, template(), tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Ok(()));
    }

    #[test]
    fn owner_of_command_on_an_unplaced_name_is_none() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::OwnerOf("web-0".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), None);
    }

    #[test]
    fn owner_of_command_on_a_service_replica_names_that_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (apply_tx, apply_rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();
        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::OwnerOf("web-0".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Some(Owner::Service("web".to_string())));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keel-controlplane --lib worker:: 2>&1 | tail -60`
Expected: FAIL to compile — `spawn` arity mismatch everywhere, `Command::ApplyService`/`Command::OwnerOf` not found.

- [ ] **Step 3: Update `worker::spawn` and `Command` in `keel-controlplane/src/worker.rs`**

Add imports at the top (after the existing `use` lines):

```rust
use crate::addresses::UsedAddresses;
use crate::services::{self, Owner, Services};
```

Add the two variants to the `Command` enum (after `RemovePlacement`):

```rust
    OwnerOf(String, Sender<Option<Owner>>),
    ApplyService(String, u32, keel_spec::JailTemplate, Sender<Result<(), services::ApplyServiceError>>),
```

Replace `spawn` (lines 36-44):

```rust
pub fn spawn(
    mut registry: Registry,
    mut placements: Placements,
    mut services: Services,
    mut used_addresses: UsedAddresses,
) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, &mut placements, &mut services, &mut used_addresses, command);
        }
    });
    (handle, tx)
}
```

Update `handle_command`'s signature and add the two new match arms:

```rust
fn handle_command(
    registry: &mut Registry,
    placements: &mut Placements,
    services: &mut Services,
    used_addresses: &mut UsedAddresses,
    command: Command,
) {
    match command {
        // ...(existing arms unchanged)...
        Command::OwnerOf(name, reply) => {
            let _ = reply.send(services::owner_of(&name, placements, services));
        }
        Command::ApplyService(name, replicas, template, reply) => {
            let result = (|| {
                for i in 0..replicas {
                    let candidate = services::replica_name(&name, i);
                    if let Some(owner) = services::owner_of(&candidate, placements, services) {
                        let is_self = matches!(&owner, Owner::Service(other) if other == &name);
                        if !is_self {
                            return Err(services::ApplyServiceError::NameConflict { name: candidate, owner });
                        }
                    }
                }
                services.apply(name, replicas, template)
            })();
            let _ = reply.send(result);
        }
    }
}
```

(`used_addresses` is unused by `handle_command` until Task 7 adds the commands that touch it; leave the parameter in place now, since `worker::spawn`'s signature must match its final shape here to avoid a second wide-reaching signature change. Note: this will produce an `unused_variables` warning until Task 7 lands — acceptable for one commit given the deliberate two-task split; if `cargo clippy` is run standalone after this task, prefix the parameter as `_used_addresses` temporarily, or simply proceed to Task 7 immediately, which is the intended order.)

- [ ] **Step 4: Update every other `worker::spawn` call site**

In `keel-controlplane/src/main.rs` (line 87):

```rust
    let (_worker_handle, commands) = worker::spawn(
        Registry::new(cluster_cidr),
        Placements::new(),
        keel_controlplane::Services::new(),
        keel_controlplane::addresses::UsedAddresses::new(),
    );
```

(Add `use keel_controlplane::addresses::UsedAddresses;` and `use keel_controlplane::Services;` to `main.rs`'s existing `use` block, or reference them with the fully-qualified paths shown above — either is fine; use the fully-qualified form to avoid disturbing the existing `use` list's order.)

In `keel-controlplane/src/http.rs`, all three test-helper call sites (lines 420, 978, 1020) — each becomes:

```rust
        let (_worker_handle, commands) = worker::spawn(
            Registry::new("10.0.0.0/16".parse().unwrap()),
            Placements::new(),
            crate::services::Services::new(),
            crate::addresses::UsedAddresses::new(),
        );
```

In `keel-agentd/src/registration.rs` (line 299, inside `start_test_control_plane`):

```rust
        let (_worker_handle, commands) = worker::spawn(
            Registry::new("10.0.0.0/16".parse().unwrap()),
            Placements::new(),
            keel_controlplane::Services::new(),
            keel_controlplane::addresses::UsedAddresses::new(),
        );
```

In `keelctl/tests/cli.rs` (line 92, inside `start_test_control_plane_with_node`):

```rust
    let (_worker_handle, commands) = keel_controlplane::worker::spawn(
        keel_controlplane::Registry::new("10.0.0.0/16".parse().unwrap()),
        keel_controlplane::Placements::new(),
        keel_controlplane::Services::new(),
        keel_controlplane::addresses::UsedAddresses::new(),
    );
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo build --workspace 2>&1 | tail -60 && cargo test --workspace 2>&1 | tail -150`
Expected: builds cleanly; all tests PASS across every crate (this is the first point every `worker::spawn` call site across the whole workspace is exercised together).

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/main.rs keel-controlplane/src/http.rs keel-agentd/src/registration.rs keelctl/tests/cli.rs
git commit -m "Add Command::ApplyService and Command::OwnerOf, threading Services/UsedAddresses through worker::spawn"
```

---

### Task 7: `keel-controlplane` — reconciliation, discovery, and deletion commands

**Files:**
- Modify: `keel-controlplane/src/worker.rs`

**Interfaces:**
- Consumes: everything from Tasks 2-6 (`services::{diff_replicas, replica_name, replica_index, nodes_hosting_service, pick_node_for_service}`, `addresses::{UsedAddresses, first_free_address}`, `Registry::{pod_cidr, is_jail_running, resolve, list}`).
- Produces: `ReplicaAction` enum (`Schedule { replica_name: String, node_id: String, node_addr: String, template: keel_spec::JailTemplate, address: std::net::Ipv4Addr, prefix_len: u8 }` / `TearDown { replica_name: String, node_id: String, node_addr: String }`, both `Clone`/`Debug`/`PartialEq`); `Command::ReconcileServices(Sender<Vec<ReplicaAction>>)`; `Command::DiscoverService(String, Sender<Result<Vec<wire::ServiceReplica>, services::UnknownService>>)`; `Command::ListServices(Sender<Vec<wire::ServiceSummary>>)`; `Command::DeleteService(String, Sender<Result<Vec<ReplicaAction>, services::UnknownService>>)`; `Command::RecordReplicaAddress(String, String, std::net::Ipv4Addr, Sender<()>)`; `Command::ReleaseReplicaAddress(String, Sender<()>)`.
- Also adds `wire::ServiceReplica { pub name: String, pub node: String, pub address: String }` and `wire::ServiceSummary { pub name: String, pub desired_replicas: u32 }` to `keel-controlplane/src/wire.rs`.

- [ ] **Step 1: Add the two wire types**

Add to `keel-controlplane/src/wire.rs` (after `ErrorBody`):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceReplica {
    pub name: String,
    pub node: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    pub desired_replicas: u32,
}
```

And matching round-trip tests in its `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn service_replica_round_trips_through_yaml() {
        let replica = ServiceReplica { name: "web-0".to_string(), node: "node-4".to_string(), address: "10.0.60.2".to_string() };
        let yaml = serde_yaml::to_string(&replica).unwrap();
        assert_eq!(serde_yaml::from_str::<ServiceReplica>(&yaml).unwrap(), replica);
    }

    #[test]
    fn service_summary_round_trips_through_yaml() {
        let summary = ServiceSummary { name: "web".to_string(), desired_replicas: 3 };
        let yaml = serde_yaml::to_string(&summary).unwrap();
        assert_eq!(serde_yaml::from_str::<ServiceSummary>(&yaml).unwrap(), summary);
    }
```

Run: `cargo test -p keel-controlplane --lib wire:: 2>&1 | tail -20` — Expected: PASS immediately (plain data types, no dependent logic).

- [ ] **Step 2: Write the failing worker-level tests**

Add to `keel-controlplane/src/worker.rs`'s `#[cfg(test)] mod tests`, a helper and the new commands' tests:

```rust
    fn apply_service(commands: &Sender<Command>, name: &str, replicas: u32) {
        let (tx, rx) = mpsc::channel();
        commands.send(Command::ApplyService(name.to_string(), replicas, template(), tx)).unwrap();
        rx.recv().unwrap().unwrap();
    }

    fn reconcile(commands: &Sender<Command>) -> Vec<ReplicaAction> {
        let (tx, rx) = mpsc::channel();
        commands.send(Command::ReconcileServices(tx)).unwrap();
        rx.recv().unwrap()
    }

    fn heartbeat_with_jails(commands: &Sender<Command>, id: &str, jails: Vec<crate::wire::JailHealth>) {
        let (tx, rx) = mpsc::channel();
        commands.send(Command::Heartbeat(id.to_string(), 0.0, 0, jails, tx)).unwrap();
        rx.recv().unwrap().unwrap();
    }

    fn running(name: &str) -> crate::wire::JailHealth {
        crate::wire::JailHealth { name: name.to_string(), running: true }
    }

    #[test]
    fn reconcile_services_schedules_every_replica_of_a_brand_new_service_across_distinct_nodes() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 2);

        let actions = reconcile(&commands);
        assert_eq!(actions.len(), 2);
        let node_ids: std::collections::HashSet<String> = actions
            .iter()
            .map(|a| match a {
                ReplicaAction::Schedule { node_id, .. } => node_id.clone(),
                ReplicaAction::TearDown { .. } => panic!("expected only Schedule actions"),
            })
            .collect();
        assert_eq!(node_ids.len(), 2, "expected the two replicas spread across two distinct nodes, got: {actions:?}");
    }

    #[test]
    fn reconcile_services_is_idempotent_once_replicas_are_recorded_placed_and_reported_healthy() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 1);
        reconcile(&commands); // computed, but not yet "recorded" as actually placed

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();
        heartbeat_with_jails(&commands, "node-1", vec![running("web-0")]);

        assert_eq!(reconcile(&commands), vec![], "a fully healthy, fully-placed service needs no further actions");
    }

    #[test]
    fn reconcile_services_leaves_a_crash_looping_replica_on_a_still_alive_node_alone() {
        // A replica whose node is Alive is never rescheduled elsewhere just
        // because it's crash-looping -- that node's own keel-agentd is
        // already retrying it locally via its Milestone-4 crash-loop
        // backoff. Rescheduling on top of that would fight the local
        // backoff and orphan the original, untracked, on its old node.
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 1);
        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();
        heartbeat_with_jails(&commands, "node-1", vec![crate::wire::JailHealth { name: "web-0".to_string(), running: false }]);

        assert_eq!(
            reconcile(&commands),
            vec![],
            "a crash-looping replica on a still-Alive node must be left to local backoff, not rescheduled"
        );
    }

    #[test]
    fn reconcile_services_reschedules_a_replica_whose_node_is_unreachable() {
        // web-0 is "placed" on a node that was never registered at all --
        // registry.resolve() fails for it exactly the way it would for a
        // genuinely Dead node, so this exercises the same "node itself is
        // unreachable, local backoff can't help" path without needing to
        // wait out the real Dead-node heartbeat timeout in a test.
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 1);
        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-unreachable".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let actions = reconcile(&commands);
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], ReplicaAction::Schedule { replica_name, node_id, .. } if replica_name == "web-0" && node_id == "node-1"),
            "expected web-0 rescheduled onto the one real Alive node, got: {actions:?}"
        );
    }

    #[test]
    fn reconcile_services_tears_down_from_the_highest_index_on_scale_down() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 3);
        for i in 0..3 {
            let (tx, rx) = mpsc::channel();
            commands.send(Command::RecordPlacement(format!("web-{i}"), "node-1".to_string(), tx)).unwrap();
            rx.recv().unwrap();
        }
        heartbeat_with_jails(&commands, "node-1", vec![running("web-0"), running("web-1"), running("web-2")]);

        apply_service(&commands, "web", 1); // scale down to 1
        let actions = reconcile(&commands);
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], ReplicaAction::TearDown { replica_name, .. } if replica_name == "web-2"));
        assert!(matches!(&actions[1], ReplicaAction::TearDown { replica_name, .. } if replica_name == "web-1"));
    }

    #[test]
    fn reconcile_services_never_double_assigns_an_address_within_one_pass() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 2); // only one alive node: both replicas land on it

        let actions = reconcile(&commands);
        let addresses: std::collections::HashSet<std::net::Ipv4Addr> = actions
            .iter()
            .map(|a| match a {
                ReplicaAction::Schedule { address, .. } => *address,
                ReplicaAction::TearDown { .. } => panic!("expected only Schedule actions"),
            })
            .collect();
        assert_eq!(addresses.len(), 2, "expected two distinct addresses, got: {actions:?}");
    }

    #[test]
    fn discover_service_on_an_unknown_service_returns_unknown_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        let (tx, rx) = mpsc::channel();
        commands.send(Command::DiscoverService("missing".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(services::UnknownService("missing".to_string())));
    }

    #[test]
    fn discover_service_omits_a_replica_that_is_not_reported_running() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 2);
        for i in 0..2 {
            let (tx, rx) = mpsc::channel();
            commands.send(Command::RecordPlacement(format!("web-{i}"), "node-1".to_string(), tx)).unwrap();
            rx.recv().unwrap();
            let (atx, arx) = mpsc::channel();
            commands
                .send(Command::RecordReplicaAddress(format!("web-{i}"), "node-1".to_string(), format!("10.0.131.{}", 2 + i).parse().unwrap(), atx))
                .unwrap();
            arx.recv().unwrap();
        }
        // web-0 running, web-1 crash-looping.
        heartbeat_with_jails(&commands, "node-1", vec![running("web-0"), crate::wire::JailHealth { name: "web-1".to_string(), running: false }]);

        let (tx, rx) = mpsc::channel();
        commands.send(Command::DiscoverService("web".to_string(), tx)).unwrap();
        let replicas = rx.recv().unwrap().unwrap();
        assert_eq!(replicas, vec![crate::wire::ServiceReplica { name: "web-0".to_string(), node: "node-1".to_string(), address: "10.0.131.2".to_string() }]);
    }

    #[test]
    fn list_services_returns_every_service_sorted_by_name() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        apply_service(&commands, "web", 3);
        apply_service(&commands, "api", 1);

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ListServices(tx)).unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            vec![
                crate::wire::ServiceSummary { name: "api".to_string(), desired_replicas: 1 },
                crate::wire::ServiceSummary { name: "web".to_string(), desired_replicas: 3 },
            ]
        );
    }

    #[test]
    fn delete_service_on_an_unknown_name_returns_unknown_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        let (tx, rx) = mpsc::channel();
        commands.send(Command::DeleteService("missing".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(services::UnknownService("missing".to_string())));
    }

    #[test]
    fn delete_service_returns_a_teardown_action_per_current_placement_and_forgets_the_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 2);
        for i in 0..2 {
            let (tx, rx) = mpsc::channel();
            commands.send(Command::RecordPlacement(format!("web-{i}"), "node-1".to_string(), tx)).unwrap();
            rx.recv().unwrap();
        }

        let (tx, rx) = mpsc::channel();
        commands.send(Command::DeleteService("web".to_string(), tx)).unwrap();
        let actions = rx.recv().unwrap().unwrap();
        assert_eq!(actions.len(), 2);

        // DeleteService only forgets the service definition and reports what
        // needs tearing down; it never touches Placements itself. In the real
        // system, Task 8's execute_replica_actions removes each placement
        // only after successfully forwarding that replica's teardown to its
        // node -- simulate that pairing here before checking the name is
        // free again, since nothing at this layer does it automatically.
        for i in 0..2 {
            let (tx, rx) = mpsc::channel();
            commands.send(Command::RemovePlacement(format!("web-{i}"), tx)).unwrap();
            rx.recv().unwrap();
        }

        // The service definition itself is gone: a later apply of the same
        // name with a different template is a fresh create, not a rejected
        // template change.
        let mut different = template();
        different.image = "base/different-image".to_string();
        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, different, tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Ok(()));
    }

    #[test]
    fn record_then_release_replica_address_round_trips() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        let (tx, rx) = mpsc::channel();
        commands
            .send(Command::RecordReplicaAddress("web-0".to_string(), "node-1".to_string(), "10.0.60.2".parse().unwrap(), tx))
            .unwrap();
        rx.recv().unwrap();

        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        apply_service(&commands, "web", 1);
        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();
        heartbeat_with_jails(&commands, "node-1", vec![running("web-0")]);

        let (dtx, drx) = mpsc::channel();
        commands.send(Command::DiscoverService("web".to_string(), dtx)).unwrap();
        assert_eq!(drx.recv().unwrap().unwrap()[0].address, "10.0.60.2");

        // A real teardown always pairs ReleaseReplicaAddress with
        // RemovePlacement -- both fire together from Task 8's
        // execute_replica_actions right after a successful DELETE forward.
        // Simulate that pairing here rather than releasing in isolation,
        // which can't actually happen against a healthy, still-placed
        // replica in the deployed system.
        let (rtx, rrx) = mpsc::channel();
        commands.send(Command::ReleaseReplicaAddress("web-0".to_string(), rtx)).unwrap();
        rrx.recv().unwrap();
        let (rp_tx, rp_rx) = mpsc::channel();
        commands.send(Command::RemovePlacement("web-0".to_string(), rp_tx)).unwrap();
        rp_rx.recv().unwrap();

        let (dtx2, drx2) = mpsc::channel();
        commands.send(Command::DiscoverService("web".to_string(), dtx2)).unwrap();
        assert_eq!(drx2.recv().unwrap().unwrap(), vec![], "a fully torn-down replica is no longer discoverable");
    }
```

Note: `register_node` here is the existing test helper already defined lower in this same `mod tests` (line 212-218 in the current file) — reused as-is.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p keel-controlplane --lib worker:: 2>&1 | tail -80`
Expected: FAIL to compile — `ReplicaAction`, `Command::ReconcileServices`/`DiscoverService`/`ListServices`/`DeleteService`/`RecordReplicaAddress`/`ReleaseReplicaAddress` not found.

- [ ] **Step 4: Implement in `keel-controlplane/src/worker.rs`**

Add imports (extending the block added in Task 6):

```rust
use crate::addresses::{self, UsedAddresses};
use crate::services::{self, Owner, Services};
use crate::wire;
use std::collections::BTreeSet;
```

Add the `ReplicaAction` enum (near the top, after `PlacementError`):

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ReplicaAction {
    Schedule {
        replica_name: String,
        node_id: String,
        node_addr: String,
        template: keel_spec::JailTemplate,
        address: std::net::Ipv4Addr,
        prefix_len: u8,
    },
    TearDown {
        replica_name: String,
        node_id: String,
        node_addr: String,
    },
}
```

Add the six new `Command` variants (after `ApplyService`):

```rust
    ReconcileServices(Sender<Vec<ReplicaAction>>),
    DiscoverService(String, Sender<Result<Vec<wire::ServiceReplica>, services::UnknownService>>),
    ListServices(Sender<Vec<wire::ServiceSummary>>),
    DeleteService(String, Sender<Result<Vec<ReplicaAction>, services::UnknownService>>),
    RecordReplicaAddress(String, String, std::net::Ipv4Addr, Sender<()>),
    ReleaseReplicaAddress(String, Sender<()>),
```

Add the six match arms in `handle_command` (after `ApplyService`):

```rust
        Command::ReconcileServices(reply) => {
            let now = Instant::now();
            let alive_nodes: Vec<scheduler::NodeResources> = registry
                .list(now)
                .into_iter()
                .filter(|s| s.status == NodeState::Alive)
                .map(|s| scheduler::NodeResources {
                    id: s.id,
                    capacity_cpu: s.capacity_cpu,
                    capacity_memory: s.capacity_memory,
                    committed_cpu: s.committed_cpu,
                    committed_memory: s.committed_memory,
                })
                .collect();

            let mut actions = Vec::new();
            let mut working_used = used_addresses.clone();

            for (service_name, record) in services.list() {
                let placed: Vec<(u32, String, String)> = placements
                    .iter()
                    .filter_map(|(jail_name, node_id)| {
                        services::replica_index(service_name, jail_name).map(|idx| (idx, jail_name.to_string(), node_id.to_string()))
                    })
                    .collect();
                // Deliberately NOT also requiring `is_jail_running`: a
                // replica whose node is Alive still counts as present even
                // while crash-looping, since that node's own keel-agentd is
                // already retrying it locally via its own Milestone-4
                // crash-loop backoff. Rescheduling it elsewhere on top of
                // that would fight the local backoff and orphan the
                // original, untracked, on its old node. Only a node that's
                // actually unreachable (registry.resolve fails, whether
                // Dead or never-registered) makes local recovery impossible
                // and warrants scheduling a replacement. `GET /services`'s
                // own Alive+running check (unchanged, see DiscoverService)
                // still excludes a crash-looping replica from what's
                // actually advertised as usable.
                let present_indices: BTreeSet<u32> = placed
                    .iter()
                    .filter(|(_, _, node_id)| registry.resolve(node_id, now).is_ok())
                    .map(|(idx, _, _)| *idx)
                    .collect();

                let (to_add, to_remove) = services::diff_replicas(record.desired_replicas, &present_indices);
                let mut busy = services::nodes_hosting_service(service_name, placements);

                for idx in to_add {
                    let replica_name = services::replica_name(service_name, idx);
                    let Ok(node_id) = services::pick_node_for_service(alive_nodes.clone(), &busy) else { continue };
                    let Some(pod_cidr) = registry.pod_cidr(&node_id) else { continue };
                    let Some(address) = addresses::first_free_address(pod_cidr, &node_id, &working_used) else { continue };
                    let Ok(node_addr) = registry.resolve(&node_id, now) else { continue };
                    working_used.record(replica_name.clone(), node_id.clone(), address);
                    busy.insert(node_id.clone());
                    actions.push(ReplicaAction::Schedule {
                        replica_name,
                        node_id,
                        node_addr,
                        template: record.template.clone(),
                        address,
                        prefix_len: pod_cidr.prefix_len(),
                    });
                }

                for idx in to_remove {
                    let replica_name = services::replica_name(service_name, idx);
                    let Some(node_id) = placements.get(&replica_name).map(|s| s.to_string()) else { continue };
                    let Ok(node_addr) = registry.resolve(&node_id, now) else { continue };
                    actions.push(ReplicaAction::TearDown { replica_name, node_id, node_addr });
                }
            }

            let _ = reply.send(actions);
        }
        Command::DiscoverService(name, reply) => {
            let result = if services.get(&name).is_none() {
                Err(services::UnknownService(name))
            } else {
                let now = Instant::now();
                let mut replicas: Vec<wire::ServiceReplica> = placements
                    .iter()
                    .filter_map(|(jail_name, node_id)| {
                        services::replica_index(&name, jail_name)?;
                        if registry.resolve(node_id, now).is_ok() && registry.is_jail_running(node_id, jail_name) {
                            let address = used_addresses.address_of(jail_name)?;
                            Some(wire::ServiceReplica { name: jail_name.to_string(), node: node_id.to_string(), address: address.to_string() })
                        } else {
                            None
                        }
                    })
                    .collect();
                replicas.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(replicas)
            };
            let _ = reply.send(result);
        }
        Command::ListServices(reply) => {
            let summaries: Vec<wire::ServiceSummary> = services
                .list()
                .into_iter()
                .map(|(name, record)| wire::ServiceSummary { name: name.to_string(), desired_replicas: record.desired_replicas })
                .collect();
            let _ = reply.send(summaries);
        }
        Command::DeleteService(name, reply) => {
            let result = if services.get(&name).is_none() {
                Err(services::UnknownService(name))
            } else {
                let now = Instant::now();
                let actions: Vec<ReplicaAction> = placements
                    .iter()
                    .filter_map(|(jail_name, node_id)| {
                        services::replica_index(&name, jail_name)?;
                        let node_addr = registry.resolve(node_id, now).ok()?;
                        Some(ReplicaAction::TearDown { replica_name: jail_name.to_string(), node_id: node_id.to_string(), node_addr })
                    })
                    .collect();
                services.remove(&name);
                Ok(actions)
            };
            let _ = reply.send(result);
        }
        Command::RecordReplicaAddress(jail_name, node_id, address, reply) => {
            used_addresses.record(jail_name, node_id, address);
            let _ = reply.send(());
        }
        Command::ReleaseReplicaAddress(jail_name, reply) => {
            used_addresses.release(&jail_name);
            let _ = reply.send(());
        }
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p keel-controlplane 2>&1 | tail -150`
Expected: PASS, all tests including every new one above.

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/worker.rs keel-controlplane/src/wire.rs
git commit -m "Add self-healing reconciliation, discovery, listing, and deletion commands for services"
```

---

### Task 8: `keel-controlplane` HTTP — `/services` routes and the jail-apply ownership guard

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: every `Command` variant from Tasks 6-7; `keel_spec::{parse_and_validate_service, JailTemplate::to_jail_spec}` (Task 1).
- Produces: HTTP routes `PUT /services/<name>`, `GET /services/<name>`, `DELETE /services/<name>`, `GET /services`; a 400 guard on `PUT /jails/<name>` and `PUT /nodes/<id>/jails/<name>` for names owned by a service.

- [ ] **Step 1: Write the failing tests**

Add to `keel-controlplane/src/http.rs`'s `#[cfg(test)] mod tests` (needs `use keel_controlplane::services;` is unnecessary since these are integration-style tests against the running HTTP server via `send_request`; no new imports needed beyond what the file already has):

```rust
    fn service_yaml(name: &str, replicas: u32) -> String {
        format!(
            "apiVersion: keel/v1\nkind: Service\nmetadata:\n  name: {name}\nspec:\n  replicas: {replicas}\n  template:\n    image: base/14.2-web\n    command: [\"/usr/local/bin/myapp\"]\n    network:\n      vnet: true\n      bridge: keel0\n    resources:\n      cpu: \"1\"\n      memory: 256M\n    restartPolicy: Always\n"
        )
    }

    #[test]
    fn put_service_creates_and_schedules_replicas_across_registered_nodes() {
        let cp_addr = start_test_server();
        let node_a = start_fake_remote_tls_agentd(200, "running: true\n");
        let node_b = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_a);
        register_node(&cp_addr, "node-b", &node_b);

        let (status, _) = send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 2));
        assert_eq!(status, 200);
    }

    #[test]
    fn put_service_with_zero_replicas_succeeds_and_schedules_nothing() {
        let cp_addr = start_test_server();
        let (status, _) = send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 0));
        assert_eq!(status, 200);
    }

    #[test]
    fn put_service_changing_the_template_on_an_existing_service_returns_409() {
        let cp_addr = start_test_server();
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));
        let changed = service_yaml("web", 1).replace("base/14.2-web", "base/different-image");
        let (status, _) = send_request(&cp_addr, "PUT", "/services/web", &changed);
        assert_eq!(status, 409);
    }

    #[test]
    fn put_service_colliding_with_an_existing_unmanaged_jail_returns_400() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_addr);
        send_request(&cp_addr, "PUT", "/nodes/node-a/jails/web-0", "apiVersion: keel/v1\n");

        let (status, body) = send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));
        assert_eq!(status, 400);
        assert!(body.contains("web-0"), "expected the conflicting name in the error, got: {body}");
    }

    #[test]
    fn put_jail_colliding_with_an_existing_service_replica_returns_400() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_addr);
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));

        let (status, body) = send_request(&cp_addr, "PUT", "/nodes/node-a/jails/web-0", "apiVersion: keel/v1\n");
        assert_eq!(status, 400);
        assert!(body.contains("service 'web'"), "expected the owning service named in the error, got: {body}");
    }

    #[test]
    fn get_service_returns_only_alive_and_running_replicas() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_addr);
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));

        send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\njails:\n  - name: web-0\n    running: true\n");

        let (status, body) = send_request(&cp_addr, "GET", "/services/web", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-0"), "expected the healthy replica listed, got: {body}");
        assert!(body.contains("node-a"), "got: {body}");
    }

    #[test]
    fn get_service_omits_a_crash_looping_replica() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_addr);
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));

        send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\njails:\n  - name: web-0\n    running: false\n");

        let (status, body) = send_request(&cp_addr, "GET", "/services/web", "");
        assert_eq!(status, 200);
        assert_eq!(body.trim(), "[]", "expected no replicas listed while crash-looping, got: {body}");
    }

    #[test]
    fn get_service_on_an_unknown_name_returns_404() {
        let cp_addr = start_test_server();
        let (status, body) = send_request(&cp_addr, "GET", "/services/missing", "");
        assert_eq!(status, 404);
        assert!(body.contains("missing"));
    }

    #[test]
    fn get_services_bare_lists_every_service() {
        let cp_addr = start_test_server();
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 2));
        send_request(&cp_addr, "PUT", "/services/api", &service_yaml("api", 1));

        let (status, body) = send_request(&cp_addr, "GET", "/services", "");
        assert_eq!(status, 200);
        assert!(body.contains("web"), "got: {body}");
        assert!(body.contains("api"), "got: {body}");
    }

    #[test]
    fn delete_service_tears_down_every_placed_replica() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-a", &node_addr);
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));
        send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\njails:\n  - name: web-0\n    running: true\n");

        let (status, _) = send_request(&cp_addr, "DELETE", "/services/web", "");
        assert_eq!(status, 200);

        let (status, _) = send_request(&cp_addr, "GET", "/services/web", "");
        assert_eq!(status, 404, "expected the service itself to be forgotten after delete");
    }

    #[test]
    fn delete_service_on_an_unknown_name_returns_404() {
        let cp_addr = start_test_server();
        let (status, _) = send_request(&cp_addr, "DELETE", "/services/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn heartbeat_piggybacks_reconciliation_and_replaces_a_replica_once_its_node_is_registered() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_tls_agentd(200, "running: true\n");
        // Apply a 1-replica service before any node exists: it succeeds
        // (best-effort), placing nothing yet.
        let (status, _) = send_request(&cp_addr, "PUT", "/services/web", &service_yaml("web", 1));
        assert_eq!(status, 200);
        let (_, body) = send_request(&cp_addr, "GET", "/services/web", "");
        assert_eq!(body.trim(), "[]", "expected no capacity yet, got: {body}");

        // Once a node registers and heartbeats, the very next heartbeat's
        // piggybacked reconciliation should place the missing replica.
        register_node(&cp_addr, "node-a", &node_addr);
        send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\n");
        send_request(&cp_addr, "POST", "/nodes/node-a/heartbeat", "committed_cpu: 0\ncommitted_memory: 0\njails:\n  - name: web-0\n    running: true\n");

        let (status, body) = send_request(&cp_addr, "GET", "/services/web", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-0"), "expected the replica to have been scheduled by heartbeat-piggybacked reconciliation, got: {body}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keel-controlplane --lib http:: 2>&1 | tail -60`
Expected: FAIL — 404 "no route" for every new path (routes don't exist yet).

- [ ] **Step 3: Implement the routes and helpers in `keel-controlplane/src/http.rs`**

Add to the top `use` block:

```rust
use crate::worker::ReplicaAction;
```

Add four new match arms to `route()` (after the existing `("DELETE", ["jails", name])` arm):

```rust
        ("PUT", ["services", name]) => handle_apply_service(name, &request.body, commands, client_config),
        ("GET", ["services", name]) => handle_get_service(name, commands),
        ("DELETE", ["services", name]) => handle_delete_service(name, commands, client_config),
        ("GET", ["services"]) => handle_list_services(commands),
```

Add the ownership guard at the top of the two existing jail-PUT arms (`("PUT", ["nodes", id, "jails", name])` and `("PUT", ["jails", name])`), replacing them with:

```rust
        ("PUT", ["nodes", id, "jails", name]) => {
            if let Some(response) = reject_if_service_owned(name, commands) {
                return response;
            }
            let (status, body) =
                handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands, client_config);
            if (200..300).contains(&status) {
                send_record_placement(name, id, commands);
            }
            (status, body)
        }
        // ...(GET/DELETE nodes/.../jails arms unchanged)...
        ("PUT", ["jails", name]) => {
            if let Some(response) = reject_if_service_owned(name, commands) {
                return response;
            }
            handle_scheduled_apply(name, &request.body, commands, client_config)
        }
```

Add the new handler functions (after `handle_scheduled_delete`):

```rust
fn reject_if_service_owned(name: &str, commands: &Sender<Command>) -> Option<(u16, Vec<u8>)> {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::OwnerOf(name.to_string(), reply_tx)).is_err() {
        return Some(error_response(500, "control plane worker is not running".to_string()));
    }
    match reply_rx.recv() {
        Ok(Some(crate::services::Owner::Service(owner))) => {
            Some(error_response(400, format!("name '{name}' is already in use by service '{owner}'")))
        }
        Ok(_) => None,
        Err(_) => Some(error_response(500, "control plane worker did not respond".to_string())),
    }
}

fn handle_apply_service(
    name: &str,
    body: &[u8],
    commands: &Sender<Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> (u16, Vec<u8>) {
    let spec: keel_spec::ServiceSpec = match keel_spec::parse_and_validate_service(&String::from_utf8_lossy(body)) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("invalid spec: {e}")),
    };
    if spec.metadata.name != name {
        return error_response(400, format!("path name '{name}' does not match spec.metadata.name '{}'", spec.metadata.name));
    }

    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ApplyService(name.to_string(), spec.spec.replicas, spec.spec.template, reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => {
            reconcile_and_execute(commands, client_config);
            (200, Vec::new())
        }
        Ok(Err(e @ crate::services::ApplyServiceError::TemplateChanged(_))) => error_response(409, e.to_string()),
        Ok(Err(e @ crate::services::ApplyServiceError::NameConflict { .. })) => error_response(400, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_get_service(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::DiscoverService(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(replicas)) => yaml_response(200, &replicas),
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_list_services(commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ListServices(reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(summaries) => yaml_response(200, &summaries),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_delete_service(
    name: &str,
    commands: &Sender<Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::DeleteService(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(actions)) => {
            execute_replica_actions(actions, commands, client_config);
            (200, Vec::new())
        }
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

/// Asks the worker to compute the current best-effort set of scheduling/
/// teardown actions across every service, then executes them. Called right
/// after a successful `Service` apply and right after a successful
/// heartbeat -- the latter is this milestone's "piggyback on the existing
/// heartbeat traffic" self-healing mechanism: no new thread, no new timer,
/// just one more step in handling a request that already happens every 5
/// seconds per node.
fn reconcile_and_execute(commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ReconcileServices(reply_tx)).is_err() {
        return;
    }
    if let Ok(actions) = reply_rx.recv() {
        execute_replica_actions(actions, commands, client_config);
    }
}

fn execute_replica_actions(actions: Vec<ReplicaAction>, commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) {
    for action in actions {
        match action {
            ReplicaAction::Schedule { replica_name, node_id, node_addr, template, address, prefix_len } => {
                let cidr = format!("{address}/{prefix_len}");
                let spec = template.to_jail_spec(&replica_name, &cidr);
                let body = serde_yaml::to_string(&spec).expect("JailSpec serialization should not fail");
                match forward(&node_addr, "PUT", &format!("/jails/{replica_name}"), body.as_bytes(), client_config) {
                    Ok((status, _)) if (200..300).contains(&status) => {
                        send_record_placement(&replica_name, &node_id, commands);
                        send_record_replica_address(&replica_name, &node_id, address, commands);
                    }
                    Ok((status, resp_body)) => eprintln!(
                        "keel-controlplane: failed to schedule replica '{replica_name}' on node '{node_id}': status {status}, body {:?}",
                        String::from_utf8_lossy(&resp_body)
                    ),
                    Err(e) => eprintln!(
                        "keel-controlplane: failed to reach node '{node_id}' at {node_addr} while scheduling replica '{replica_name}': {e}"
                    ),
                }
            }
            ReplicaAction::TearDown { replica_name, node_id, node_addr } => {
                match forward(&node_addr, "DELETE", &format!("/jails/{replica_name}"), &[], client_config) {
                    Ok((status, _)) if (200..300).contains(&status) => {
                        send_remove_placement(&replica_name, commands);
                        send_release_replica_address(&replica_name, commands);
                    }
                    Ok((status, resp_body)) => eprintln!(
                        "keel-controlplane: failed to tear down replica '{replica_name}' on node '{node_id}': status {status}, body {:?}",
                        String::from_utf8_lossy(&resp_body)
                    ),
                    Err(e) => eprintln!(
                        "keel-controlplane: failed to reach node '{node_id}' at {node_addr} while tearing down replica '{replica_name}': {e}"
                    ),
                }
            }
        }
    }
}

fn send_record_replica_address(name: &str, node_id: &str, address: std::net::Ipv4Addr, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RecordReplicaAddress(name.to_string(), node_id.to_string(), address, reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}

fn send_release_replica_address(name: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ReleaseReplicaAddress(name.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}
```

Finally, make `handle_heartbeat` (from Task 5) also piggyback reconciliation on success — it already takes `commands: &Sender<Command>` but not `client_config`; thread it through by updating its call site in `route()` and its own signature:

In `route()`, change the heartbeat arm:

```rust
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, &request.body, commands, client_config),
```

Replace `handle_heartbeat` itself:

```rust
fn handle_heartbeat(id: &str, body: &[u8], commands: &Sender<Command>, client_config: &Arc<rustls::ClientConfig>) -> (u16, Vec<u8>) {
    let heartbeat: Heartbeat = match serde_yaml::from_slice(body) {
        Ok(h) => h,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands
        .send(Command::Heartbeat(id.to_string(), heartbeat.committed_cpu, heartbeat.committed_memory, heartbeat.jails, reply_tx))
        .is_err()
    {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => {
            reconcile_and_execute(commands, client_config);
            (200, Vec::new())
        }
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p keel-controlplane 2>&1 | tail -200`
Expected: PASS, all tests including every new one in this task.

- [ ] **Step 5: Run the full workspace to catch any missed call site**

Run: `cargo build --workspace 2>&1 | tail -60 && cargo test --workspace 2>&1 | tail -200`
Expected: builds cleanly and all tests pass (this exercises `keel-agentd`'s and `keelctl`'s dependencies on `keel-controlplane::worker::Command`/`http::run` once more, confirming nothing else broke).

- [ ] **Step 6: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Add PUT/GET/DELETE /services routes and the jail/service name-collision guard"
```

---

### Task 9: `keelctl` — `kind`-sniffing apply routing and jail→service `get`/`delete` fallback

**Files:**
- Modify: `keelctl/src/main.rs`

**Interfaces:**
- Consumes: `keel_spec::{sniff_kind, parse_and_validate_service}` (Task 1); control-plane `/services/<name>` routes (Task 8).
- Produces: `dispatch`/`send_request`/`send_request_tcp`/`parse_response` now return `Result<(u16, String), String>` instead of `Result<String, String>` (Err reserved for transport-level failures only; any well-formed HTTP response, including non-2xx, is `Ok((status, body))`); new `success_body(Result<(u16, String), String>) -> Result<String, String>` helper restoring the old "non-2xx becomes Err" behavior for callers that don't need the fallback.

- [ ] **Step 1: Write the failing tests**

Add to `keelctl/tests/cli.rs` (after `apply_get_delete_round_trip`):

```rust
fn write_service_spec_file(test_name: &str, service_name: &str, replicas: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("keelctl-test-service-spec-{test_name}.yaml"));
    let yaml = format!(
        "apiVersion: keel/v1\nkind: Service\nmetadata:\n  name: {service_name}\nspec:\n  replicas: {replicas}\n  template:\n    image: base/14.2-web\n    command: [\"/usr/local/bin/myapp\"]\n    network:\n      vnet: true\n      bridge: keel0\n    resources:\n      cpu: \"1\"\n      memory: 256M\n    restartPolicy: Always\n"
    );
    std::fs::write(&path, yaml).unwrap();
    path
}

#[test]
fn apply_routes_a_service_kind_to_the_services_path() {
    let node_addr = start_test_agentd_tcp("service_apply_routing");
    let control_plane_addr = start_test_control_plane_with_node("node-1", &node_addr);
    let spec_path = write_service_spec_file("service_apply_routing", "web", 0);

    let (ok, _, stderr) = run_keelctl_scheduled(&control_plane_addr, &["apply", "-f", spec_path.to_str().unwrap()]);
    assert!(ok, "service apply failed: {stderr}");

    let (ok, body, stderr) = run_keelctl_scheduled(&control_plane_addr, &["get", "web"]);
    assert!(ok, "expected the jail-path 404 to fall back to the service path: {stderr}");
    assert_eq!(body.trim(), "[]", "a zero-replica service has no discoverable replicas yet");
}

#[test]
fn delete_falls_back_from_jail_to_service_on_404() {
    let control_plane_addr = start_test_control_plane_with_node("node-1", "127.0.0.1:1");
    let spec_path = write_service_spec_file("service_delete_fallback", "web", 0);
    run_keelctl_scheduled(&control_plane_addr, &["apply", "-f", spec_path.to_str().unwrap()]);

    let (ok, _, stderr) = run_keelctl_scheduled(&control_plane_addr, &["delete", "web"]);
    assert!(ok, "expected delete to fall back to the service path: {stderr}");
}

#[test]
fn get_on_a_name_that_is_neither_a_jail_nor_a_service_still_reports_jail_not_found() {
    let socket_path = start_test_server("neither_jail_nor_service");
    let (ok, _, stderr) = run_keelctl(&socket_path, &["get", "missing"]);
    assert!(!ok);
    assert!(stderr.contains("not found"), "expected the original jail-not-found error preserved, got: {stderr}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p keelctl 2>&1 | tail -60`
Expected: FAIL — `apply_routes_a_service_kind_to_the_services_path` fails because `run_apply` always calls `keel_spec::parse_and_validate` (rejecting `kind: Service` YAML as an invalid `JailSpec`, since it's missing `spec.image`/`spec.network`/etc. entirely); the fallback tests fail because `run_get`/`run_delete` never retry.

- [ ] **Step 3: Implement in `keelctl/src/main.rs`**

Replace `run_apply` (lines 130-137):

```rust
fn run_apply(target: &Target, args: &[String]) -> Result<String, String> {
    let index = args.iter().position(|a| a == "-f").ok_or("apply requires -f FILE")?;
    let file = args.get(index + 1).ok_or("apply requires -f FILE")?;
    let yaml = std::fs::read_to_string(file).map_err(|e| format!("failed to read {file}: {e}"))?;
    let kind = keel_spec::sniff_kind(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
    if kind == "Service" {
        let spec = keel_spec::parse_and_validate_service(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
        let path = jails_path(target, &format!("/services/{}", spec.metadata.name));
        success_body(dispatch(target, "PUT", &path, &yaml)).map(|_| String::new())
    } else {
        let spec = keel_spec::parse_and_validate(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
        let path = jails_path(target, &format!("/jails/{}", spec.metadata.name));
        success_body(dispatch(target, "PUT", &path, &yaml)).map(|_| String::new())
    }
}
```

Replace `run_get`/`run_delete` (lines 139-152):

```rust
fn run_get(target: &Target, args: &[String]) -> Result<String, String> {
    match args.first() {
        Some(name) => get_or_delete_with_service_fallback(target, "GET", name),
        None => success_body(dispatch(target, "GET", &jails_path(target, "/jails"), "")),
    }
}

fn run_delete(target: &Target, args: &[String]) -> Result<String, String> {
    let name = args.first().ok_or("delete requires a jail name")?;
    get_or_delete_with_service_fallback(target, "DELETE", name).map(|_| String::new())
}

/// Tries `/jails/<name>` first; on a `404`, retries against
/// `/services/<name>` (jail names and service names share one flat
/// namespace, so a 404 on one path is a cheap, unambiguous signal to try
/// the other). If *both* return 404, the original jail-path error is
/// surfaced -- it's the more familiar message, and the only one available
/// at all against a plain single-node `keel-agentd`, which has no
/// `/services` route whatsoever.
fn get_or_delete_with_service_fallback(target: &Target, method: &str, name: &str) -> Result<String, String> {
    let jail_path = jails_path(target, &format!("/jails/{name}"));
    let (status, body) = dispatch(target, method, &jail_path, "")?;
    if status != 404 {
        return success_body(Ok((status, body)));
    }
    let service_path = jails_path(target, &format!("/services/{name}"));
    let (service_status, service_body) = dispatch(target, method, &service_path, "")?;
    if service_status == 404 {
        success_body(Ok((status, body)))
    } else {
        success_body(Ok((service_status, service_body)))
    }
}
```

Replace `dispatch` (lines 121-128) to return the raw status alongside the body:

```rust
fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, tls_ca_file, tls_cert_file, tls_key_file, tls_crl_file, .. } => {
            send_request_tcp(addr, method, path, body, tls_ca_file, tls_cert_file, tls_key_file, tls_crl_file)
        }
    }
}

/// Converts a raw `(status, body)` pair into the old collapsed
/// `Result<String, String>` shape: 2xx becomes `Ok(body)`, anything else
/// becomes `Err` of the parsed `ErrorBody`'s message (or the raw body, if
/// it isn't one).
fn success_body(result: Result<(u16, String), String>) -> Result<String, String> {
    let (status, body) = result?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        let error: ErrorBody = serde_yaml::from_str(&body).unwrap_or(ErrorBody { error: body });
        Err(error.error)
    }
}
```

Replace `send_request` (lines 154-165) to return the status:

```rust
fn send_request(socket: &PathBuf, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("failed to connect to {}: {e}", socket.display()))?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;
    parse_response(&response)
}
```

Replace `send_request_tcp`'s return type (line 169, the signature) to `Result<(u16, String), String>` and its final line (`parse_response(&response)` at line 212, unchanged in body — it already just forwards `parse_response`'s result, so only the function's declared return type annotation needs updating):

```rust
fn send_request_tcp(
    addr: &str,
    method: &str,
    path: &str,
    body: &str,
    tls_ca_file: &PathBuf,
    tls_cert_file: &PathBuf,
    tls_key_file: &PathBuf,
    tls_crl_file: &PathBuf,
) -> Result<(u16, String), String> {
```

(Only the return type in the signature changes; the function body from `let client_config = ...` through `parse_response(&response)` at the end stays exactly as-is, since it already just threads through to `parse_response`.)

Replace `parse_response` (lines 215-243) to stop collapsing non-2xx into `Err` itself:

```rust
fn parse_response(response: &[u8]) -> Result<(u16, String), String> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(response).map_err(|e| format!("malformed response: {e}"))? {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Err("incomplete response from server".to_string()),
    };
    let status = parsed.code.unwrap_or(0);
    let content_length = parsed
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| "response missing Content-Length header".to_string())?;
    let actual = response.len() - header_len;
    if actual != content_length {
        return Err(format!("truncated response: expected {content_length} bytes, got {actual}"));
    }
    let response_body = String::from_utf8_lossy(&response[header_len..]).to_string();
    Ok((status, response_body))
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p keelctl 2>&1 | tail -100`
Expected: PASS, all tests -- including every pre-existing test in `cli.rs` (in particular `apply_get_delete_round_trip`'s final "deleted jail get fails with not found" assertion, and `get_against_a_control_plane_that_truncates_mid_body_fails_instead_of_printing_a_partial_response`, both of which depend on the exact Err-preserving/fallback-avoiding behavior this task's design accounts for) and the four new ones above.

- [ ] **Step 5: Commit**

```bash
git add keelctl/src/main.rs keelctl/tests/cli.rs
git commit -m "Route keelctl apply by kind and fall back from jail to service on 404 for get/delete"
```

---

### Task 10: Final workspace verification, VM verification, and README update

**Files:** README.md (docs only; verification only otherwise).

- [ ] **Step 1: Full workspace build and test**

Run: `cargo build --workspace 2>&1 | tail -60`
Expected: builds cleanly across every crate.

Run: `cargo test --workspace 2>&1 | tail -200`
Expected: PASS, every crate, including every test added across Tasks 1-9.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -80`
Expected: no new warnings attributable to this milestone's code (pre-existing warnings, if any, are out of scope, matching Milestone 12/14's precedent).

- [ ] **Step 2: VM verification (three real nodes, same discipline as every milestone since Milestone 2)**

Using the existing cluster (`--cluster-cidr`, mTLS certs, `pod_cidr` all already live since Milestone 14):

1. Apply a 3-replica service: `keelctl apply -f web-service.yaml --control-plane-addr <cp>:7620 --tls-ca-file ... --tls-cert-file ... --tls-key-file ... --tls-crl-file ...` where `web-service.yaml` is a `kind: Service` with `replicas: 3`. Confirm `keelctl get web` (falling back from `/jails/web` 404 to `/services/web`) lists 3 replicas (`web-0`, `web-1`, `web-2`) spread across distinct nodes with auto-assigned addresses inside each node's own `pod_cidr`, matching `GET /nodes`'s previously-verified blocks from Milestone 14 (`node-4` → `10.0.60.0/24`, `node-5` → `10.0.207.0/24`).
2. Kill one replica's hosting node's `keel-agentd` process. Confirm that within one heartbeat-tick window (~5-10s), the control plane schedules a replacement replica (same name, new node) onto a remaining healthy node, discoverable via `keelctl get web`, with no process anywhere ever restarting.
3. Scale the service up (`replicas: 5`) and back down (`replicas: 2`) via repeated `keelctl apply`; confirm the replica count and named instances converge correctly each time (scale-down removes the *highest*-indexed replicas first).
4. Delete the service (`keelctl delete web`, via the jail→service fallback); confirm every remaining replica jail is torn down on its respective node (`jls` shows none left) and `keelctl get web` now falls all the way through to a jail-not-found-shaped 404.
5. Confirm a plain `kind: Jail` apply/get/delete round trip (Milestone 5-14's existing verification) is completely unaffected throughout.
6. Clean teardown: confirm no leftover jails and no lingering processes on all three VMs afterward.

- [ ] **Step 3: Update the README**

Add a "Milestone 15: service discovery via replica sets" entry to the README's "The journey so far" section (following Milestones 12-14's per-milestone write-up style: what problem it closes, the key design choices — deterministic `<name>-N` naming, spreading-then-bin-packing, heartbeat-piggybacked self-healing, the zero-`keel-agentd`-changes property — and the VM verification results from Step 2), and mark roadmap item 15 done (moving Sub-project 5 to "in progress, milestone 1 of N done" or "complete" depending on whether this is the sub-project's only planned milestone — cross-check against the design spec's own framing, which calls this "the first milestone" of Sub-project 6/discovery-and-load-balancing, implying more milestones follow in that line before it's "complete").

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "Document Milestone 15 completion: service discovery via replica sets"
```

---

## Self-Review Notes

**Spec coverage:**
- Goals: `kind: Service` spec + deterministic naming (Tasks 1, 6, 8); auto-assigned `network.address` within `pod_cidr` (Task 4, wired in Task 7); spreading-then-bin-packing scheduler preference (Task 3); `GET /services/<name>` discovery filtered by Alive+running (Task 7, 8); heartbeat-piggybacked self-healing with no new thread/timer (Tasks 5, 7, 8's `reconcile_and_execute` called from `handle_heartbeat`); `keelctl apply/get/delete` parity (Task 9); zero `keel-agentd` behavior changes beyond the heartbeat body field (Task 5's `heartbeat_once` change is the *only* agentd-side change in the whole plan, and it's purely about what it *reports*, not what it *does*).
- Non-Goals: no proxying/DNS/rolling-updates/persistence/cross-service-awareness/stronger-anti-affinity/IPv6/new-protocol are all satisfied by absence — no task introduces any of them. `Services` is confirmed in-memory-only (Task 2's `Services` has no `store`/persistence module, unlike `keel-agentd`'s `JailRecord`/`store.rs`).
- Architecture: every subsection (`keel-spec`'s `Service` kind, `keel-controlplane`'s `Services` registry reusing `Placements`, scheduling spreading over unchanged `pick_node`, addressing via `pod_cidr` arithmetic with network-plus-1 reserved, heartbeat wire extension, discovery endpoint, heartbeat-piggybacked reconciliation, `keelctl` fallback) maps to a task above.
- Error Handling: template-immutability 409 (Task 6/8), name-collision 400 naming the owner (Task 6/8, both directions), `replicas: 0` valid (Task 2/6 tests), under-capacity is not a hard failure (Task 7's `ReconcileServices` silently skips indices it can't place, Task 8's `put_service_with_zero_replicas_succeeds`/apply-when-no-node-yet test), unknown-service 404 (Task 7/8).
- Testing Strategy: every bullet (keel-spec unit tests, controlplane unit tests for `Services`/spreading/addressing/heartbeat-triggered reconciliation, HTTP-layer tests, keelctl tests, VM verification) has a corresponding task.

**Placeholder scan:** no "TBD"/"handle appropriately"/"similar to Task N" patterns; every step shows complete, concrete code or an exact command with expected output. The one deliberately-deferred piece (Task 6's `used_addresses` parameter going briefly unused before Task 7 consumes it) is called out explicitly with its reasoning, not left implicit.

**Type/signature consistency:** `Command::Heartbeat`'s final shape (`String, f64, u64, Vec<wire::JailHealth>, Sender<Result<(), UnknownNode>>`) is identical everywhere it's constructed (Tasks 5-8). `worker::spawn`'s 4-argument shape (`Registry, Placements, Services, UsedAddresses`) is applied identically at all 6 call sites (Task 6, Step 4) and never referenced with a different arity afterward. `ReplicaAction`'s two variants and their exact field names (`replica_name`, `node_id`, `node_addr`, `template`, `address`, `prefix_len` / `replica_name`, `node_id`, `node_addr`) are defined once in Task 7 and consumed unchanged in Task 8's `execute_replica_actions`. `services::owner_of`/`Owner`/`ApplyServiceError` are defined once in Task 2 and reused verbatim by Task 6's `Command::ApplyService`/`Command::OwnerOf` and Task 8's `reject_if_service_owned`. `keelctl`'s `dispatch`/`send_request`/`send_request_tcp`/`parse_response` all agree on the new `Result<(u16, String), String>` shape (Task 9).
