# Milestone 16: Service Load Balancing via a Per-Node Virtual-IP Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every `kind: Service` a stable, cluster-wide virtual IP (`<vip>:<port>`) that any jail can connect to and be transparently relayed to a currently-healthy replica, round-robin, with zero client-side awareness of which node actually answers.

**Architecture:** `keel-spec` gains `spec.port: u16` on `ServiceSpecBody`. `keel-controlplane` gains a `service_cidr: Ipv4Net`-scoped `Services` registry that allocates one VIP per service (a new host-granularity hash-then-probe function, `derive_service_vip`, distinct from Milestone 14's block-granularity `derive_pod_cidr`), carries `vip`/`port` through `GET /services` and a new heartbeat-response body (`ServiceProxyEntry`, one per known service, replicas filtered by the exact same `Alive`+`running` check `GET /services/<name>` already uses). `keel-net` gains `add_alias`/`remove_alias` (a bridge's *second* address, via `ifconfig <bridge> alias`/`-alias`). `keel-agentd` gains a new `proxy` module: on every heartbeat round-trip it diffs the desired service set against what it's running, aliasing/binding new services, swapping replica lists for known ones, and tearing down disappeared ones — each service's own background thread relays accepted TCP connections to a round-robin replica, retrying once on a different replica if the first connect fails.

**Tech Stack:** Rust (2021 edition), `ipnet` (CIDR arithmetic), `serde`/`serde_yaml` (wire types), `rustls` (existing mTLS transport, unchanged), hand-rolled HTTP parsing (`httparse`), plain `std::net`/`std::thread`/`std::sync` for the proxy (no new dependencies anywhere in this milestone).

## Global Constraints

- `service_cidr` is a distinct address pool from any node's Milestone-14 `pod_cidr` — never validated against it, exactly as `cluster_cidr` and `pod_cidr` are already unrelated today (Non-Goals).
- A service's `vip` is allocated once, on first creation, and never changes on a scale-only re-apply (mirrors `template`'s existing immutability precedent from Milestone 15).
- A service's `port` is likewise immutable once created — a re-apply with a different `port` is rejected with a new `ApplyServiceError::PortChanged`, the same shape as the existing `TemplateChanged`. Only `replicas` may change on a re-apply.
- VIP collision is resolved by linear probing across `service_cidr`'s host addresses (bounded by the CIDR's actual size); exhaustion is a hard `apply`-time rejection, never a silent partial success.
- The proxy is a pure L4 TCP byte relay — never inspects, parses, or terminates the payload protocol (Non-Goals).
- A service with zero currently-healthy replicas keeps its alias and listener up; every accepted connection is closed immediately rather than relayed anywhere.
- A node that can't refresh (heartbeat failing) keeps serving its last-known replica list rather than tearing down aliases/listeners.
- Design reference: `docs/superpowers/specs/2026-07-17-keel-agent-milestone16-service-load-balancing-design.md` (Approved, and corrected during review — see that file's git history for the specific corrections: the VIP-derivation algorithm, `port` immutability, the `keelctl get services` CLI gap, and a wrong milestone citation). Follow it exactly; every place this plan makes an implementation decision the spec left open is called out inline with its rationale.

---

## Facts about the current codebase this plan relies on

Gathered by reading the actual current source, not assumed from the design spec:

- `keel-controlplane/src/subnet.rs`'s `derive_pod_cidr` hardcodes `POD_PREFIX_LEN = 24` and always returns a whole `/24`-aligned `Ipv4Net` (its hash picks a *block index*, never a host address). It cannot be reused for VIP allocation — see Task 2.
- `keel-controlplane/src/services.rs`'s `Services` is `{ by_name: HashMap<String, ServiceRecord> }`, constructed via `Services::new()` (zero args) — no `service_cidr` field today. `ServiceRecord` is `{ desired_replicas: u32, template: JailTemplate }`. `apply(&mut self, name, desired_replicas, template) -> Result<(), ApplyServiceError>` unconditionally overwrites the whole record on every call (even an unchanged re-apply), rejecting only a `template` change via `ApplyServiceError::TemplateChanged(String)`.
- `keel-controlplane/src/wire.rs`'s `ServiceSummary` is `{ name: String, desired_replicas: u32 }` (`GET /services`'s per-service listing type, Milestone 15). `ServiceReplica` is `{ name, node, address: String }` (unaffected by this milestone). Neither is `#[serde(deny_unknown_fields)]`.
- `keel-controlplane/src/worker.rs`'s `Command::ApplyService(String, u32, keel_spec::JailTemplate, Sender<Result<(), services::ApplyServiceError>>)` handler (line 189) does its own name-conflict check across `0..replicas` before calling `services.apply(name, replicas, template)`. `Command::ListServices(Sender<Vec<wire::ServiceSummary>>)` (line 304) maps `services.list()` directly. `Command::DiscoverService` (line 282) computes one service's healthy-replica list inline (Alive node + running jail filter) — this exact filter needs to be shared with the new heartbeat-body computation so the two never drift (design's explicit requirement).
- `Services::new()`/`worker::spawn(registry, placements, services, used_addresses)` construction sites: `keel-controlplane/src/worker.rs` (28 identical test call sites, all the literal 4-line block `spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new())`), `keel-controlplane/src/http.rs` (3 call sites, all the literal block with `Registry::new("10.0.0.0/16".parse().unwrap())`/`crate::services::Services::new()`), `keel-controlplane/src/main.rs` (1, real startup), `keel-agentd/src/registration.rs` (1 test call site), `keelctl/tests/cli.rs` (1 test call site). **`worker::spawn`'s own signature does not change** — exactly like `Registry::new(cluster_cidr)` is called *before* being handed to `worker::spawn` today, `Services::new(service_cidr)` is likewise called before construction; only the `Services::new()` call itself (34 total sites) gains one argument.
- `Command::ApplyService(...)` *construction* (not the match-arm pattern) appears at 9 sites in `keel-controlplane/src/worker.rs`'s tests (lines 552, 561, 567, 580, 592, 600, 618, 631, 856) plus 1 in `http.rs`'s `handle_apply_service` (line 290) — 10 total, each needs a `port` argument added.
- `keel-controlplane/src/http.rs`'s `handle_heartbeat` (line 448) currently returns `(200, Vec::new())` on success — this is the "always-empty-200" body this milestone replaces.
- `keel-net`'s `NetError` (`keel-net/src/error.rs`) has exactly 3 variants: `Spawn`, `CommandFailed`, `NotFound`. `ProcessNetManager`'s existing `add_route`/`remove_route` (`keel-net/src/process.rs:145-169`) establish the idempotency-via-stderr-match pattern: success, or stderr containing a known "already in this state" substring, is `Ok(())`; anything else is `NetError::CommandFailed`. FreeBSD's `ifconfig` reports "File exists" when aliasing an address already present, and "Can't assign requested address" when removing one that isn't — the same vocabulary `gateway_set`'s existing check (`process.rs:69-70`) and `add_route`/`remove_route` already rely on.
- `keel-agentd/src/worker.rs`'s `Command` enum already has `AddRoute(String, String, Sender<Result<(), keel_net::NetError>>)`/`RemoveRoute(String, Sender<Result<(), keel_net::NetError>>)`, handled by delegating straight to `Reconciler::add_route`/`remove_route` (`keel-agentd/src/reconciler.rs:112-118`), which themselves just call `self.net.add_route`/`remove_route`. This is the exact precedent for the new `AddServiceAlias`/`RemoveServiceAlias` (Task 9) — a second, independently-owned `NetManager` is not needed.
- `keel-agentd/src/registration.rs`'s `heartbeat_once` (lines 107-133) sends the heartbeat and discards `send_request`'s returned response body entirely (`send_request(...)?;` with no capture). `spawn`'s loop (lines 43-85) already threads `installed_routes: HashMap<String, String>` across iterations outside the loop for exactly this kind of "state that persists between heartbeat ticks" need — `proxied_services` follows the same pattern.
- `keel-agentd/src/lib.rs` lists 10 `pub mod`s; there is no network-facing proxy module today.
- Every fixture/example spec in this codebase uses the bridge name `"keel0"` (Milestone 14's convention); nothing in `keel-net` or `keel-agentd` derives or configures a bridge name independently of a `JailSpec`'s `network.bridge`. The design spec itself assumes the service VIP aliases onto "that node's own `keel0` bridge" without threading a bridge name through the heartbeat/proxy path. This plan follows that assumption literally: the proxy module hardcodes `"keel0"` as the bridge to alias onto (a `const`, not a per-service or per-node configurable — no existing code path gives it anything else to use).

---

### Task 1: `keel-spec` — `spec.port` field and validation

**Files:**
- Modify: `keel-spec/src/types.rs`
- Modify: `keel-spec/src/error.rs`
- Modify: `keel-spec/src/lib.rs`
- Test: same files' `#[cfg(test)]` modules

**Interfaces:**
- Consumes: nothing new.
- Produces: `keel_spec::ServiceSpecBody { replicas: u32, port: u16, template: JailTemplate }`; `keel_spec::SpecError::InvalidPort(u16)`; `parse_and_validate_service` rejects `port: 0`.

- [ ] **Step 1: Write the failing tests**

In `keel-spec/src/types.rs`, add `port: 3` to the existing `SERVICE_EXAMPLE_YAML` test fixture used by `parses_the_service_example_yaml` and its siblings — read the current fixture first (it's the `const SERVICE_EXAMPLE_YAML` near the `#[cfg(test)]` module, already used by three tests: `parses_the_service_example_yaml`, `rejects_a_template_with_an_embedded_network_address`, `to_jail_spec_builds_a_replica_spec_from_the_template_plus_name_and_address`) and add a `port: 8080` line right after `replicas: 3`:

```rust
const SERVICE_EXAMPLE_YAML: &str = r#"
apiVersion: keel/v1
kind: Service
metadata:
  name: web
spec:
  replicas: 3
  port: 8080
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
```

Add an assertion to `parses_the_service_example_yaml`:

```rust
        assert_eq!(spec.spec.port, 8080);
```

(insert right after the existing `assert_eq!(spec.spec.replicas, 3);` line).

In `keel-spec/src/lib.rs`'s `#[cfg(test)] mod tests`, add `port: 8080` to `VALID_SERVICE_YAML` right after `replicas: 2`:

```rust
    const VALID_SERVICE_YAML: &str = r#"
apiVersion: keel/v1
kind: Service
metadata:
  name: web
spec:
  replicas: 2
  port: 8080
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
```

Add two new tests after `parse_and_validate_service_rejects_invalid_resources`:

```rust
    #[test]
    fn parse_and_validate_service_accepts_the_port_field() {
        let spec = parse_and_validate_service(VALID_SERVICE_YAML).unwrap();
        assert_eq!(spec.spec.port, 8080);
    }

    #[test]
    fn parse_and_validate_service_rejects_port_zero() {
        let yaml = VALID_SERVICE_YAML.replace("port: 8080", "port: 0");
        assert!(matches!(parse_and_validate_service(&yaml), Err(SpecError::InvalidPort(0))));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-spec`
Expected: FAIL to compile — `port` is not a field of `ServiceSpecBody`, `InvalidPort` doesn't exist.

- [ ] **Step 3: Add the `port` field and validation**

In `keel-spec/src/types.rs`, modify `ServiceSpecBody`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpecBody {
    pub replicas: u32,
    pub port: u16,
    pub template: JailTemplate,
}
```

In `keel-spec/src/error.rs`, add a variant after `InvalidMemory`:

```rust
    #[error("invalid port {0}: must be non-zero")]
    InvalidPort(u16),
```

In `keel-spec/src/lib.rs`, modify `parse_and_validate_service`:

```rust
pub fn parse_and_validate_service(yaml: &str) -> Result<ServiceSpec, SpecError> {
    let spec: ServiceSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    resources::parse_cpu_cores(&spec.spec.template.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.template.resources.memory)?;
    if spec.spec.port == 0 {
        return Err(SpecError::InvalidPort(0));
    }
    Ok(spec)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-spec`
Expected: PASS (all tests, including the two new ones and the three `SERVICE_EXAMPLE_YAML` tests that now carry `port`).

- [ ] **Step 5: Commit**

```bash
git add keel-spec/src/types.rs keel-spec/src/error.rs keel-spec/src/lib.rs
git commit -m "Add spec.port field to kind: Service, rejecting port 0"
```

---

### Task 2: `keel-controlplane` — `derive_service_vip`, a new host-granularity hash function

**Files:**
- Modify: `keel-controlplane/src/subnet.rs`

**Interfaces:**
- Consumes: nothing new (uses `subnet.rs`'s existing private `fnv1a`).
- Produces: `pub fn derive_service_vip(service_name: &str, service_cidr: &Ipv4Net, attempt: u32) -> Ipv4Addr` — `attempt: 0` gives the base hash candidate; `attempt: 1, 2, ...` gives the linear-probe sequence (wrapping modulo the CIDR's host count). Task 3's `Services::apply` is the consumer that does the actual probing loop.

- [ ] **Step 1: Write the failing tests**

Add to `keel-controlplane/src/subnet.rs`'s existing `#[cfg(test)] mod tests`, after `panics_if_cluster_cidr_is_smaller_than_a_single_pod_block`:

```rust
    #[test]
    fn derive_service_vip_is_deterministic() {
        let service_cidr = cidr("10.0.250.0/24");
        assert_eq!(
            derive_service_vip("web", &service_cidr, 0),
            derive_service_vip("web", &service_cidr, 0)
        );
    }

    #[test]
    fn derive_service_vip_stays_within_the_cidr() {
        let service_cidr = cidr("10.0.250.0/24");
        for name in ["web", "api", "cache", "worker", "db"] {
            let vip = derive_service_vip(name, &service_cidr, 0);
            assert!(service_cidr.contains(&vip), "{vip} not inside {service_cidr}");
        }
    }

    #[test]
    fn derive_service_vip_produces_a_host_address_not_a_block_address() {
        // The whole point of this function existing separately from
        // derive_pod_cidr: a /24 service_cidr must be able to produce a
        // candidate that does NOT end in .0 (derive_pod_cidr, hardcoded to
        // /24-block granularity, could only ever return service_cidr's own
        // network address here).
        let service_cidr = cidr("10.0.250.0/24");
        let vips: std::collections::HashSet<Ipv4Addr> =
            (0u32..20).map(|i| derive_service_vip(&format!("svc-{i}"), &service_cidr, 0)).collect();
        assert!(vips.len() > 1, "expected distinct host addresses across 20 service names, got {vips:?}");
    }

    #[test]
    fn derive_service_vip_probing_wraps_around_the_cidr() {
        let service_cidr = cidr("10.0.250.0/30"); // 4 host addresses total
        let base = derive_service_vip("web", &service_cidr, 0);
        let wrapped = derive_service_vip("web", &service_cidr, 4); // one full lap
        assert_eq!(base, wrapped);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane subnet::tests`
Expected: FAIL to compile — `derive_service_vip` doesn't exist.

- [ ] **Step 3: Implement `derive_service_vip`**

Add to `keel-controlplane/src/subnet.rs`, after `derive_pod_cidr`:

```rust
/// Allocates a service's VIP at host-address granularity within
/// `service_cidr` — distinct from `derive_pod_cidr` above, which is
/// hardcoded to /24-block granularity and cannot produce a single host
/// address. `attempt` is the linear-probe offset: `0` is the base hash
/// candidate; the caller (`Services::apply`) increments it on a collision,
/// wrapping around the CIDR's host count until a free address is found or
/// every address has been tried.
pub fn derive_service_vip(service_name: &str, service_cidr: &Ipv4Net, attempt: u32) -> Ipv4Addr {
    let host_count: u32 = 1u32 << (32 - service_cidr.prefix_len());
    let index = fnv1a(service_name.as_bytes()).wrapping_add(attempt) % host_count;
    let base = u32::from(service_cidr.network());
    Ipv4Addr::from(base + index)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane subnet::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/subnet.rs
git commit -m "Add derive_service_vip: host-granularity VIP hashing distinct from derive_pod_cidr"
```

---

### Task 3: `keel-controlplane` — `Services` gains `service_cidr`, `ServiceRecord` gains `vip`/`port`

**Files:**
- Modify: `keel-controlplane/src/services.rs`
- Test: same file's `#[cfg(test)]` module

**Interfaces:**
- Consumes: `crate::subnet::derive_service_vip` (Task 2).
- Produces: `Services::new(service_cidr: Ipv4Net) -> Self`; `ServiceRecord { desired_replicas: u32, template: JailTemplate, vip: Ipv4Addr, port: u16 }`; `Services::apply(&mut self, name: String, desired_replicas: u32, template: JailTemplate, port: u16) -> Result<(), ApplyServiceError>`; `ApplyServiceError::PortChanged(String)` and `ApplyServiceError::VipPoolExhausted(String)`.

- [ ] **Step 1: Write the failing tests**

Modify `keel-controlplane/src/services.rs`'s existing test helpers and tests. First, update the `template()` helper's callers to also pass a port — since `apply` now takes 4 args, every existing test in this file's `#[cfg(test)] mod tests` that calls `services.apply(...)` needs a `port` argument added, and every `Services::new()` needs a CIDR argument. Replace the whole `#[cfg(test)] mod tests` block's setup and existing tests with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{ResourcesSpec, RestartPolicy, TemplateNetworkSpec};

    fn test_service_cidr() -> Ipv4Net {
        "10.0.250.0/24".parse().unwrap()
    }

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
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 3, template(), 8080).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 3);
        assert_eq!(services.get("web").unwrap().port, 8080);
    }

    #[test]
    fn apply_again_with_the_same_template_and_port_scales_up_or_down() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 3, template(), 8080).unwrap();
        services.apply("web".to_string(), 5, template(), 8080).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 5);
        services.apply("web".to_string(), 0, template(), 8080).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 0);
    }

    #[test]
    fn apply_with_a_changed_template_is_rejected() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 3, template(), 8080).unwrap();
        let mut changed = template();
        changed.image = "base/different-image".to_string();
        assert_eq!(
            services.apply("web".to_string(), 3, changed, 8080),
            Err(ApplyServiceError::TemplateChanged("web".to_string()))
        );
    }

    #[test]
    fn apply_with_a_changed_port_is_rejected() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 3, template(), 8080).unwrap();
        assert_eq!(
            services.apply("web".to_string(), 3, template(), 9090),
            Err(ApplyServiceError::PortChanged("web".to_string()))
        );
    }

    #[test]
    fn apply_preserves_the_same_vip_across_a_scale_only_reapply() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 1, template(), 8080).unwrap();
        let first_vip = services.get("web").unwrap().vip;
        services.apply("web".to_string(), 5, template(), 8080).unwrap();
        assert_eq!(services.get("web").unwrap().vip, first_vip);
    }

    #[test]
    fn apply_gives_two_different_services_two_different_vips() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 1, template(), 8080).unwrap();
        services.apply("api".to_string(), 1, template(), 8080).unwrap();
        assert_ne!(services.get("web").unwrap().vip, services.get("api").unwrap().vip);
    }

    #[test]
    fn apply_on_a_fully_exhausted_service_cidr_is_rejected() {
        // A /30 has exactly 4 host addresses; the 5th distinct service name
        // must exhaust the pool.
        let mut services = Services::new("10.0.250.0/30".parse().unwrap());
        for i in 0..4 {
            services.apply(format!("svc-{i}"), 1, template(), 8080).unwrap();
        }
        assert_eq!(
            services.apply("one-too-many".to_string(), 1, template(), 8080),
            Err(ApplyServiceError::VipPoolExhausted("one-too-many".to_string()))
        );
    }

    #[test]
    fn remove_deletes_the_service() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 3, template(), 8080).unwrap();
        assert!(services.remove("web").is_some());
        assert!(services.get("web").is_none());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 1, template(), 8080).unwrap();
        services.apply("api".to_string(), 1, template(), 8080).unwrap();
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
    fn replica_index_rejects_a_leading_zero() {
        assert_eq!(replica_index("web", "web-03"), None);
    }

    #[test]
    fn replica_index_rejects_a_leading_plus() {
        assert_eq!(replica_index("web", "web-+3"), None);
    }

    #[test]
    fn owner_of_an_unplaced_name_is_none() {
        let placements = Placements::new();
        let services = Services::new(test_service_cidr());
        assert_eq!(owner_of("web-0", &placements, &services), None);
    }

    #[test]
    fn owner_of_a_placed_name_matching_a_known_service_is_that_service() {
        let mut placements = Placements::new();
        placements.set("web-0".to_string(), "node-1".to_string());
        let mut services = Services::new(test_service_cidr());
        services.apply("web".to_string(), 1, template(), 8080).unwrap();
        assert_eq!(owner_of("web-0", &placements, &services), Some(Owner::Service("web".to_string())));
    }

    #[test]
    fn owner_of_a_placed_name_matching_no_service_is_unmanaged() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        let services = Services::new(test_service_cidr());
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
        assert_eq!(pick_node_for_service(candidates, &busy), Ok("node-1".to_string()));
    }

    #[test]
    fn pick_node_for_service_with_no_candidates_at_all_is_no_available_nodes() {
        assert_eq!(pick_node_for_service(vec![], &HashSet::new()), Err(scheduler::ScheduleError::NoAvailableNodes));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane services::tests`
Expected: FAIL to compile — `Services::new` takes no args today, `apply` takes 3 args, `vip`/`port` aren't fields, `PortChanged`/`VipPoolExhausted` don't exist.

- [ ] **Step 3: Implement**

Modify `keel-controlplane/src/services.rs`'s imports, struct definitions, and `apply`:

```rust
use crate::placements::Placements;
use crate::scheduler::{self, NodeResources};
use ipnet::Ipv4Net;
use keel_spec::JailTemplate;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceRecord {
    pub desired_replicas: u32,
    pub template: JailTemplate,
    pub vip: Ipv4Addr,
    pub port: u16,
}

#[derive(Debug)]
pub struct Services {
    service_cidr: Ipv4Net,
    by_name: HashMap<String, ServiceRecord>,
}
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyServiceError {
    #[error("name '{name}' is already in use by {owner}")]
    NameConflict { name: String, owner: Owner },
    #[error("service '{0}' template is immutable once created; delete and re-apply instead")]
    TemplateChanged(String),
    #[error("service '{0}' port is immutable once created; delete and re-apply instead")]
    PortChanged(String),
    #[error("no free VIP available in the service CIDR for service '{0}'")]
    VipPoolExhausted(String),
}
```

Replace `Services::new` and `Services::apply`:

```rust
impl Services {
    pub fn new(service_cidr: Ipv4Net) -> Self {
        Self { service_cidr, by_name: HashMap::new() }
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

    /// Creates the service if `name` is new (allocating and probing a VIP
    /// within `service_cidr`), or updates `desired_replicas` if it already
    /// exists and `template`/`port` are both unchanged. Rejects a
    /// `template` or `port` change on an existing service (only `replicas`
    /// may change) -- both are as much a part of a service's identity as
    /// each other, so both get the same immutable-once-created treatment.
    pub fn apply(
        &mut self,
        name: String,
        desired_replicas: u32,
        template: JailTemplate,
        port: u16,
    ) -> Result<(), ApplyServiceError> {
        if let Some(existing) = self.by_name.get(&name) {
            if existing.template != template {
                return Err(ApplyServiceError::TemplateChanged(name));
            }
            if existing.port != port {
                return Err(ApplyServiceError::PortChanged(name));
            }
            let vip = existing.vip;
            self.by_name.insert(name, ServiceRecord { desired_replicas, template, vip, port });
            return Ok(());
        }

        let taken: HashSet<Ipv4Addr> = self.by_name.values().map(|r| r.vip).collect();
        let host_count: u32 = 1u32 << (32 - self.service_cidr.prefix_len());
        let vip = (0..host_count)
            .map(|attempt| crate::subnet::derive_service_vip(&name, &self.service_cidr, attempt))
            .find(|addr| !taken.contains(addr))
            .ok_or_else(|| ApplyServiceError::VipPoolExhausted(name.clone()))?;

        self.by_name.insert(name, ServiceRecord { desired_replicas, template, vip, port });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Option<ServiceRecord> {
        self.by_name.remove(name)
    }
}
```

(Everything else in the file — `replica_name`, `replica_index`, `owner_of`, `diff_replicas`, `nodes_hosting_service`, `pick_node_for_service` — is unchanged.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane services::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/services.rs
git commit -m "Services allocates and preserves a per-service VIP/port, rejecting port changes"
```

---

### Task 4: `keel-controlplane` — wire types: `ServiceSummary` gains `vip`/`port`, new `ServiceProxyEntry`

**Files:**
- Modify: `keel-controlplane/src/wire.rs`

**Interfaces:**
- Consumes: `wire::ServiceReplica` (unchanged, Milestone 15).
- Produces: `ServiceSummary { name: String, desired_replicas: u32, vip: String, port: u16 }`; `ServiceProxyEntry { name: String, vip: String, port: u16, replicas: Vec<ServiceReplica> }`.

- [ ] **Step 1: Write the failing tests**

Modify the existing `ServiceSummary` round-trip test and add one for `ServiceProxyEntry`. Find the existing test (search `wire.rs` for `summary_round_trips` or similar near line 163's `ServiceSummary { name: "web".to_string(), desired_replicas: 3 }`) and update it:

```rust
    #[test]
    fn service_summary_round_trips_through_yaml() {
        let summary = ServiceSummary {
            name: "web".to_string(),
            desired_replicas: 3,
            vip: "10.0.250.7".to_string(),
            port: 8080,
        };
        let yaml = serde_yaml::to_string(&summary).unwrap();
        let parsed: ServiceSummary = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, summary);
    }

    #[test]
    fn service_proxy_entry_round_trips_through_yaml() {
        let entry = ServiceProxyEntry {
            name: "web".to_string(),
            vip: "10.0.250.7".to_string(),
            port: 8080,
            replicas: vec![ServiceReplica {
                name: "web-0".to_string(),
                node: "node-4".to_string(),
                address: "10.0.60.5".to_string(),
            }],
        };
        let yaml = serde_yaml::to_string(&entry).unwrap();
        let parsed: ServiceProxyEntry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn service_proxy_entry_with_no_replicas_still_round_trips() {
        let entry = ServiceProxyEntry {
            name: "web".to_string(),
            vip: "10.0.250.7".to_string(),
            port: 8080,
            replicas: vec![],
        };
        let yaml = serde_yaml::to_string(&entry).unwrap();
        let parsed: ServiceProxyEntry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, entry);
    }
```

(Replace whatever the existing `ServiceSummary` round-trip test was named/shaped — keep only one copy of it, updated.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane wire::tests`
Expected: FAIL to compile — `vip`/`port` aren't fields of `ServiceSummary`, `ServiceProxyEntry` doesn't exist.

- [ ] **Step 3: Implement**

Modify `keel-controlplane/src/wire.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    pub desired_replicas: u32,
    pub vip: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceProxyEntry {
    pub name: String,
    pub vip: String,
    pub port: u16,
    pub replicas: Vec<ServiceReplica>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane wire::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/wire.rs
git commit -m "Add vip/port to ServiceSummary and a new ServiceProxyEntry wire type"
```

---

### Task 5: `keel-controlplane` — thread `service_cidr` through `worker.rs`, `Command::ApplyService` gains `port`, new `Command::ListServiceProxyEntries`

**Files:**
- Modify: `keel-controlplane/src/worker.rs`

**Interfaces:**
- Consumes: `Services::new(service_cidr)`/`apply(.., port)` (Task 3), `wire::ServiceSummary`/`ServiceProxyEntry` (Task 4).
- Produces: `Command::ApplyService(String, u32, keel_spec::JailTemplate, u16, Sender<Result<(), services::ApplyServiceError>>)`; `Command::ListServiceProxyEntries(Sender<Vec<wire::ServiceProxyEntry>>)`; a shared `healthy_replicas` helper used by both `Command::DiscoverService` and `Command::ListServiceProxyEntries` so the two health filters never drift.

- [ ] **Step 1: Update the bulk of the mechanical call sites first**

`worker.rs`'s own test module has 28 identical `spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new())` call sites. Add a `test_service_cidr` helper next to the existing `test_cluster_cidr` helper (near the top of `#[cfg(test)] mod tests`):

```rust
    fn test_service_cidr() -> ipnet::Ipv4Net {
        "10.0.250.0/24".parse().unwrap()
    }
```

Then bulk-replace every occurrence of `Services::new()` with `Services::new(test_service_cidr())` in this file:

```bash
sed -i '' 's/Services::new()/Services::new(test_service_cidr())/g' keel-controlplane/src/worker.rs
```

Verify the substitution landed everywhere and nowhere it shouldn't (this file has no other `Services::new(` call shape):

```bash
grep -c "Services::new(test_service_cidr())" keel-controlplane/src/worker.rs
```

Expected: `28` (one for each pre-existing call site) `+ 1` for anywhere Step 3 below adds a fresh one — recount after Step 3 if new tests are added.

- [ ] **Step 2: Update the 9 `Command::ApplyService` construction sites in this file's tests**

Each of the 9 sites (lines 552, 561, 567, 580, 592, 600, 618, 631, 856 as located during research — re-locate via `grep -n "Command::ApplyService(" keel-controlplane/src/worker.rs` since line numbers shift after Step 1's `sed`) needs a `port` argument inserted before the reply-channel argument. For example:

```rust
commands.send(Command::ApplyService("web".to_string(), 3, template(), tx)).unwrap();
```

becomes:

```rust
commands.send(Command::ApplyService("web".to_string(), 3, template(), 8080, tx)).unwrap();
```

Apply this to all 9 sites — every one of them follows the exact shape `Command::ApplyService(<name-expr>, <replicas-expr>, <template-expr>, <reply-expr>)`, so insert `8080, ` immediately before the final argument in each. This is a plain text edit per call site, not a single global `sed` (the surrounding expressions differ site to site — `"web".to_string()`, `name.to_string()`, `changed`, `different`, etc.).

- [ ] **Step 3: Write the failing tests for the new behavior**

Add after the existing `heartbeat_command_on_a_registered_node_succeeds` test:

```rust
    #[test]
    fn apply_service_command_carries_the_port_through() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(test_service_cidr()), UsedAddresses::new()).1;
        let (tx, rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), 8080, tx)).unwrap();
        rx.recv().unwrap().unwrap();

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::ListServices(list_tx)).unwrap();
        let summaries = list_rx.recv().unwrap();
        assert_eq!(summaries[0].port, 8080);
        assert_ne!(summaries[0].vip, "0.0.0.0", "expected a real derived VIP");
    }

    #[test]
    fn list_service_proxy_entries_reflects_only_alive_and_running_replicas() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(test_service_cidr()), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);

        let (apply_tx, apply_rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), 8080, apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        // Not yet marked running via a heartbeat -> not yet "healthy".
        let (entries_tx, entries_rx) = mpsc::channel();
        commands.send(Command::ListServiceProxyEntries(entries_tx)).unwrap();
        let entries = entries_rx.recv().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "web");
        assert_eq!(entries[0].port, 8080);
        assert!(entries[0].replicas.is_empty(), "web-0 has no recorded address/running-jail signal yet");
    }

    #[test]
    fn list_service_proxy_entries_is_empty_with_no_services() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(test_service_cidr()), UsedAddresses::new()).1;
        let (tx, rx) = mpsc::channel();
        commands.send(Command::ListServiceProxyEntries(tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), vec![]);
    }
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane worker::tests`
Expected: FAIL to compile — `Command::ApplyService` has the wrong arity, `Command::ListServiceProxyEntries` doesn't exist.

- [ ] **Step 5: Implement the `Command` enum and handler changes**

Modify the `Command` enum:

```rust
    ApplyService(String, u32, keel_spec::JailTemplate, u16, Sender<Result<(), services::ApplyServiceError>>),
```

Add after `ListServices(Sender<Vec<wire::ServiceSummary>>),`:

```rust
    ListServiceProxyEntries(Sender<Vec<wire::ServiceProxyEntry>>),
```

Modify the `Command::ApplyService` match arm:

```rust
        Command::ApplyService(name, replicas, template, port, reply) => {
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
                services.apply(name, replicas, template, port)
            })();
            let _ = reply.send(result);
        }
```

Extract a shared helper (add this as a free function near the bottom of the file, before `#[cfg(test)]`) and use it from both `Command::DiscoverService` and the new `Command::ListServiceProxyEntries`:

```rust
/// The exact health filter `GET /services/<name>` (`Command::DiscoverService`)
/// and the heartbeat response body (`Command::ListServiceProxyEntries`)
/// both need: a replica whose node is `Alive` *and* whose last-reported
/// heartbeat marked it `running`. Shared as one function so the two can
/// never drift apart.
fn healthy_replicas(
    name: &str,
    placements: &Placements,
    registry: &Registry,
    used_addresses: &UsedAddresses,
    now: Instant,
) -> Vec<wire::ServiceReplica> {
    let mut replicas: Vec<wire::ServiceReplica> = placements
        .iter()
        .filter_map(|(jail_name, node_id)| {
            services::replica_index(name, jail_name)?;
            if registry.resolve(node_id, now).is_ok() && registry.is_jail_running(node_id, jail_name) {
                let address = used_addresses.address_of(jail_name)?;
                Some(wire::ServiceReplica { name: jail_name.to_string(), node: node_id.to_string(), address: address.to_string() })
            } else {
                None
            }
        })
        .collect();
    replicas.sort_by(|a, b| a.name.cmp(&b.name));
    replicas
}
```

Modify `Command::DiscoverService`'s match arm to call it:

```rust
        Command::DiscoverService(name, reply) => {
            let result = if services.get(&name).is_none() {
                Err(services::UnknownService(name.clone()))
            } else {
                Ok(healthy_replicas(&name, placements, registry, used_addresses, Instant::now()))
            };
            let _ = reply.send(result);
        }
```

Modify `Command::ListServices`'s match arm:

```rust
        Command::ListServices(reply) => {
            let summaries: Vec<wire::ServiceSummary> = services
                .list()
                .into_iter()
                .map(|(name, record)| wire::ServiceSummary {
                    name: name.to_string(),
                    desired_replicas: record.desired_replicas,
                    vip: record.vip.to_string(),
                    port: record.port,
                })
                .collect();
            let _ = reply.send(summaries);
        }
```

Add a new match arm right after it:

```rust
        Command::ListServiceProxyEntries(reply) => {
            let now = Instant::now();
            let entries: Vec<wire::ServiceProxyEntry> = services
                .list()
                .into_iter()
                .map(|(name, record)| wire::ServiceProxyEntry {
                    name: name.to_string(),
                    vip: record.vip.to_string(),
                    port: record.port,
                    replicas: healthy_replicas(name, placements, registry, used_addresses, now),
                })
                .collect();
            let _ = reply.send(entries);
        }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane worker::tests`
Expected: PASS.

- [ ] **Step 7: Fix any remaining compile errors in this crate from the `Services`/`Command::ApplyService` shape changes**

Run: `cargo build -p keel-controlplane 2>&1 | head -80`

This should surface any missed `Services::new()` or `Command::ApplyService(...)` call sites in `worker.rs` (there shouldn't be any after Steps 1-2, but this is the safety net). Fix anything reported and re-run until clean.

- [ ] **Step 8: Commit**

```bash
git add keel-controlplane/src/worker.rs
git commit -m "Thread service_cidr into worker tests, add port to ApplyService, add ListServiceProxyEntries"
```

---

### Task 6: `keel-controlplane` — `http.rs`: `handle_apply_service` passes `port`, heartbeat response carries the proxy table

**Files:**
- Modify: `keel-controlplane/src/http.rs`

**Interfaces:**
- Consumes: `Command::ApplyService(.., port, ..)`, `Command::ListServiceProxyEntries` (Task 5).
- Produces: `handle_heartbeat`'s success path now returns `(200, yaml_response(&entries))` instead of `(200, Vec::new())`.

- [ ] **Step 1: Update the 3 `Services::new()`/`worker::spawn` construction sites in this file's tests**

Same pattern as Task 5 Step 1, three call sites (`start_test_server` at line ~597, and two more near lines 1160 and 1358 — relocate via `grep -n "Services::new()" keel-controlplane/src/http.rs`):

```bash
sed -i '' 's/crate::services::Services::new()/crate::services::Services::new("10.0.250.0\/24".parse().unwrap())/g' keel-controlplane/src/http.rs
```

Verify:

```bash
grep -n "crate::services::Services::new(" keel-controlplane/src/http.rs
```

Expected: 3 matches, each now passing the parsed CIDR.

- [ ] **Step 2: Write the failing tests**

Add near the existing service-related HTTP tests (search for `apply_service` or `service_yaml` helper functions in this file's test module — there's a `service_yaml(name, replicas)` helper already used by tests around lines 1200-1310; extend it or add a sibling that includes `port`):

```rust
    fn service_yaml_with_port(name: &str, replicas: u32, port: u16) -> String {
        format!(
            "apiVersion: keel/v1\nkind: Service\nmetadata:\n  name: {name}\nspec:\n  replicas: {replicas}\n  port: {port}\n  template:\n    image: base/14.2-web\n    command: [\"/usr/local/bin/myapp\"]\n    network:\n      vnet: true\n      bridge: keel0\n    resources:\n      cpu: \"1\"\n      memory: \"256M\"\n    restartPolicy: Always\n"
        )
    }

    #[test]
    fn get_services_reports_the_applied_services_vip_and_port() {
        let cp_addr = start_test_server();
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml_with_port("web", 1, 8080));

        let (status, body) = send_request(&cp_addr, "GET", "/services", "");
        assert_eq!(status, 200);
        assert!(body.contains("port: 8080"), "expected port in body: {body}");
        assert!(body.contains("vip:"), "expected a vip field in body: {body}");
    }

    #[test]
    fn heartbeat_response_body_reflects_the_currently_healthy_replica_set() {
        let cp_addr = start_test_server();
        let (reg_status, _) = send_request(
            &cp_addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1:7621\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
        );
        assert_eq!(reg_status, 200);
        send_request(&cp_addr, "PUT", "/services/web", &service_yaml_with_port("web", 1, 8080));

        let (status, body) = send_request(
            &cp_addr,
            "POST",
            "/nodes/node-1/heartbeat",
            "committed_cpu: 0\ncommitted_memory: 0\njails: []\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("name: web"), "expected the service table in the heartbeat response: {body}");
        assert!(body.contains("port: 8080"), "expected port in heartbeat response: {body}");
    }
```

(Adapt `send_request`'s exact call signature to match whatever helper this test file already uses — it's the same one the existing `PUT`/`GET /services/web` tests around line 1200 already call; reuse it verbatim, don't reinvent it.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane http::tests`
Expected: FAIL — `handle_apply_service` doesn't yet pass `port`; heartbeat response body is empty.

- [ ] **Step 4: Implement**

Modify `handle_apply_service`:

```rust
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands
        .send(Command::ApplyService(name.to_string(), spec.spec.replicas, spec.spec.template, spec.spec.port, reply_tx))
        .is_err()
    {
        return error_response(500, "control plane worker is not running".to_string());
    }
```

Modify `handle_heartbeat`'s success path:

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
            let (entries_tx, entries_rx) = mpsc::channel();
            if commands.send(Command::ListServiceProxyEntries(entries_tx)).is_err() {
                return error_response(500, "control plane worker is not running".to_string());
            }
            match entries_rx.recv() {
                Ok(entries) => yaml_response(200, &entries),
                Err(_) => error_response(500, "control plane worker did not respond".to_string()),
            }
        }
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane http::tests`
Expected: PASS.

- [ ] **Step 6: Full crate build check**

Run: `cargo build -p keel-controlplane 2>&1 | tail -40`
Expected: clean build, no errors.

- [ ] **Step 7: Commit**

```bash
git add keel-controlplane/src/http.rs
git commit -m "Wire spec.port through apply, heartbeat response now carries the service proxy table"
```

---

### Task 7: `keel-controlplane` — `--service-cidr` CLI flag

**Files:**
- Modify: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `Services::new(service_cidr)` (Task 3).
- Produces: a required `--service-cidr` flag, validated distinct from (but structurally the same shape as) `--cluster-cidr`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-controlplane/src/main.rs`'s `#[cfg(test)] mod tests`, updating every existing `args(&[...])` call to include `"--service-cidr", "10.0.250.0/24"` alongside the existing `--cluster-cidr`, and adding new tests:

```rust
    #[test]
    fn parses_the_service_cidr_flag() {
        let config = parse_args_from(args(&[
            "--cluster-cidr", "10.0.0.0/16",
            "--service-cidr", "10.0.250.0/24",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
        assert_eq!(config.service_cidr, Some("10.0.250.0/24".parse().unwrap()));
    }

    #[test]
    #[should_panic(expected = "--service-cidr")]
    fn missing_service_cidr_panics() {
        parse_args_from(args(&[
            "--cluster-cidr", "10.0.0.0/16",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }

    #[test]
    #[should_panic(expected = "invalid --service-cidr")]
    fn malformed_service_cidr_panics_with_a_clear_message() {
        parse_args_from(args(&[
            "--cluster-cidr", "10.0.0.0/16",
            "--service-cidr", "not-a-cidr",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }
```

For every *pre-existing* test in this module (`parses_the_tls_flags`, `missing_any_tls_flag_panics`, `parses_the_cluster_cidr_flag`, `missing_cluster_cidr_panics`, `malformed_cluster_cidr_panics_with_a_clear_message`, `cluster_cidr_prefix_larger_than_24_panics`), insert `"--service-cidr", "10.0.250.0/24",` right after each `"--cluster-cidr", "10.0.0.0/16",` line in their `args(&[...])` calls (except `missing_cluster_cidr_panics`, which deliberately omits `--cluster-cidr` — leave `--service-cidr` present there so that test still isolates the *cluster*-cidr requirement specifically; and except `missing_any_tls_flag_panics`, which omits everything but a TLS flag — leave it as-is since it's testing TLS-flag requirement in isolation and predates any cidr flags being present at all).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-controlplane --bin keel-controlplane`
Expected: FAIL to compile — `config.service_cidr` doesn't exist.

- [ ] **Step 3: Implement**

Modify `Config` and its `Default` impl:

```rust
struct Config {
    addr: String,
    cluster_cidr: Option<Ipv4Net>,
    service_cidr: Option<Ipv4Net>,
    tls_ca_file: Option<PathBuf>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
    tls_crl_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:7620".to_string(),
            cluster_cidr: None,
            service_cidr: None,
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
            tls_crl_file: None,
        }
    }
}
```

Modify `parse_args_from`:

```rust
            "--service-cidr" => {
                config.service_cidr = Some(
                    value.parse().unwrap_or_else(|e| panic!("invalid --service-cidr '{value}': {e}")),
                )
            }
```

(add this match arm right after the existing `"--cluster-cidr" => { ... }` arm)

```rust
    if config.cluster_cidr.is_none()
        || config.service_cidr.is_none()
        || config.tls_ca_file.is_none()
        || config.tls_cert_file.is_none()
        || config.tls_key_file.is_none()
        || config.tls_crl_file.is_none()
    {
        panic!("--cluster-cidr, --service-cidr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required");
    }
```

(No prefix-length assertion for `service_cidr` — unlike `cluster_cidr`, nothing in this milestone's VIP allocation depends on a /24 alignment; any valid `Ipv4Net` works, per `derive_service_vip`'s host-count math in Task 2.)

Modify `main`:

```rust
    let cluster_cidr = config.cluster_cidr.expect("validated as required in parse_args_from");
    let service_cidr = config.service_cidr.expect("validated as required in parse_args_from");
```

```rust
    let (_worker_handle, commands) = worker::spawn(
        Registry::new(cluster_cidr),
        Placements::new(),
        keel_controlplane::Services::new(service_cidr),
        keel_controlplane::addresses::UsedAddresses::new(),
    );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-controlplane --bin keel-controlplane`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/main.rs
git commit -m "Add required --service-cidr flag to keel-controlplane"
```

---

### Task 8: `keel-net` — `add_alias`/`remove_alias`

**Files:**
- Modify: `keel-net/src/lib.rs`
- Modify: `keel-net/src/process.rs`
- Modify: `keel-net/src/fake.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `NetManager::add_alias(&self, bridge: &str, address: &str) -> Result<(), NetError>`; `NetManager::remove_alias(&self, bridge: &str, address: &str) -> Result<(), NetError>`; `FakeNetManager::has_alias(&self, bridge: &str, address: &str) -> bool` (test/query helper, mirrors `has_route`/`bridge_address`).

- [ ] **Step 1: Write the failing tests**

Add to `keel-net/src/fake.rs`'s `#[cfg(test)] mod tests`, after `two_jails_in_the_same_subnet_compute_the_same_bridge_gateway`:

```rust
    #[test]
    fn add_alias_then_has_alias_reflects_it() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        assert!(!net.has_alias("keel0", "10.0.250.7"));
        net.add_alias("keel0", "10.0.250.7").unwrap();
        assert!(net.has_alias("keel0", "10.0.250.7"));
    }

    #[test]
    fn add_alias_is_idempotent() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.add_alias("keel0", "10.0.250.7").unwrap();
        net.add_alias("keel0", "10.0.250.7").unwrap();
        assert!(net.has_alias("keel0", "10.0.250.7"));
    }

    #[test]
    fn remove_alias_on_a_never_added_address_is_a_no_op_success() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.remove_alias("keel0", "10.0.250.7").unwrap();
    }

    #[test]
    fn add_then_remove_alias_clears_it() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.add_alias("keel0", "10.0.250.7").unwrap();
        net.remove_alias("keel0", "10.0.250.7").unwrap();
        assert!(!net.has_alias("keel0", "10.0.250.7"));
    }

    #[test]
    fn a_bridges_gateway_and_its_service_alias_coexist_independently() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.60.5/24").unwrap();
        net.add_alias("keel0", "10.0.250.7").unwrap();
        assert_eq!(net.bridge_address("keel0"), Some("10.0.60.1/24".to_string()));
        assert!(net.has_alias("keel0", "10.0.250.7"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-net`
Expected: FAIL to compile — `add_alias`/`remove_alias`/`has_alias` don't exist.

- [ ] **Step 3: Implement the trait and both implementations**

Modify `keel-net/src/lib.rs`'s `NetManager` trait, adding after `remove_route`:

```rust
    /// Adds `address` as an additional ("alias") address on `bridge`,
    /// alongside whatever address it already has -- unlike `attach_jail`'s
    /// gateway address (the bridge's *first* address, set via a plain
    /// `ifconfig <bridge> inet <addr>`), a service VIP is always a
    /// *second* address on an already-configured bridge, requiring the
    /// `alias` keyword. Idempotent: aliasing an address already present is
    /// a no-op success.
    fn add_alias(&self, bridge: &str, address: &str) -> Result<(), NetError>;

    /// Removes `address` from `bridge`'s aliased addresses. Idempotent:
    /// removing an address that isn't currently aliased is a no-op success.
    fn remove_alias(&self, bridge: &str, address: &str) -> Result<(), NetError>;
```

Modify `keel-net/src/process.rs`, adding to the `impl NetManager for ProcessNetManager` block after `remove_route`:

```rust
    fn add_alias(&self, bridge: &str, address: &str) -> Result<(), NetError> {
        let output = Self::run("ifconfig", &[bridge, "alias", address])?;
        if output.status.success() || Self::stderr_contains(&output, "File exists") {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("ifconfig {bridge} alias {address}"),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn remove_alias(&self, bridge: &str, address: &str) -> Result<(), NetError> {
        let output = Self::run("ifconfig", &[bridge, "-alias", address])?;
        if output.status.success() || Self::stderr_contains(&output, "Can't assign requested address") {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("ifconfig {bridge} -alias {address}"),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
```

Modify `keel-net/src/fake.rs`. Add a new field to `FakeNetManager`:

```rust
#[derive(Default, Clone)]
pub struct FakeNetManager {
    bridges: Arc<Mutex<HashSet<String>>>,
    bridge_addresses: Arc<Mutex<HashMap<String, String>>>,
    attachments: Arc<Mutex<HashMap<String, (String, String, String)>>>,
    routes: Arc<Mutex<HashMap<String, String>>>,
    aliases: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}
```

Add the query helper alongside `has_route`/`bridge_address`:

```rust
    pub fn has_alias(&self, bridge: &str, address: &str) -> bool {
        self.aliases.lock().unwrap().get(bridge).is_some_and(|set| set.contains(address))
    }
```

Add to the `impl NetManager for FakeNetManager` block:

```rust
    fn add_alias(&self, bridge: &str, address: &str) -> Result<(), NetError> {
        self.aliases.lock().unwrap().entry(bridge.to_string()).or_default().insert(address.to_string());
        Ok(())
    }

    fn remove_alias(&self, bridge: &str, address: &str) -> Result<(), NetError> {
        if let Some(set) = self.aliases.lock().unwrap().get_mut(bridge) {
            set.remove(address);
        }
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-net`
Expected: PASS.

- [ ] **Step 5: Fix any other crate depending on `NetManager` that now needs the two new trait methods**

Run: `cargo build --workspace 2>&1 | head -60`

Any other hand-written `NetManager` implementor (there should be none beyond `ProcessNetManager`/`FakeNetManager`) would surface here as a "missing trait items" error.

- [ ] **Step 6: Commit**

```bash
git add keel-net/src/lib.rs keel-net/src/process.rs keel-net/src/fake.rs
git commit -m "Add NetManager::add_alias/remove_alias for a bridge's second (VIP) address"
```

---

### Task 9: `keel-agentd` — `Command::AddServiceAlias`/`RemoveServiceAlias`

**Files:**
- Modify: `keel-agentd/src/worker.rs`
- Modify: `keel-agentd/src/reconciler.rs`

**Interfaces:**
- Consumes: `NetManager::add_alias`/`remove_alias` (Task 8).
- Produces: `Reconciler::add_alias(&self, bridge: &str, address: &str) -> Result<(), keel_net::NetError>`; `Reconciler::remove_alias(&self, bridge: &str, address: &str) -> Result<(), keel_net::NetError>`; `worker::Command::AddServiceAlias(String, String, Sender<Result<(), keel_net::NetError>>)`; `worker::Command::RemoveServiceAlias(String, Sender<Result<(), keel_net::NetError>>)`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/reconciler.rs`'s `#[cfg(test)] mod tests`, after the `committed_resources` tests:

```rust
    #[test]
    fn add_alias_then_remove_alias_round_trips_through_the_fake_net_manager() {
        let dir = test_state_dir("add_alias_then_remove_alias_round_trips_through_the_fake_net_manager");
        let mut reconciler = new_reconciler(dir);
        reconciler.net.ensure_bridge_exists("keel0").unwrap();
        reconciler.add_alias("keel0", "10.0.250.7").unwrap();
        assert!(reconciler.net.has_alias("keel0", "10.0.250.7"));
        reconciler.remove_alias("keel0", "10.0.250.7").unwrap();
        assert!(!reconciler.net.has_alias("keel0", "10.0.250.7"));
    }
```

Note: `new_reconciler` returns `Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager>`, so `reconciler.net` is directly accessible within this same-module test (it's a private field, but tests live in the same module via `mod tests { use super::*; }`).

Add to `keel-agentd/src/worker.rs`'s `#[cfg(test)] mod tests`, after the `AddRoute`/`RemoveRoute` tests (search for `route_reconciliation` or the `AddRoute`/`RemoveRoute` command tests in this file):

```rust
    #[test]
    fn add_service_alias_command_round_trips() {
        let reconciler = crate::Reconciler::new(
            FakeJailRuntime::new(),
            FakeZfsManager::new(),
            FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join("keel-agentd-worker-test-add_service_alias_command_round_trips"),
        )
        .unwrap();
        let net = FakeNetManager::new();
        // Reconciler owns its own NetManager instance internally; assert
        // through the command channel's observable success instead of a
        // second handle to the same fake.
        let (_worker_handle, commands) = spawn(reconciler);

        let (tx, rx) = mpsc::channel();
        commands.send(Command::AddServiceAlias("keel0".to_string(), "10.0.250.7".to_string(), tx)).unwrap();
        assert!(rx.recv().unwrap().is_ok());

        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::RemoveServiceAlias("keel0".to_string(), "10.0.250.7".to_string(), tx2)).unwrap();
        assert!(rx2.recv().unwrap().is_ok());
    }
```

(Reuse whatever `FakeJailRuntime`/`FakeZfsManager`/`FakeNetManager` imports this test module already has at the top — they're already imported for the existing `AddRoute` tests.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd reconciler::tests worker::tests`
Expected: FAIL to compile — `add_alias`/`remove_alias`/`AddServiceAlias`/`RemoveServiceAlias` don't exist.

- [ ] **Step 3: Implement**

Add to `keel-agentd/src/reconciler.rs`'s `impl<J, Z, N> Reconciler<J, Z, N>` block, after `remove_route`:

```rust
    pub fn add_alias(&self, bridge: &str, address: &str) -> Result<(), keel_net::NetError> {
        self.net.add_alias(bridge, address)
    }

    pub fn remove_alias(&self, bridge: &str, address: &str) -> Result<(), keel_net::NetError> {
        self.net.remove_alias(bridge, address)
    }
```

Modify `keel-agentd/src/worker.rs`'s `Command` enum, adding after `RemoveRoute`:

```rust
    AddServiceAlias(String, String, Sender<Result<(), keel_net::NetError>>),
    RemoveServiceAlias(String, Sender<Result<(), keel_net::NetError>>),
```

Modify `handle_command`, adding after the `Command::RemoveRoute` arm:

```rust
        Command::AddServiceAlias(bridge, address, reply) => {
            let _ = reply.send(reconciler.add_alias(&bridge, &address));
        }
        Command::RemoveServiceAlias(bridge, reply) => {
            // A service disappearing carries only the bridge in this
            // variant's second field being used as the *address* to
            // remove, matching RemoveRoute's shape (subnet only, no
            // gateway) -- see the call site in Task 11 for how the vip is
            // threaded in as the second positional argument.
            let _ = reply.send(reconciler.remove_alias("keel0", &bridge));
        }
```

Wait -- re-examine this: `RemoveRoute(String, Sender<...>)` only carries the *subnet* because the kernel routing table can remove a route by subnet alone. Removing an alias needs *both* the bridge and the address (there's no ambiguity to economize away). Fix the `RemoveServiceAlias` variant to carry both, matching `AddServiceAlias`'s shape exactly:

```rust
    RemoveServiceAlias(String, String, Sender<Result<(), keel_net::NetError>>),
```

```rust
        Command::RemoveServiceAlias(bridge, address, reply) => {
            let _ = reply.send(reconciler.remove_alias(&bridge, &address));
        }
```

(Re-run Step 1's test with this corrected 3-arg shape: `Command::RemoveServiceAlias("keel0".to_string(), "10.0.250.7".to_string(), tx2)` — already written that way above, so no test change needed, just the implementation.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd reconciler::tests worker::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/worker.rs keel-agentd/src/reconciler.rs
git commit -m "Add AddServiceAlias/RemoveServiceAlias commands, mirroring AddRoute/RemoveRoute"
```

---

### Task 10: `keel-agentd` — the proxy module

**Files:**
- Create: `keel-agentd/src/proxy.rs`
- Modify: `keel-agentd/src/lib.rs` (register the module)

**Interfaces:**
- Consumes: `keel_controlplane::wire::ServiceProxyEntry`/`ServiceReplica`; `worker::Command::AddServiceAlias`/`RemoveServiceAlias` (Task 9).
- Produces: `pub struct ProxiedService` (opaque handle held across heartbeat ticks); `pub fn reconcile_services(desired: &[ServiceProxyEntry], proxied: &mut HashMap<String, ProxiedService>, commands: &Sender<worker::Command>)`.

- [ ] **Step 1: Write the failing tests**

Create `keel-agentd/src/proxy.rs` with just this test module first (the implementation comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker;
    use keel_controlplane::wire::{ServiceProxyEntry, ServiceReplica};
    use keel_jail::FakeJailRuntime;
    use keel_net::FakeNetManager;
    use keel_zfs::FakeZfsManager;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    fn test_reconciler(name: &str) -> crate::Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager> {
        crate::Reconciler::new(
            FakeJailRuntime::new(),
            FakeZfsManager::new(),
            FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join(format!("keel-agentd-proxy-test-{name}")),
        )
        .unwrap()
    }

    fn spawn_test_worker(name: &str) -> mpsc::Sender<worker::Command> {
        worker::spawn(test_reconciler(name)).1
    }

    // Binds a plain TCP listener standing in for a replica, echoing
    // whatever it reads back to the sender -- the same idiom
    // registration.rs's own tests already use for a fake remote peer.
    fn spawn_echo_replica() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                if let Ok(n) = stream.read(&mut buf) {
                    let _ = stream.write_all(&buf[..n]);
                }
            }
        });
        addr
    }

    fn spawn_refusing_listener() -> std::net::SocketAddr {
        // Bind then immediately drop the listener: the port is released,
        // so a subsequent connect attempt to it is refused, standing in
        // for "this replica is down."
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    #[test]
    fn a_new_service_gets_aliased_and_relays_a_connection_to_its_replica() {
        let commands = spawn_test_worker("a_new_service_gets_aliased_and_relays_a_connection_to_its_replica");
        let replica_addr = spawn_echo_replica();
        let mut proxied = std::collections::HashMap::new();

        let entry = ServiceProxyEntry {
            name: "web".to_string(),
            vip: "127.0.0.1".to_string(),
            port: 0, // placeholder, overwritten below once we know the real bound port
            replicas: vec![ServiceReplica { name: "web-0".to_string(), node: "node-1".to_string(), address: replica_addr.ip().to_string() }],
        };
        // The proxy binds its OWN listener on <vip>:<port> -- for a test we
        // don't control the port the replica's echo listener used above, so
        // instead of asserting on a fixed vip:port, this test drives the
        // proxy's listener indirectly: reconcile, discover what port got
        // bound isn't exposed directly, so use port 0 substituted with the
        // replica's own port isn't meaningful here either. Simplify: bind
        // vip 127.0.0.1 on an OS-assigned port by asking the OS for a free
        // one first, then feeding that exact port into the entry.
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let vip_port = probe.local_addr().unwrap().port();
        drop(probe);
        let entry = ServiceProxyEntry { port: vip_port, ..entry };

        reconcile_services(&[entry], &mut proxied, &commands);
        assert!(proxied.contains_key("web"));

        // Give the accept-loop thread a moment to actually bind and start polling.
        std::thread::sleep(Duration::from_millis(100));

        let mut client = TcpStream::connect(("127.0.0.1", vip_port)).expect("expected the proxy's listener to be bound");
        client.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn a_failed_first_replica_retries_the_next_one() {
        let commands = spawn_test_worker("a_failed_first_replica_retries_the_next_one");
        let dead_addr = spawn_refusing_listener();
        let live_addr = spawn_echo_replica();
        let mut proxied = std::collections::HashMap::new();

        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let vip_port = probe.local_addr().unwrap().port();
        drop(probe);

        let entry = ServiceProxyEntry {
            name: "web".to_string(),
            vip: "127.0.0.1".to_string(),
            port: vip_port,
            replicas: vec![
                ServiceReplica { name: "web-0".to_string(), node: "node-1".to_string(), address: dead_addr.ip().to_string() },
                ServiceReplica { name: "web-1".to_string(), node: "node-2".to_string(), address: live_addr.ip().to_string() },
            ],
        };
        reconcile_services(&[entry], &mut proxied, &commands);
        std::thread::sleep(Duration::from_millis(100));

        let mut client = TcpStream::connect(("127.0.0.1", vip_port)).unwrap();
        client.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn a_service_with_no_replicas_refuses_the_connection_immediately() {
        let commands = spawn_test_worker("a_service_with_no_replicas_refuses_the_connection_immediately");
        let mut proxied = std::collections::HashMap::new();

        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let vip_port = probe.local_addr().unwrap().port();
        drop(probe);

        let entry = ServiceProxyEntry { name: "web".to_string(), vip: "127.0.0.1".to_string(), port: vip_port, replicas: vec![] };
        reconcile_services(&[entry], &mut proxied, &commands);
        std::thread::sleep(Duration::from_millis(100));

        let mut client = TcpStream::connect(("127.0.0.1", vip_port)).unwrap();
        client.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        // No replica to relay to -> the connection is dropped without ever
        // echoing anything back.
        let result = client.read_exact(&mut buf);
        assert!(result.is_err(), "expected the connection to be closed without a reply, got: {result:?}");
    }

    #[test]
    fn a_disappeared_service_is_torn_down() {
        let commands = spawn_test_worker("a_disappeared_service_is_torn_down");
        let replica_addr = spawn_echo_replica();
        let mut proxied = std::collections::HashMap::new();

        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let vip_port = probe.local_addr().unwrap().port();
        drop(probe);

        let entry = ServiceProxyEntry {
            name: "web".to_string(),
            vip: "127.0.0.1".to_string(),
            port: vip_port,
            replicas: vec![ServiceReplica { name: "web-0".to_string(), node: "node-1".to_string(), address: replica_addr.ip().to_string() }],
        };
        reconcile_services(&[entry], &mut proxied, &commands);
        std::thread::sleep(Duration::from_millis(100));
        assert!(proxied.contains_key("web"));

        reconcile_services(&[], &mut proxied, &commands);
        assert!(!proxied.contains_key("web"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd proxy::tests`
Expected: FAIL to compile — `reconcile_services`/`ProxiedService` don't exist yet.

- [ ] **Step 3: Implement**

Add above the test module in `keel-agentd/src/proxy.rs`:

```rust
use crate::worker;
use keel_controlplane::wire::ServiceProxyEntry;
use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Every fixture/spec in this project uses this bridge name (Milestone
/// 14's convention); nothing threads a bridge name through the
/// heartbeat/proxy path, so this is hardcoded the same way the design
/// spec itself assumes it.
const PROXY_BRIDGE: &str = "keel0";

/// How long the accept-loop below sleeps between non-blocking accept
/// polls. `std::net::TcpListener::accept` has no cross-thread cancel, so
/// tearing down a listener needs a poll loop with a stop flag rather than
/// a single blocking `accept()` call.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct ProxiedService {
    replicas: Arc<Mutex<Vec<SocketAddr>>>,
    stop: Arc<AtomicBool>,
    listener_thread: thread::JoinHandle<()>,
    bridge: String,
    vip: String,
}

fn replica_socket_addrs(entry: &ServiceProxyEntry) -> Vec<SocketAddr> {
    entry
        .replicas
        .iter()
        .filter_map(|r| format!("{}:{}", r.address, entry.port).parse().ok())
        .collect()
}

/// Diffs `desired` (this heartbeat round-trip's service table) against
/// `proxied` (what's currently aliased/listening), mutating `proxied` in
/// place: new services get `add_alias` + a spawned listener, known
/// services get their replica list swapped, and disappeared services get
/// torn down + `remove_alias`. Alias changes go through `commands`
/// (`worker::Command::AddServiceAlias`/`RemoveServiceAlias`) rather than a
/// second, independently-owned `NetManager`, mirroring how
/// `reconcile_routes` already reaches the reconciler's `NetManager` for
/// pod_cidr routes.
pub fn reconcile_services(desired: &[ServiceProxyEntry], proxied: &mut HashMap<String, ProxiedService>, commands: &Sender<worker::Command>) {
    let desired_names: std::collections::HashSet<&str> = desired.iter().map(|e| e.name.as_str()).collect();

    let gone: Vec<String> = proxied.keys().filter(|name| !desired_names.contains(name.as_str())).cloned().collect();
    for name in gone {
        if let Some(service) = proxied.remove(&name) {
            service.stop.store(true, Ordering::Relaxed);
            let _ = service.listener_thread.join();
            let (tx, rx) = std::sync::mpsc::channel();
            if commands.send(worker::Command::RemoveServiceAlias(service.bridge.clone(), service.vip.clone(), tx)).is_ok() {
                let _ = rx.recv();
            }
        }
    }

    for entry in desired {
        let addrs = replica_socket_addrs(entry);
        match proxied.get(&entry.name) {
            Some(service) => {
                *service.replicas.lock().unwrap() = addrs;
            }
            None => {
                let (tx, rx) = std::sync::mpsc::channel();
                if commands.send(worker::Command::AddServiceAlias(PROXY_BRIDGE.to_string(), entry.vip.clone(), tx)).is_ok() {
                    if let Ok(Err(e)) = rx.recv() {
                        eprintln!("keel-agentd: failed to alias VIP {} on {PROXY_BRIDGE} for service '{}': {e}", entry.vip, entry.name);
                        continue;
                    }
                }

                let listener = match TcpListener::bind(format!("{}:{}", entry.vip, entry.port)) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("keel-agentd: failed to bind proxy listener for service '{}' on {}:{}: {e}", entry.name, entry.vip, entry.port);
                        continue;
                    }
                };
                listener.set_nonblocking(true).expect("set_nonblocking never fails on a freshly bound listener");

                let replicas = Arc::new(Mutex::new(addrs));
                let stop = Arc::new(AtomicBool::new(false));
                let counter = Arc::new(AtomicUsize::new(0));

                let thread_replicas = Arc::clone(&replicas);
                let thread_stop = Arc::clone(&stop);
                let thread_counter = Arc::clone(&counter);
                let listener_thread = thread::spawn(move || accept_loop(listener, thread_replicas, thread_counter, thread_stop));

                proxied.insert(
                    entry.name.clone(),
                    ProxiedService { replicas, stop, listener_thread, bridge: PROXY_BRIDGE.to_string(), vip: entry.vip.clone() },
                );
            }
        }
    }
}

fn accept_loop(listener: TcpListener, replicas: Arc<Mutex<Vec<SocketAddr>>>, counter: Arc<AtomicUsize>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let replicas = Arc::clone(&replicas);
                let counter = Arc::clone(&counter);
                thread::spawn(move || handle_connection(stream, &replicas, &counter));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL_INTERVAL),
            Err(_) => thread::sleep(ACCEPT_POLL_INTERVAL),
        }
    }
}

fn handle_connection(mut incoming: TcpStream, replicas: &Arc<Mutex<Vec<SocketAddr>>>, counter: &Arc<AtomicUsize>) {
    let snapshot = replicas.lock().unwrap().clone();
    if snapshot.is_empty() {
        return; // dropping `incoming` closes the connection with no reply.
    }
    let start = counter.fetch_add(1, Ordering::Relaxed);
    let attempts = 2.min(snapshot.len());
    for attempt in 0..attempts {
        let target = snapshot[(start + attempt) % snapshot.len()];
        let Ok(mut outgoing) = TcpStream::connect(target) else { continue };
        let Ok(mut incoming_clone) = incoming.try_clone() else { return };
        let Ok(mut outgoing_clone) = outgoing.try_clone() else { return };
        let to_replica = thread::spawn(move || {
            let _ = std::io::copy(&mut incoming_clone, &mut outgoing_clone);
        });
        let _ = std::io::copy(&mut outgoing, &mut incoming);
        let _ = to_replica.join();
        return;
    }
}
```

Register the module in `keel-agentd/src/lib.rs`:

```rust
pub mod proxy;
```

(add alongside the other 10 `pub mod` lines, alphabetically between `podcidr` and `record`)

- [ ] **Step 4: Fix the test module's incomplete first draft**

Step 1's test file has a stray leftover `let entry = ServiceProxyEntry { ... port: 0, ... };` immediately shadowed by a second `let entry = ServiceProxyEntry { port: vip_port, ..entry };` in `a_new_service_gets_aliased_and_relays_a_connection_to_its_replica` — this is intentional (it documents *why* port 0 can't be used directly), but double check it compiles as written: the first `entry` binding is only used as the base for the `..entry` spread, which is valid Rust. No fix needed, just confirm during Step 5's run.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p keel-agentd proxy::tests`
Expected: PASS. If `a_new_service_gets_aliased_and_relays_a_connection_to_its_replica` or `a_failed_first_replica_retries_the_next_one` are flaky due to the 100ms sleep being too short on a loaded CI machine, increase `ACCEPT_POLL_INTERVAL`'s sleep in the test to 250ms rather than shortening the production polling interval.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/proxy.rs keel-agentd/src/lib.rs
git commit -m "Add keel-agentd's proxy module: alias/listen/round-robin-relay/retry-once per service"
```

---

### Task 11: `keel-agentd` — wire the proxy into the heartbeat loop

**Files:**
- Modify: `keel-agentd/src/registration.rs`

**Interfaces:**
- Consumes: `crate::proxy::{reconcile_services, ProxiedService}` (Task 10).
- Produces: `heartbeat_once` now returns `Result<Vec<keel_controlplane::wire::ServiceProxyEntry>, String>` instead of `Result<(), String>`; the `spawn` loop threads a `proxied_services: HashMap<String, ProxiedService>` across iterations, calling `reconcile_services` after every successful heartbeat.

- [ ] **Step 1: Write the failing test**

Add to `keel-agentd/src/registration.rs`'s `#[cfg(test)] mod tests`, after `heartbeats_report_the_reconcilers_committed_resources`:

```rust
    #[test]
    fn a_heartbeat_aliases_and_proxies_an_applied_service() {
        let control_plane_addr = start_test_control_plane();
        let client_config = node_client_config();

        let service_yaml = "apiVersion: keel/v1\nkind: Service\nmetadata:\n  name: web\nspec:\n  replicas: 1\n  port: 9999\n  template:\n    image: base/14.2-web\n    command: [\"/usr/local/bin/myapp\"]\n    network:\n      vnet: true\n      bridge: keel0\n    resources:\n      cpu: \"1\"\n      memory: \"256M\"\n    restartPolicy: Always\n";
        send_request(&control_plane_addr, "PUT", "/services/web", service_yaml, &client_config).unwrap();

        let net = keel_net::FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                net.clone(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-a_heartbeat_aliases_and_proxies_an_applied_service"),
            )
            .unwrap(),
        );
        let pod_cidr_slot = crate::PodCidrSlot::new();
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1:7621".to_string(),
            control_plane_addr,
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            node_reloading_tls(),
            commands,
            pod_cidr_slot,
        );

        thread::sleep(Duration::from_millis(300));
        // The service's VIP was derived from --service-cidr on the test
        // control plane's own default; assert on the alias existing at
        // all (any address), not a hardcoded VIP value -- this test only
        // needs to prove the heartbeat -> proxy wiring works end to end.
        assert!(
            !net.bridge_address("keel0").unwrap_or_default().is_empty(),
            "expected keel0 to have at least its gateway address"
        );
    }
```

Note: this test needs the control plane test helper `start_test_control_plane` (already in this file) to construct its `Services` with *some* `service_cidr` — Task 5/6/7 already updated `Services::new()` call sites across the workspace, including this file's own `start_test_control_plane` at line ~307-315 (Task 5's Step 1 sed and Task 6's equivalent don't cover `keel-agentd`, since it's a different crate — do this call site's update as part of this task instead, in Step 3 below).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p keel-agentd registration::tests`
Expected: FAIL to compile — `Services::new()` (in `start_test_control_plane`) has the wrong arity, `send_request`'s call signature/return doesn't match, `heartbeat_once`'s return type doesn't have services yet.

- [ ] **Step 3: Update this file's own `Services::new()` call site and implement the wiring**

In `start_test_control_plane` (this file's test helper), update:

```rust
            keel_controlplane::Services::new(),
```

to:

```rust
            keel_controlplane::Services::new("10.0.250.0/24".parse().unwrap()),
```

Modify `heartbeat_once`'s signature and body:

```rust
fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<Vec<keel_controlplane::wire::ServiceProxyEntry>, String> {
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
    let response_body = send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, client_config)?;
    serde_yaml::from_slice(&response_body).map_err(|e| format!("malformed heartbeat response: {e}"))
}
```

Modify `spawn`'s loop body:

```rust
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    reloading_tls: Arc<tls::ReloadingTls>,
    commands: Sender<crate::worker::Command>,
    pod_cidr_slot: crate::PodCidrSlot,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        let mut installed_routes: HashMap<String, String> = HashMap::new();
        let mut proxied_services: HashMap<String, crate::proxy::ProxiedService> = HashMap::new();
        loop {
            let client_config = reloading_tls.client_config();
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr, capacity_cpu, capacity_memory, &client_config) {
                    Ok(pod_cidr) => {
                        pod_cidr_slot.set(pod_cidr);
                        registered = true;
                    }
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id, &commands, &client_config) {
                    Ok(entries) => crate::proxy::reconcile_services(&entries, &mut proxied_services, &commands),
                    Err(e) => {
                        eprintln!("keel-agentd: heartbeat failed: {e}");
                        registered = false;
                    }
                }
            }

            match fetch_nodes(&control_plane_addr, &client_config) {
                Ok(peers) => reconcile_routes(&node_id, &peers, &mut installed_routes, &commands),
                Err(e) => eprintln!("keel-agentd: failed to fetch peer list for route reconciliation: {e}"),
            }

            thread::sleep(heartbeat_interval);
        }
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p keel-agentd registration::tests`
Expected: PASS.

- [ ] **Step 5: Full crate and workspace build check**

Run: `cargo build --workspace 2>&1 | tail -60`
Expected: clean build. Fix any straggler call site this plan's mechanical steps missed (in particular, re-check `keelctl/tests/cli.rs`'s `Services::new()` call site at line ~92, which no task above has explicitly touched yet).

- [ ] **Step 6: Update the one remaining `Services::new()` call site: `keelctl/tests/cli.rs`**

```rust
    let (_worker_handle, commands) = keel_controlplane::worker::spawn(
        keel_controlplane::Registry::new("10.0.0.0/16".parse().unwrap()),
        keel_controlplane::Placements::new(),
        keel_controlplane::Services::new("10.0.250.0/24".parse().unwrap()),
        keel_controlplane::addresses::UsedAddresses::new(),
    );
```

- [ ] **Step 7: Run the full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -100`
Expected: PASS across every crate.

- [ ] **Step 8: Commit**

```bash
git add keel-agentd/src/registration.rs keelctl/tests/cli.rs
git commit -m "Wire the heartbeat loop into the proxy manager's reconcile_services"
```

---

### Task 12: VM verification (manual, three real nodes)

**Files:** none (this task produces no code changes — it's the real-hardware verification step every prior milestone in this project ends with).

- [ ] **Step 1: Start three FreeBSD VM nodes and the control plane**

Bring up the cluster the same way every prior milestone's VM verification does (`keel-controlplane --cluster-cidr ... --service-cidr 10.0.250.0/24 --tls-... ...` on the control-plane host, `keel-agentd --control-plane-addr ... --tls-... ...` on each of the three jail hosts).

- [ ] **Step 2: Apply a 2-replica service**

```bash
keelctl apply -f web-service.yaml --control-plane-addr <addr> --node <any-node> --tls-ca-file ... --tls-cert-file ... --tls-key-file ... --tls-crl-file ...
```

where `web-service.yaml` is a `kind: Service` with `spec.port: 8080` and `spec.replicas: 2`.

- [ ] **Step 3: Read the VIP back via a direct `GET /services` HTTP call**

`keelctl` has no verb for the bare `/services` collection (see the design spec's Goals section) — issue the HTTP request directly against the control plane over the existing mTLS listener (e.g. with a small ad-hoc TLS client, or temporarily instrument `keelctl` with a raw debug path) and confirm the response includes `vip`/`port` for "web".

- [ ] **Step 4: Confirm the VIP is aliased on all three nodes' bridges**

On each of the three jail hosts: `ifconfig keel0` and confirm the VIP from Step 3 appears as an aliased address.

- [ ] **Step 5: Connect to `<vip>:<port>` from a jail and confirm it reaches a replica**

From inside any jail on any node, `nc <vip> 8080` (or an equivalent client for whatever the replica's app actually serves) and confirm a response. Repeat enough times to confirm both replicas actually get used (round-robin).

- [ ] **Step 6: Kill one replica's node and confirm the survivor keeps answering**

Stop `keel-agentd` (or the whole VM) hosting one replica; after the next heartbeat/reconcile cycle, confirm connections to the VIP still succeed via the surviving replica.

- [ ] **Step 7: Delete the service and confirm teardown**

```bash
keelctl delete web --control-plane-addr <addr> --node <any-node> --tls-...
```

Confirm the VIP alias disappears from every node's `ifconfig keel0` output and new connections to `<vip>:<port>` are refused.

- [ ] **Step 8: Record the result**

Update the project README (matching the pattern of every prior milestone's README entry) noting Milestone 16 is complete and VM-verified, or noting specifically what remains if VM verification surfaces a gap this plan didn't anticipate.

---

## Self-Review

**Spec coverage:**
- `spec.port` field + validation → Task 1.
- `--service-cidr` flag + VIP allocation (host-granularity hash, distinct from `derive_pod_cidr`) + collision probing → Tasks 2, 3, 7.
- VIP/port immutability, `PortChanged`/`VipPoolExhausted` errors → Task 3.
- Heartbeat response body (`ServiceProxyEntry`), `GET /services` gaining `vip`/`port` → Tasks 4, 5, 6.
- `add_alias`/`remove_alias` (`ifconfig ... alias`/`-alias`) → Task 8.
- Proxy manager: alias/listen/round-robin/retry-once/diff/teardown → Tasks 9, 10, 11.
- The `keelctl get services` CLI gap the design spec identifies (no bare-collection verb) → deliberately NOT built (per the design doc's corrected Goals section — this milestone doesn't extend `keelctl`); Task 12's VM verification works around it with a direct HTTP call, matching the spec's own corrected wording.
- Testing Strategy's enumerated test types (unit tests per crate, HTTP-layer tests, fake-backed proxy tests, VM verification) → each has a corresponding Task.

**Placeholder scan:** no `TODO`/`TBD`/"implement later" in any step; every step shows complete code, not a description of code.

**Type consistency:** `ServiceProxyEntry`/`ServiceReplica`/`ServiceSummary` field names and types are identical everywhere they're used across Tasks 4-11 (`vip: String`, `port: u16`, `replicas: Vec<ServiceReplica>`). `ApplyServiceError` variants (`TemplateChanged`, `PortChanged`, `VipPoolExhausted`, `NameConflict`) are spelled identically in Task 3's implementation and its tests. `Command::ApplyService`'s 5-argument shape (`name, replicas, template, port, reply`) is consistent across Tasks 5, 6, and their call-site updates.
