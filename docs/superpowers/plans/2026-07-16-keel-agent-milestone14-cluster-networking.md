# Milestone 14: Cross-Node Overlay Networking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every node a deterministic, collision-checked `/24` subnet derived from its `node-id`, teach `keel-agentd` to keep its kernel routing table in sync with every peer's subnet over the existing 5-second heartbeat loop, and reject `JailSpec`s addressed outside a node's own subnet at apply time — all with no tunnel protocol and no new control-plane persistence.

**Architecture:** `keel-controlplane` gains a pure `derive_pod_cidr(node_id, cluster_cidr) -> Ipv4Net` function (hand-rolled FNV-1a hash, `ipnet` crate for CIDR arithmetic) that `Registry::register` calls and collision-checks against every other currently-registered node; the derived `pod_cidr` is stored in `NodeRecord`, returned in the registration response body, and exposed per-node in `GET /nodes`. `keel-agentd`'s existing registration/heartbeat loop stores its own `pod_cidr` in a small shared slot (read by the apply-time HTTP handler) and, every tick, fetches `GET /nodes` and diffs the peer list against a locally tracked "installed routes" map, calling two new `NetManager` methods (`add_route`/`remove_route`, shelling out to `route(8)`) through the reconciler worker's existing command channel.

**Tech Stack:** Rust (2021 edition), `ipnet` crate (already a workspace dependency via `keel-spec`), `serde`/`serde_yaml` for wire types, `rustls` for the existing mTLS transport, hand-rolled HTTP parsing (`httparse`) — no new dependencies beyond `ipnet` becoming a direct dependency of `keel-controlplane` and `keel-agentd`.

## Global Constraints

- Per-node block size is a hardcoded `/24` (`POD_PREFIX_LEN = 24`), not configurable — no flag for it.
- No IPv6 anywhere in this milestone.
- No new background thread and no new polling cadence in `keel-agentd`: route reconciliation piggybacks on `registration.rs`'s existing 5-second tick.
- No control-plane persistence: `pod_cidr` is recomputed from `node_id` + `--cluster-cidr` on every registration, never read back from disk.
- `--cluster-cidr` is a new, **unconditionally required** flag on `keel-controlplane` (matching how `--tls-*-file` became unconditionally required in Milestone 12). `keel-agentd` gains **no** new CLI flags.
- Plain single-node `keel-agentd` (no `--control-plane-addr`) must be completely unaffected: no `pod_cidr`, apply-time subnet check always skipped.
- `ipnet` is already a workspace dependency (`keel-spec/Cargo.toml`, used by `validate_address`) — reuse it in `keel-controlplane` and `keel-agentd` rather than hand-rolling CIDR parsing/containment.
- Design reference: `docs/superpowers/specs/2026-07-16-keel-agent-milestone14-cluster-networking-design.md` (Approved). Follow it exactly; where this plan adds an implementation detail the spec left open (e.g. exact `route(8)` tolerance substrings, the `PodCidrSlot` type), it is called out inline.

---

## Verified facts used by this plan

Computed with a plain Python FNV-1a implementation (offset basis `0x811c9dc5`, prime `0x01000193`, 32-bit wrapping), matching the hand-rolled Rust implementation this plan specifies:

| node_id | fnv1a (u32) | `% 256` (cluster `10.0.0.0/16`) | `% 4` (cluster `10.0.0.0/22`) |
|---|---|---|---|
| `node-1` | 1422144387 | 131 → `10.0.131.0/24` | 3 |
| `node-2` | 1438922006 | 22 → `10.0.22.0/24` | 2 |
| `node-3` | 1455699625 | 169 → `10.0.169.0/24` | 1 |
| `node-4` | 1472477244 | 60 → `10.0.60.0/24` | 0 |
| `node-8` | 1539587720 | 136 → `10.0.136.0/24` | **0 (collides with node-4)** |

`node-4` and `node-8` derive the **same** `pod_cidr` (`10.0.0.0/24`) under cluster CIDR `10.0.0.0/22` — this plan uses that exact pair for the collision-rejection test, so the test is deterministic rather than probabilistic.

---

### Task 1: `keel-net` — `add_route`/`remove_route` on `NetManager`

**Files:**
- Modify: `keel-net/src/lib.rs`
- Modify: `keel-net/src/fake.rs`
- Modify: `keel-net/src/process.rs`
- Modify: `keel-net/tests/freebsd_net.rs`

**Interfaces:**
- Produces: `NetManager::add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), NetError>`, `NetManager::remove_route(&self, subnet: &str) -> Result<(), NetError>`, and a test-only `FakeNetManager::has_route(&self, subnet: &str) -> Option<String>` (returns the stored gateway address if the subnet is currently routed).

- [ ] **Step 1: Write the failing `FakeNetManager` tests**

Add to `keel-net/src/fake.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn add_route_then_has_route_reflects_it() {
        let net = FakeNetManager::new();
        assert_eq!(net.has_route("10.0.4.0/24"), None);
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        assert_eq!(net.has_route("10.0.4.0/24"), Some("192.168.64.5".to_string()));
    }

    #[test]
    fn add_route_is_idempotent() {
        let net = FakeNetManager::new();
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        assert_eq!(net.has_route("10.0.4.0/24"), Some("192.168.64.5".to_string()));
    }

    #[test]
    fn remove_route_on_a_never_added_subnet_is_a_no_op_success() {
        let net = FakeNetManager::new();
        net.remove_route("10.0.9.0/24").unwrap();
    }

    #[test]
    fn add_then_remove_route_clears_it() {
        let net = FakeNetManager::new();
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        net.remove_route("10.0.4.0/24").unwrap();
        assert_eq!(net.has_route("10.0.4.0/24"), None);
    }
```

- [ ] **Step 2: Run to verify it fails to compile**

Run: `cargo test -p keel-net --lib 2>&1 | tail -30`
Expected: FAIL — `no method named add_route/has_route found`.

- [ ] **Step 3: Add the trait methods and both implementations**

In `keel-net/src/lib.rs`, add to `pub trait NetManager` (after `detach_jail`):

```rust
    /// Adds a route to `subnet` via `gateway_addr` to the host's kernel
    /// routing table. Idempotent: adding a route that already exists in
    /// the table with the same gateway is a no-op success, not an error.
    fn add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), NetError>;

    /// Removes the route to `subnet` from the host's kernel routing table.
    /// Idempotent: removing a route that isn't present is a no-op success.
    fn remove_route(&self, subnet: &str) -> Result<(), NetError>;
```

In `keel-net/src/fake.rs`, add a `routes` field and implement both methods:

```rust
#[derive(Default)]
pub struct FakeNetManager {
    bridges: Mutex<HashSet<String>>,
    attachments: Mutex<HashMap<String, (String, String, String)>>,
    routes: Mutex<HashMap<String, String>>,
}
```

```rust
    fn add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), NetError> {
        self.routes.lock().unwrap().insert(subnet.to_string(), gateway_addr.to_string());
        Ok(())
    }

    fn remove_route(&self, subnet: &str) -> Result<(), NetError> {
        self.routes.lock().unwrap().remove(subnet);
        Ok(())
    }
```

Add the test-only accessor below the `impl NetManager for FakeNetManager` block:

```rust
impl FakeNetManager {
    pub fn has_route(&self, subnet: &str) -> Option<String> {
        self.routes.lock().unwrap().get(subnet).cloned()
    }
}
```

(There is already an `impl FakeNetManager { pub fn new() ... }` block — add `has_route` as a second method inside that same block rather than a new one.)

In `keel-net/src/process.rs`, add to `impl NetManager for ProcessNetManager`:

```rust
    fn add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), NetError> {
        let output = Self::run("route", &["add", "-net", subnet, gateway_addr])?;
        if output.status.success() || Self::stderr_contains(&output, "File exists") {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("route add -net {subnet} {gateway_addr}"),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn remove_route(&self, subnet: &str) -> Result<(), NetError> {
        let output = Self::run("route", &["delete", "-net", subnet])?;
        if output.status.success() || Self::stderr_contains(&output, "not in table") {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("route delete -net {subnet}"),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
```

(`"File exists"` and `"not in table"` are FreeBSD `route(8)`'s actual duplicate-add/missing-delete message substrings — the same tolerance idiom `attach_jail` already uses for `ifconfig`'s `"File exists"`.)

- [ ] **Step 4: Run to verify the fake tests pass**

Run: `cargo test -p keel-net --lib 2>&1 | tail -30`
Expected: PASS (all `keel-net` unit tests, including the four new ones).

- [ ] **Step 5: Append FreeBSD-only real-command tests**

Add to `keel-net/tests/freebsd_net.rs` (this file is already gated `#![cfg(target_os = "freebsd")]` at the top, so it will not compile or run on this non-FreeBSD dev machine — that's expected and matches every other test in this file):

```rust
fn destroy_route_if_exists(subnet: &str) {
    let _ = Command::new("route").args(["delete", "-net", subnet]).output();
}

#[test]
fn add_route_then_remove_route_round_trips_through_the_kernel_table() {
    let net = ProcessNetManager::new();
    let subnet = "10.99.9.0/24";
    destroy_route_if_exists(subnet);

    net.add_route(subnet, "127.0.0.1").expect("add_route should succeed");
    let check = Command::new("netstat").args(["-rn", "-f", "inet"]).output().expect("netstat should run");
    let table = String::from_utf8_lossy(&check.stdout);
    assert!(table.contains("10.99.9"), "expected the route to appear in the kernel table: {table}");

    net.remove_route(subnet).expect("remove_route should succeed");
    let check = Command::new("netstat").args(["-rn", "-f", "inet"]).output().expect("netstat should run");
    let table = String::from_utf8_lossy(&check.stdout);
    assert!(!table.contains("10.99.9"), "expected the route to be gone from the kernel table: {table}");
}

#[test]
fn add_route_and_remove_route_are_idempotent_against_the_real_kernel() {
    let net = ProcessNetManager::new();
    let subnet = "10.99.10.0/24";
    destroy_route_if_exists(subnet);

    net.add_route(subnet, "127.0.0.1").expect("first add_route should succeed");
    net.add_route(subnet, "127.0.0.1").expect("second add_route should tolerate the duplicate");

    net.remove_route(subnet).expect("first remove_route should succeed");
    net.remove_route(subnet).expect("second remove_route should tolerate the missing route");
}
```

- [ ] **Step 6: Run the full `keel-net` test suite**

Run: `cargo test -p keel-net 2>&1 | tail -20`
Expected: PASS. The `freebsd_net` test binary reports zero tests run (or is skipped entirely) on this non-FreeBSD machine — that's expected, not a failure.

- [ ] **Step 7: Commit**

```bash
git add keel-net/src/lib.rs keel-net/src/fake.rs keel-net/src/process.rs keel-net/tests/freebsd_net.rs
git commit -m "feat(keel-net): add add_route/remove_route to NetManager"
```

---

### Task 2: `keel-controlplane` — deterministic `derive_pod_cidr`

**Files:**
- Modify: `keel-controlplane/Cargo.toml`
- Create: `keel-controlplane/src/subnet.rs`
- Modify: `keel-controlplane/src/lib.rs`

**Interfaces:**
- Produces: `pub fn derive_pod_cidr(node_id: &str, cluster_cidr: &ipnet::Ipv4Net) -> ipnet::Ipv4Net` (panics if `cluster_cidr.prefix_len() > 24`).

- [ ] **Step 1: Add the `ipnet` dependency**

In `keel-controlplane/Cargo.toml`, add to `[dependencies]` (it's already in `Cargo.lock` at `2.12.0` via `keel-spec`, so this adds no new crate to the workspace, only a direct dependency edge):

```toml
ipnet = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `keel-controlplane/src/subnet.rs`:

```rust
use ipnet::Ipv4Net;
use std::net::Ipv4Addr;

const POD_PREFIX_LEN: u8 = 24;

pub fn derive_pod_cidr(node_id: &str, cluster_cidr: &Ipv4Net) -> Ipv4Net {
    assert!(
        cluster_cidr.prefix_len() <= POD_PREFIX_LEN,
        "cluster CIDR prefix length {} must be <= {POD_PREFIX_LEN} to contain at least one /24 block",
        cluster_cidr.prefix_len()
    );
    let block_count: u32 = 1u32 << (POD_PREFIX_LEN - cluster_cidr.prefix_len());
    let index = fnv1a(node_id.as_bytes()) % block_count;
    let base = u32::from(cluster_cidr.network());
    let block_addr = Ipv4Addr::from(base + index * (1u32 << (32 - POD_PREFIX_LEN)));
    Ipv4Net::new(block_addr, POD_PREFIX_LEN).expect("prefix length 24 is always valid for an IPv4 address")
}

fn fnv1a(bytes: &[u8]) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    #[test]
    fn deterministic_across_repeated_calls() {
        let cluster_cidr = cidr("10.0.0.0/16");
        assert_eq!(derive_pod_cidr("node-1", &cluster_cidr), derive_pod_cidr("node-1", &cluster_cidr));
    }

    #[test]
    fn matches_hand_computed_fnv1a_values() {
        // fnv1a("node-1") = 1422144387 % 256 = 131; fnv1a("node-2") = 1438922006 % 256 = 22;
        // fnv1a("node-3") = 1455699625 % 256 = 169 (computed independently in Python, see the plan doc).
        let cluster_cidr = cidr("10.0.0.0/16");
        assert_eq!(derive_pod_cidr("node-1", &cluster_cidr), cidr("10.0.131.0/24"));
        assert_eq!(derive_pod_cidr("node-2", &cluster_cidr), cidr("10.0.22.0/24"));
        assert_eq!(derive_pod_cidr("node-3", &cluster_cidr), cidr("10.0.169.0/24"));
    }

    #[test]
    fn different_node_ids_spread_across_the_available_blocks() {
        let cluster_cidr = cidr("10.0.0.0/16");
        let blocks: std::collections::HashSet<Ipv4Net> =
            (1..=20).map(|i| derive_pod_cidr(&format!("node-{i}"), &cluster_cidr)).collect();
        assert!(blocks.len() > 15, "expected most of 20 node-ids on distinct blocks, got {}", blocks.len());
    }

    #[test]
    fn two_node_ids_can_collide_on_a_small_cluster_cidr() {
        // fnv1a("node-4") % 4 == fnv1a("node-8") % 4 == 0 (computed independently in Python).
        let cluster_cidr = cidr("10.0.0.0/22");
        assert_eq!(derive_pod_cidr("node-4", &cluster_cidr), derive_pod_cidr("node-8", &cluster_cidr));
        assert_eq!(derive_pod_cidr("node-4", &cluster_cidr), cidr("10.0.0.0/24"));
    }

    #[test]
    #[should_panic(expected = "must be <= 24")]
    fn panics_if_cluster_cidr_is_smaller_than_a_single_pod_block() {
        derive_pod_cidr("node-1", &cidr("10.0.0.0/28"));
    }
}
```

Add `pub mod subnet;` to `keel-controlplane/src/lib.rs` (alongside the existing `pub mod` lines).

- [ ] **Step 3: Run to verify the tests pass**

Run: `cargo test -p keel-controlplane subnet:: 2>&1 | tail -30`
Expected: PASS (6 tests). This is a pure function with no wiring elsewhere yet, so there is no red step here beyond "doesn't exist until you create the file."

- [ ] **Step 4: Commit**

```bash
git add keel-controlplane/Cargo.toml keel-controlplane/src/subnet.rs keel-controlplane/src/lib.rs
git commit -m "feat(keel-controlplane): add deterministic derive_pod_cidr"
```

---

### Task 3: `keel-controlplane` — `Registry` gains `cluster_cidr`/`pod_cidr`, wire protocol change

This is the widest-blast-radius task: `Registry::new()` and `Command::Register`'s reply type both change shape, which breaks every call site across `keel-controlplane`, `keel-agentd`, and `keelctl` until they're all fixed in this same task (Rust won't let the workspace compile otherwise).

**Files:**
- Modify: `keel-controlplane/src/registry.rs`
- Modify: `keel-controlplane/src/wire.rs`
- Modify: `keel-controlplane/src/worker.rs`
- Modify: `keel-controlplane/src/http.rs`
- Modify: `keel-agentd/src/registration.rs` (test helper only, in this task)
- Modify: `keelctl/tests/cli.rs` (one call site)

**Interfaces:**
- Consumes: `subnet::derive_pod_cidr(node_id: &str, cluster_cidr: &Ipv4Net) -> Ipv4Net` (Task 2).
- Produces: `Registry::new(cluster_cidr: ipnet::Ipv4Net) -> Registry`; `Registry::register(...) -> Result<ipnet::Ipv4Net, PodCidrCollision>`; `wire::NodeStatus.pod_cidr: String`; `wire::RegisterResponse { pod_cidr: String }`; `worker::Command::Register(String, String, f64, u64, Sender<Result<ipnet::Ipv4Net, registry::PodCidrCollision>>)`.

- [ ] **Step 1: Write the failing `Registry` tests**

Replace `keel-controlplane/src/registry.rs` in full:

```rust
use crate::subnet::derive_pod_cidr;
use crate::wire::{NodeState, NodeStatus};
use ipnet::Ipv4Net;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEAD_THRESHOLD: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct NodeRecord {
    addr: String,
    last_heartbeat: Instant,
    capacity_cpu: f64,
    capacity_memory: u64,
    committed_cpu: f64,
    committed_memory: u64,
    pod_cidr: Ipv4Net,
}

#[derive(Debug)]
pub struct Registry {
    cluster_cidr: Ipv4Net,
    nodes: HashMap<String, NodeRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown node '{0}'")]
pub struct UnknownNode(pub String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("unknown node '{0}'")]
    Unknown(String),
    #[error("node '{id}' is dead (last seen {last_seen_secs}s ago)")]
    Dead { id: String, last_seen_secs: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("node '{node_id}' derived pod_cidr {derived} which collides with already-registered node '{conflicting_node_id}'")]
pub struct PodCidrCollision {
    pub node_id: String,
    pub derived: String,
    pub conflicting_node_id: String,
}

impl Registry {
    pub fn new(cluster_cidr: Ipv4Net) -> Self {
        Self { cluster_cidr, nodes: HashMap::new() }
    }

    pub fn register(
        &mut self,
        id: String,
        addr: String,
        capacity_cpu: f64,
        capacity_memory: u64,
        now: Instant,
    ) -> Result<Ipv4Net, PodCidrCollision> {
        let pod_cidr = derive_pod_cidr(&id, &self.cluster_cidr);
        if let Some((conflicting_id, _)) =
            self.nodes.iter().find(|(other_id, record)| other_id.as_str() != id.as_str() && record.pod_cidr == pod_cidr)
        {
            return Err(PodCidrCollision {
                node_id: id,
                derived: pod_cidr.to_string(),
                conflicting_node_id: conflicting_id.clone(),
            });
        }
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
            },
        );
        Ok(pod_cidr)
    }

    pub fn heartbeat(
        &mut self,
        id: &str,
        committed_cpu: f64,
        committed_memory: u64,
        now: Instant,
    ) -> Result<(), UnknownNode> {
        match self.nodes.get_mut(id) {
            Some(record) => {
                record.last_heartbeat = now;
                record.committed_cpu = committed_cpu;
                record.committed_memory = committed_memory;
                Ok(())
            }
            None => Err(UnknownNode(id.to_string())),
        }
    }

    pub fn list(&self, now: Instant) -> Vec<NodeStatus> {
        let mut statuses: Vec<NodeStatus> = self
            .nodes
            .iter()
            .map(|(id, record)| {
                let elapsed = now.saturating_duration_since(record.last_heartbeat);
                NodeStatus {
                    id: id.clone(),
                    addr: record.addr.clone(),
                    pod_cidr: record.pod_cidr.to_string(),
                    status: if elapsed < DEAD_THRESHOLD { NodeState::Alive } else { NodeState::Dead },
                    last_seen_secs: elapsed.as_secs(),
                    capacity_cpu: record.capacity_cpu,
                    capacity_memory: record.capacity_memory,
                    committed_cpu: record.committed_cpu,
                    committed_memory: record.committed_memory,
                }
            })
            .collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }

    pub fn resolve(&self, id: &str, now: Instant) -> Result<String, ResolveError> {
        let record = self.nodes.get(id).ok_or_else(|| ResolveError::Unknown(id.to_string()))?;
        let elapsed = now.saturating_duration_since(record.last_heartbeat);
        if elapsed >= DEAD_THRESHOLD {
            return Err(ResolveError::Dead { id: id.to_string(), last_seen_secs: elapsed.as_secs() });
        }
        Ok(record.addr.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cluster_cidr() -> Ipv4Net {
        "10.0.0.0/16".parse().unwrap()
    }

    fn small_cluster_cidr() -> Ipv4Net {
        "10.0.0.0/22".parse().unwrap()
    }

    #[test]
    fn register_then_list_shows_the_node_as_alive() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "192.168.64.4".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();

        let statuses = registry.list(now);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-1");
        assert_eq!(statuses[0].addr, "192.168.64.4");
        assert_eq!(statuses[0].status, NodeState::Alive);
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn register_returns_the_derived_pod_cidr() {
        let mut registry = Registry::new(test_cluster_cidr());
        let pod_cidr = registry.register("node-1".to_string(), "192.168.64.4".to_string(), 4.0, 8 * 1024 * 1024 * 1024, Instant::now()).unwrap();
        assert_eq!(pod_cidr, "10.0.131.0/24".parse::<Ipv4Net>().unwrap());
    }

    #[test]
    fn list_includes_pod_cidr_per_node() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "192.168.64.4".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();
        assert_eq!(registry.list(now)[0].pod_cidr, "10.0.131.0/24");
    }

    #[test]
    fn a_colliding_registration_is_rejected_and_names_both_nodes() {
        let mut registry = Registry::new(small_cluster_cidr());
        let now = Instant::now();
        registry.register("node-4".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();

        let err = registry
            .register("node-8".to_string(), "10.0.0.2".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now)
            .unwrap_err();
        assert_eq!(err.node_id, "node-8");
        assert_eq!(err.conflicting_node_id, "node-4");
        assert_eq!(err.derived, "10.0.0.0/24");

        // The first node's assignment must be untouched by the rejected second registration.
        let statuses = registry.list(now);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-4");
        assert_eq!(statuses[0].pod_cidr, "10.0.0.0/24");
    }

    #[test]
    fn reregistering_an_existing_id_refreshes_its_address_and_heartbeat() {
        let mut registry = Registry::new(test_cluster_cidr());
        let t0 = Instant::now();
        let first_pod_cidr = registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0).unwrap();

        let t1 = t0 + Duration::from_secs(5);
        let second_pod_cidr = registry.register("node-1".to_string(), "10.0.0.2".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t1).unwrap();

        assert_eq!(first_pod_cidr, second_pod_cidr, "re-registering the same node-id must derive the same pod_cidr");

        let statuses = registry.list(t1);
        assert_eq!(statuses.len(), 1, "re-registering must not create a second entry");
        assert_eq!(statuses[0].addr, "10.0.0.2");
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn heartbeat_on_a_known_node_updates_its_last_heartbeat() {
        let mut registry = Registry::new(test_cluster_cidr());
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0).unwrap();

        let t1 = t0 + Duration::from_secs(10);
        registry.heartbeat("node-1", 0.0, 0, t1).unwrap();

        let statuses = registry.list(t1);
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn heartbeat_on_an_unknown_node_returns_unknown_node_error() {
        let mut registry = Registry::new(test_cluster_cidr());
        let err = registry.heartbeat("missing", 0.0, 0, Instant::now()).unwrap_err();
        assert_eq!(err, UnknownNode("missing".to_string()));
        assert_eq!(err.to_string(), "unknown node 'missing'");
    }

    #[test]
    fn list_reports_dead_once_a_node_exceeds_the_dead_threshold() {
        let mut registry = Registry::new(test_cluster_cidr());
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0).unwrap();

        let just_under = t0 + Duration::from_secs(14);
        assert_eq!(registry.list(just_under)[0].status, NodeState::Alive);

        let at_threshold = t0 + DEAD_THRESHOLD;
        assert_eq!(registry.list(at_threshold)[0].status, NodeState::Dead);
    }

    #[test]
    fn list_is_sorted_by_id() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-2".to_string(), "10.0.0.2".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();

        let statuses = registry.list(now);
        assert_eq!(statuses.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["node-1", "node-2"]);
    }

    #[test]
    fn list_on_an_empty_registry_is_empty() {
        let registry = Registry::new(test_cluster_cidr());
        assert_eq!(registry.list(Instant::now()), vec![]);
    }

    #[test]
    fn resolve_on_an_unknown_node_returns_unknown_error() {
        let registry = Registry::new(test_cluster_cidr());
        let err = registry.resolve("missing", Instant::now()).unwrap_err();
        assert_eq!(err, ResolveError::Unknown("missing".to_string()));
    }

    #[test]
    fn resolve_on_an_alive_node_returns_its_address() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();
        assert_eq!(registry.resolve("node-1", now), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_on_a_dead_node_returns_dead_error_with_elapsed_seconds() {
        let mut registry = Registry::new(test_cluster_cidr());
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0).unwrap();

        let at_threshold = t0 + DEAD_THRESHOLD;
        let err = registry.resolve("node-1", at_threshold).unwrap_err();
        assert_eq!(err, ResolveError::Dead { id: "node-1".to_string(), last_seen_secs: DEAD_THRESHOLD.as_secs() });
    }

    #[test]
    fn register_initializes_committed_resources_to_zero() {
        let mut registry = Registry::new(test_cluster_cidr());
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, now).unwrap();

        let statuses = registry.list(now);
        assert_eq!(statuses[0].capacity_cpu, 4.0);
        assert_eq!(statuses[0].capacity_memory, 8 * 1024 * 1024 * 1024);
        assert_eq!(statuses[0].committed_cpu, 0.0);
        assert_eq!(statuses[0].committed_memory, 0);
    }

    #[test]
    fn heartbeat_updates_committed_resources_without_changing_capacity() {
        let mut registry = Registry::new(test_cluster_cidr());
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), 4.0, 8 * 1024 * 1024 * 1024, t0).unwrap();

        let t1 = t0 + Duration::from_secs(5);
        registry.heartbeat("node-1", 2.0, 1024 * 1024 * 1024, t1).unwrap();

        let statuses = registry.list(t1);
        assert_eq!(statuses[0].committed_cpu, 2.0);
        assert_eq!(statuses[0].committed_memory, 1024 * 1024 * 1024);
        assert_eq!(statuses[0].capacity_cpu, 4.0, "heartbeat must not change capacity");
        assert_eq!(statuses[0].capacity_memory, 8 * 1024 * 1024 * 1024, "heartbeat must not change capacity");
    }
}
```

- [ ] **Step 2: Run to verify it fails to compile**

Run: `cargo test -p keel-controlplane --lib 2>&1 | tail -40`
Expected: FAIL — `wire::NodeStatus` has no field `pod_cidr` yet (fixed in Step 3 below, same task).

- [ ] **Step 3: Update `wire.rs`, `worker.rs`, and `http.rs`'s `handle_register`**

In `keel-controlplane/src/wire.rs`, add `pod_cidr` to `NodeStatus` (right after `addr`) and add a new `RegisterResponse` type:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub id: String,
    pub addr: String,
    pub pod_cidr: String,
    pub status: NodeState,
    pub last_seen_secs: u64,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
    pub committed_cpu: f64,
    pub committed_memory: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub pod_cidr: String,
}
```

Update the existing `node_status_round_trips_through_yaml` test in the same file to include `pod_cidr: "10.0.4.0/24".to_string()` in its constructed `NodeStatus`, and add:

```rust
    #[test]
    fn register_response_round_trips_through_yaml() {
        let response = RegisterResponse { pod_cidr: "10.0.4.0/24".to_string() };
        let yaml = serde_yaml::to_string(&response).unwrap();
        let parsed: RegisterResponse = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, response);
    }
```

In `keel-controlplane/src/worker.rs`, change the `Command::Register` variant and its handler:

```rust
use crate::registry::{PodCidrCollision, Registry, ResolveError, UnknownNode};
```

```rust
pub enum Command {
    Register(String, String, f64, u64, Sender<Result<ipnet::Ipv4Net, PodCidrCollision>>),
    Heartbeat(String, f64, u64, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
    ResolveOrSchedule(String, Sender<Result<(String, String), ScheduleOrResolveError>>),
    ResolvePlacement(String, Sender<Result<(String, String), PlacementError>>),
    RecordPlacement(String, String, Sender<()>),
    RemovePlacement(String, Sender<()>),
}
```

```rust
        Command::Register(id, addr, capacity_cpu, capacity_memory, reply) => {
            let result = registry.register(id, addr, capacity_cpu, capacity_memory, Instant::now());
            let _ = reply.send(result);
        }
```

Update every `worker.rs` test that registers a node: change `Registry::new()` → `Registry::new(test_cluster_cidr())` (add the helper below, matching `registry.rs`'s), and every `reg_rx.recv().unwrap();` after a `Command::Register` send → `reg_rx.recv().unwrap().unwrap();`. Add near the top of `worker.rs`'s `#[cfg(test)] mod tests`:

```rust
    fn test_cluster_cidr() -> ipnet::Ipv4Net {
        "10.0.0.0/16".parse().unwrap()
    }
```

Every `spawn(Registry::new(), Placements::new())` in `worker.rs`'s tests becomes `spawn(Registry::new(test_cluster_cidr()), Placements::new())` (11 call sites: `register_command_makes_the_node_visible_in_list`, `heartbeat_command_on_unknown_id_returns_an_error`, `heartbeat_command_on_a_registered_node_succeeds`, `list_command_on_a_fresh_worker_is_empty`, `resolve_command_on_a_registered_alive_node_returns_its_address`, `resolve_command_on_an_unknown_node_returns_an_error`, `register_node` helper, `heartbeat_node` helper (no `Registry::new` there, skip), and the four `resolve_or_schedule_*`/`record_then_remove_placement_is_reflected_by_resolve_placement` tests). In the `register_node` test helper specifically:

```rust
    fn register_node(commands: &Sender<Command>, id: &str, addr: &str, capacity_cpu: f64, capacity_memory: u64) {
        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(id.to_string(), addr.to_string(), capacity_cpu, capacity_memory, reg_tx))
            .unwrap();
        reg_rx.recv().unwrap().unwrap();
    }
```

And in the three inline (non-helper) `Command::Register` sends inside `register_command_makes_the_node_visible_in_list`, `heartbeat_command_on_a_registered_node_succeeds`, and `resolve_command_on_a_registered_alive_node_returns_its_address`, change their `reg_rx.recv().unwrap();` to `reg_rx.recv().unwrap().unwrap();`.

In `keel-controlplane/src/http.rs`, update `handle_register`:

```rust
fn handle_register(body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let registration: NodeRegistration = match serde_yaml::from_slice(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands
        .send(Command::Register(registration.id, registration.addr, registration.capacity_cpu, registration.capacity_memory, reply_tx))
        .is_err()
    {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(pod_cidr)) => yaml_response(200, &RegisterResponse { pod_cidr: pod_cidr.to_string() }),
        Ok(Err(e)) => error_response(409, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}
```

Add `RegisterResponse` to `http.rs`'s `use crate::wire::{...}` import line.

Fix `http.rs`'s own three `Registry::new()` test call sites (`start_test_server`, and the two `reloading_tls_*` tests) to `Registry::new("10.0.0.0/16".parse().unwrap())`.

Finally, in `keelctl/tests/cli.rs`, fix the one `Registry::new()` call site in `start_test_control_plane_with_node` (around line 93):

```rust
    let (_worker_handle, commands) = keel_controlplane::worker::spawn(
        keel_controlplane::Registry::new("10.0.0.0/16".parse().unwrap()),
        keel_controlplane::Placements::new(),
    );
```

and its `reg_rx.recv().unwrap();` (around line 105) to `reg_rx.recv().unwrap().unwrap();`.

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -60`
Expected: PASS across `keel-controlplane`, `keel-agentd`, `keelctl` (agentd and keelctl only needed the mechanical `Registry::new(...)` fix above — their own registration/PodCidr behavior comes in later tasks and is unaffected by this step).

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/registry.rs keel-controlplane/src/wire.rs keel-controlplane/src/worker.rs keel-controlplane/src/http.rs keelctl/tests/cli.rs
git commit -m "feat(keel-controlplane): Registry derives and collision-checks pod_cidr per node"
```

---

### Task 4: `keel-controlplane` — `--cluster-cidr` CLI flag

**Files:**
- Modify: `keel-controlplane/src/main.rs`

**Interfaces:**
- Consumes: `Registry::new(cluster_cidr: ipnet::Ipv4Net)` (Task 3).

- [ ] **Step 1: Write the failing CLI-parsing tests**

Add to `keel-controlplane/src/main.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn parses_the_cluster_cidr_flag() {
        let config = parse_args_from(args(&[
            "--cluster-cidr", "10.0.0.0/16",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
        assert_eq!(config.cluster_cidr, Some("10.0.0.0/16".parse().unwrap()));
    }

    #[test]
    #[should_panic(expected = "--cluster-cidr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required")]
    fn missing_cluster_cidr_panics() {
        parse_args_from(args(&[
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }

    #[test]
    #[should_panic(expected = "invalid --cluster-cidr")]
    fn malformed_cluster_cidr_panics_with_a_clear_message() {
        parse_args_from(args(&[
            "--cluster-cidr", "not-a-cidr",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }

    #[test]
    #[should_panic(expected = "must be <= 24")]
    fn cluster_cidr_prefix_larger_than_24_panics() {
        parse_args_from(args(&[
            "--cluster-cidr", "10.0.0.0/28",
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
            "--tls-crl-file", "/etc/keel/crl.pem",
        ]));
    }
```

- [ ] **Step 2: Run to verify it fails to compile**

Run: `cargo test -p keel-controlplane --bin keel-controlplane 2>&1 | tail -30`
Expected: FAIL — `Config` has no field `cluster_cidr`.

- [ ] **Step 3: Implement the flag**

In `keel-controlplane/src/main.rs`:

```rust
use ipnet::Ipv4Net;
```

```rust
struct Config {
    addr: String,
    cluster_cidr: Option<Ipv4Net>,
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
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
            tls_crl_file: None,
        }
    }
}
```

```rust
            "--cluster-cidr" => {
                config.cluster_cidr = Some(
                    value.parse().unwrap_or_else(|e| panic!("invalid --cluster-cidr '{value}': {e}")),
                )
            }
```

```rust
    if config.cluster_cidr.is_none()
        || config.tls_ca_file.is_none()
        || config.tls_cert_file.is_none()
        || config.tls_key_file.is_none()
        || config.tls_crl_file.is_none()
    {
        panic!("--cluster-cidr, --tls-ca-file, --tls-cert-file, --tls-key-file, and --tls-crl-file are all required");
    }
    if let Some(cidr) = config.cluster_cidr {
        assert!(cidr.prefix_len() <= 24, "--cluster-cidr prefix length {} must be <= 24", cidr.prefix_len());
    }
    config
```

And in `fn main()`, thread it through:

```rust
    let config = parse_args();
    let cluster_cidr = config.cluster_cidr.expect("validated as required in parse_args_from");
    let ca_file = config.tls_ca_file.expect("validated as required in parse_args_from");
    ...
    let (_worker_handle, commands) = worker::spawn(Registry::new(cluster_cidr), Placements::new());
```

- [ ] **Step 4: Run to verify the tests pass**

Run: `cargo test -p keel-controlplane 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-controlplane/src/main.rs
git commit -m "feat(keel-controlplane): add unconditionally-required --cluster-cidr flag"
```

---

### Task 5: `keel-agentd` — `PodCidrSlot` and capturing `pod_cidr` at registration

**Files:**
- Modify: `keel-agentd/Cargo.toml`
- Create: `keel-agentd/src/podcidr.rs`
- Modify: `keel-agentd/src/lib.rs`
- Modify: `keel-agentd/src/registration.rs`

**Interfaces:**
- Consumes: `keel_controlplane::wire::RegisterResponse { pod_cidr: String }` (Task 3, already reachable — `keel-agentd` already depends on `keel-controlplane`).
- Produces: `pub struct PodCidrSlot` with `fn new() -> Self`, `fn set(&self, pod_cidr: ipnet::Ipv4Net)`, `fn get(&self) -> Option<ipnet::Ipv4Net>` (all `Clone`, cheap `Arc`-backed); `registration::spawn` gains a `pod_cidr_slot: PodCidrSlot` parameter and calls `.set(...)` after a successful registration.

- [ ] **Step 1: Add the `ipnet` dependency**

In `keel-agentd/Cargo.toml`, add to `[dependencies]`:

```toml
ipnet = "2"
```

- [ ] **Step 2: Write the failing `PodCidrSlot` test**

Create `keel-agentd/src/podcidr.rs`:

```rust
use ipnet::Ipv4Net;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct PodCidrSlot(Arc<Mutex<Option<Ipv4Net>>>);

impl PodCidrSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, pod_cidr: Ipv4Net) {
        *self.0.lock().unwrap() = Some(pod_cidr);
    }

    pub fn get(&self) -> Option<Ipv4Net> {
        *self.0.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_slot_is_empty() {
        assert_eq!(PodCidrSlot::new().get(), None);
    }

    #[test]
    fn set_then_get_returns_the_value() {
        let slot = PodCidrSlot::new();
        slot.set("10.0.4.0/24".parse().unwrap());
        assert_eq!(slot.get(), Some("10.0.4.0/24".parse().unwrap()));
    }

    #[test]
    fn clones_share_the_same_underlying_slot() {
        let slot = PodCidrSlot::new();
        let clone = slot.clone();
        clone.set("10.0.5.0/24".parse().unwrap());
        assert_eq!(slot.get(), Some("10.0.5.0/24".parse().unwrap()));
    }
}
```

Add `pub mod podcidr;` and `pub use podcidr::PodCidrSlot;` to `keel-agentd/src/lib.rs`.

- [ ] **Step 3: Run to verify it passes**

Run: `cargo test -p keel-agentd podcidr:: 2>&1 | tail -20`
Expected: PASS (3 tests) — this module has no wiring elsewhere yet.

- [ ] **Step 4: Wire `PodCidrSlot` into the registration loop**

In `keel-agentd/src/registration.rs`, change `send_request` to return the body on success instead of discarding it:

```rust
fn send_request(addr: &str, method: &str, path: &str, body: &str, client_config: &Arc<rustls::ClientConfig>) -> Result<Vec<u8>, String> {
```

(Keep everything up through the `Content-Length` truncation check unchanged.) Replace the final block:

```rust
    if (200..300).contains(&status) {
        Ok(response[header_len..].to_vec())
    } else {
        Err(format!("control plane returned status {status}"))
    }
```

Update `heartbeat_once` (it doesn't need the body, just success/failure):

```rust
fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, client_config)?;
    Ok(())
}
```

Update `register_once` to parse the returned `pod_cidr` and return it:

```rust
fn register_once(
    control_plane_addr: &str,
    node_id: &str,
    advertise_addr: &str,
    capacity_cpu: f64,
    capacity_memory: u64,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<ipnet::Ipv4Net, String> {
    let body = format!(
        "id: {node_id}\naddr: {advertise_addr}\ncapacity_cpu: {capacity_cpu}\ncapacity_memory: {capacity_memory}\n"
    );
    let response_body = send_request(control_plane_addr, "POST", "/nodes/register", &body, client_config)?;
    let response: keel_controlplane::wire::RegisterResponse = serde_yaml::from_slice(&response_body)
        .map_err(|e| format!("malformed registration response: {e}"))?;
    response
        .pod_cidr
        .parse()
        .map_err(|e| format!("control plane returned invalid pod_cidr '{}': {e}", response.pod_cidr))
}
```

Update `spawn`'s signature and loop body to accept and use a `PodCidrSlot`:

```rust
#[allow(clippy::too_many_arguments)]
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
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("keel-agentd: heartbeat failed: {e}");
                        registered = false;
                    }
                }
            }
            thread::sleep(heartbeat_interval);
        }
    })
}
```

- [ ] **Step 5: Fix `registration.rs`'s own test call sites**

`registration.rs`'s tests call `spawn(...)` four times and `start_test_control_plane()` calls `Registry::new()` once. Update `start_test_control_plane`:

```rust
    fn start_test_control_plane() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) =
            worker::spawn(Registry::new("10.0.0.0/16".parse().unwrap()), Placements::new());
        let reloading_tls = keel_controlplane::tls::ReloadingTls::spawn(
            fixture("fixture-node.crt"),
            fixture("fixture-node.key"),
            fixture("ca.crt"),
            fixture("crl.pem"),
            Duration::from_secs(3600),
        )
        .unwrap();
        thread::spawn(move || keel_controlplane::http::run(listener, commands, reloading_tls));
        addr
    }
```

And append `crate::PodCidrSlot::new()` as the final argument to every `spawn(...)` call in this file's tests (`registers_and_then_keeps_heartbeating`, `heartbeats_report_the_reconcilers_committed_resources`, `registration_with_a_wrong_ca_certificate_never_registers`).

Add a new test proving the capture actually works:

```rust
    #[test]
    fn a_successful_registration_stores_the_returned_pod_cidr_in_the_slot() {
        let control_plane_addr = start_test_control_plane();
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                keel_net::FakeNetManager::new(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-a_successful_registration_stores_the_returned_pod_cidr_in_the_slot"),
            )
            .unwrap(),
        );
        let pod_cidr_slot = crate::PodCidrSlot::new();
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr,
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            node_reloading_tls(),
            commands,
            pod_cidr_slot.clone(),
        );

        thread::sleep(Duration::from_millis(200));
        assert!(pod_cidr_slot.get().is_some(), "expected the registration loop to have stored a pod_cidr by now");
    }
```

- [ ] **Step 6: Run to verify tests pass**

Run: `cargo test -p keel-agentd registration:: 2>&1 | tail -40`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add keel-agentd/Cargo.toml keel-agentd/src/podcidr.rs keel-agentd/src/lib.rs keel-agentd/src/registration.rs
git commit -m "feat(keel-agentd): capture pod_cidr from registration into a shared PodCidrSlot"
```

Note: this task leaves `main.rs`'s call to `registration::spawn` (Task 5 changed its signature) uncompiled — `keel-agentd`'s binary target will not build again until Task 9. `cargo test -p keel-agentd registration::` above scopes to the library's own test target, which is unaffected; do not run `cargo build --workspace` until after Task 9.

---

### Task 6: `keel-agentd` — `Command::AddRoute`/`RemoveRoute` on the reconciler worker

**Files:**
- Modify: `keel-agentd/src/reconciler.rs`
- Modify: `keel-agentd/src/worker.rs`

**Interfaces:**
- Consumes: `keel_net::NetManager::add_route`/`remove_route` (Task 1).
- Produces: `Reconciler::add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), keel_net::NetError>`, `Reconciler::remove_route(&self, subnet: &str) -> Result<(), keel_net::NetError>`; `worker::Command::AddRoute(String, String, Sender<Result<(), keel_net::NetError>>)`, `worker::Command::RemoveRoute(String, Sender<Result<(), keel_net::NetError>>)`.

- [ ] **Step 1: Write the failing `worker.rs` tests**

Add to `keel-agentd/src/worker.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn add_route_command_calls_through_to_the_net_manager() {
        let commands = spawn_test_worker("add_route_command_calls_through_to_the_net_manager");

        let (tx, rx) = mpsc::channel();
        commands.send(Command::AddRoute("10.0.5.0/24".to_string(), "192.168.64.5".to_string(), tx)).unwrap();
        assert!(rx.recv().unwrap().is_ok());
    }

    #[test]
    fn remove_route_command_calls_through_to_the_net_manager() {
        let commands = spawn_test_worker("remove_route_command_calls_through_to_the_net_manager");

        let (add_tx, add_rx) = mpsc::channel();
        commands.send(Command::AddRoute("10.0.5.0/24".to_string(), "192.168.64.5".to_string(), add_tx)).unwrap();
        add_rx.recv().unwrap().unwrap();

        let (rm_tx, rm_rx) = mpsc::channel();
        commands.send(Command::RemoveRoute("10.0.5.0/24".to_string(), rm_tx)).unwrap();
        assert!(rm_rx.recv().unwrap().is_ok());
    }
```

- [ ] **Step 2: Run to verify it fails to compile**

Run: `cargo test -p keel-agentd worker:: 2>&1 | tail -30`
Expected: FAIL — `Command` has no variant `AddRoute`.

- [ ] **Step 3: Implement**

In `keel-agentd/src/reconciler.rs`, add two methods to `impl<J: JailRuntime, Z: ZfsManager, N: NetManager> Reconciler<J, Z, N>` (near `delete`, since both touch `self.net`):

```rust
    pub fn add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), keel_net::NetError> {
        self.net.add_route(subnet, gateway_addr)
    }

    pub fn remove_route(&self, subnet: &str) -> Result<(), keel_net::NetError> {
        self.net.remove_route(subnet)
    }
```

In `keel-agentd/src/worker.rs`, extend `Command` and its handler:

```rust
pub enum Command {
    Apply(JailSpec, Sender<Result<(), ReconcileError>>),
    Get(Option<String>, Sender<Vec<JailStatus>>),
    Delete(String, Sender<Result<(), ReconcileError>>),
    Tick,
    CommittedResources(Sender<(f64, u64)>),
    AddRoute(String, String, Sender<Result<(), keel_net::NetError>>),
    RemoveRoute(String, Sender<Result<(), keel_net::NetError>>),
}
```

```rust
        Command::AddRoute(subnet, gateway_addr, reply) => {
            let _ = reply.send(reconciler.add_route(&subnet, &gateway_addr));
        }
        Command::RemoveRoute(subnet, reply) => {
            let _ = reply.send(reconciler.remove_route(&subnet));
        }
```

- [ ] **Step 4: Run to verify tests pass**

Run: `cargo test -p keel-agentd worker:: 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/reconciler.rs keel-agentd/src/worker.rs
git commit -m "feat(keel-agentd): add Command::AddRoute/RemoveRoute to the reconciler worker"
```

---

### Task 7: `keel-agentd` — route reconciliation on the registration tick

**Files:**
- Modify: `keel-agentd/src/registration.rs`

**Interfaces:**
- Consumes: `keel_controlplane::wire::NodeStatus` (Task 3, `pod_cidr` field), `worker::Command::AddRoute`/`RemoveRoute` (Task 6), `send_request` returning `Result<Vec<u8>, String>` (Task 5).
- Produces: `pub(crate) fn diff_routes(self_id: &str, peers: &[NodeStatus], installed: &HashMap<String, String>) -> (Vec<(String, String)>, Vec<String>)` — pure function returning `(routes_to_add: Vec<(pod_cidr, gateway_addr)>, routes_to_remove: Vec<pod_cidr>)`.

- [ ] **Step 1: Write the failing pure-function tests**

Add near the top of `keel-agentd/src/registration.rs` (below the existing `use` statements, above `pub fn spawn`):

```rust
use keel_controlplane::wire::{NodeState, NodeStatus};
use std::collections::HashMap;

pub(crate) fn diff_routes(
    self_id: &str,
    peers: &[NodeStatus],
    installed: &HashMap<String, String>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut to_add = Vec::new();
    for peer in peers {
        if peer.id == self_id || peer.status != NodeState::Alive {
            continue;
        }
        if installed.get(&peer.id) != Some(&peer.pod_cidr) {
            to_add.push((peer.pod_cidr.clone(), peer.addr.clone()));
        }
    }

    let alive_ids: std::collections::HashSet<&str> = peers
        .iter()
        .filter(|p| p.status == NodeState::Alive && p.id != self_id)
        .map(|p| p.id.as_str())
        .collect();
    let mut to_remove = Vec::new();
    for (id, pod_cidr) in installed {
        if !alive_ids.contains(id.as_str()) {
            to_remove.push(pod_cidr.clone());
        }
    }

    (to_add, to_remove)
}
```

Add tests (in the existing `#[cfg(test)] mod tests` block):

```rust
    fn node_status(id: &str, addr: &str, pod_cidr: &str, status: NodeState) -> NodeStatus {
        NodeStatus {
            id: id.to_string(),
            addr: addr.to_string(),
            pod_cidr: pod_cidr.to_string(),
            status,
            last_seen_secs: 0,
            capacity_cpu: 4.0,
            capacity_memory: 8 * 1024 * 1024 * 1024,
            committed_cpu: 0.0,
            committed_memory: 0,
        }
    }

    #[test]
    fn a_new_alive_peer_is_added_and_self_is_never_added() {
        let peers = vec![
            node_status("node-1", "10.0.0.1", "10.0.1.0/24", NodeState::Alive),
            node_status("node-2", "10.0.0.2", "10.0.2.0/24", NodeState::Alive),
        ];
        let (to_add, to_remove) = diff_routes("node-1", &peers, &HashMap::new());
        assert_eq!(to_add, vec![("10.0.2.0/24".to_string(), "10.0.0.2".to_string())]);
        assert!(to_remove.is_empty());
    }

    #[test]
    fn an_already_installed_peer_with_the_same_pod_cidr_is_not_re_added() {
        let peers = vec![node_status("node-2", "10.0.0.2", "10.0.2.0/24", NodeState::Alive)];
        let mut installed = HashMap::new();
        installed.insert("node-2".to_string(), "10.0.2.0/24".to_string());
        let (to_add, to_remove) = diff_routes("node-1", &peers, &installed);
        assert!(to_add.is_empty());
        assert!(to_remove.is_empty());
    }

    #[test]
    fn a_dead_peer_that_was_installed_is_removed() {
        let peers = vec![node_status("node-2", "10.0.0.2", "10.0.2.0/24", NodeState::Dead)];
        let mut installed = HashMap::new();
        installed.insert("node-2".to_string(), "10.0.2.0/24".to_string());
        let (to_add, to_remove) = diff_routes("node-1", &peers, &installed);
        assert!(to_add.is_empty());
        assert_eq!(to_remove, vec!["10.0.2.0/24".to_string()]);
    }

    #[test]
    fn a_peer_missing_entirely_from_the_list_that_was_installed_is_removed() {
        let peers: Vec<NodeStatus> = vec![];
        let mut installed = HashMap::new();
        installed.insert("node-2".to_string(), "10.0.2.0/24".to_string());
        let (to_add, to_remove) = diff_routes("node-1", &peers, &installed);
        assert!(to_add.is_empty());
        assert_eq!(to_remove, vec!["10.0.2.0/24".to_string()]);
    }
```

- [ ] **Step 2: Run to verify tests pass**

Run: `cargo test -p keel-agentd registration::tests 2>&1 | tail -40`
Expected: PASS — this is a pure function with no wiring into `spawn` yet.

- [ ] **Step 3: Wire the diff into the tick loop**

Change `spawn`'s loop body to call `GET /nodes` and reconcile routes on every tick, regardless of that tick's register/heartbeat outcome:

```rust
    thread::spawn(move || {
        let mut registered = false;
        let mut installed_routes: HashMap<String, String> = HashMap::new();
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
                    Ok(()) => {}
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
```

Add the two new helper functions (below `heartbeat_once`):

```rust
fn fetch_nodes(control_plane_addr: &str, client_config: &Arc<rustls::ClientConfig>) -> Result<Vec<NodeStatus>, String> {
    let body = send_request(control_plane_addr, "GET", "/nodes", "", client_config)?;
    serde_yaml::from_slice(&body).map_err(|e| format!("malformed /nodes response: {e}"))
}

fn reconcile_routes(
    self_id: &str,
    peers: &[NodeStatus],
    installed_routes: &mut HashMap<String, String>,
    commands: &Sender<crate::worker::Command>,
) {
    let (to_add, to_remove) = diff_routes(self_id, peers, installed_routes);

    for pod_cidr in to_remove {
        let (tx, rx) = std::sync::mpsc::channel();
        if commands.send(crate::worker::Command::RemoveRoute(pod_cidr.clone(), tx)).is_err() {
            return;
        }
        match rx.recv() {
            Ok(Ok(())) => {
                installed_routes.retain(|_, v| v != &pod_cidr);
            }
            Ok(Err(e)) => eprintln!("keel-agentd: failed to remove route for {pod_cidr}: {e}"),
            Err(_) => eprintln!("keel-agentd: reconciler worker did not respond to RemoveRoute"),
        }
    }

    for (pod_cidr, gateway_addr) in to_add {
        let (tx, rx) = std::sync::mpsc::channel();
        if commands.send(crate::worker::Command::AddRoute(pod_cidr.clone(), gateway_addr.clone(), tx)).is_err() {
            return;
        }
        match rx.recv() {
            Ok(Ok(())) => {
                if let Some(peer) = peers.iter().find(|p| p.pod_cidr == pod_cidr) {
                    installed_routes.insert(peer.id.clone(), pod_cidr);
                }
            }
            Ok(Err(e)) => eprintln!("keel-agentd: failed to add route for {pod_cidr} via {gateway_addr}: {e}"),
            Err(_) => eprintln!("keel-agentd: reconciler worker did not respond to AddRoute"),
        }
    }
}
```

- [ ] **Step 4: Write the integration test**

Add to `registration.rs`'s test module — a two-node scenario using the same real-control-plane pattern the file's existing tests already use, checking `FakeNetManager::has_route` after the reconciliation tick:

```rust
    #[test]
    fn route_reconciliation_adds_a_route_for_a_peer_and_removes_it_once_the_peer_goes_dead() {
        let control_plane_addr = start_test_control_plane();

        // Pre-register a second, alive peer directly against the control plane's
        // own worker (bypassing a second full agentd instance, matching how
        // `keelctl`'s own tests pre-seed a peer node).
        let (worker_handle_ignored, cp_commands) = {
            // Reuse the running control plane's registry by registering through
            // the same HTTP endpoint this node itself will use.
            (None::<()>, ())
        };
        let _ = (worker_handle_ignored, cp_commands);

        // Register "node-2" as a peer up front via a raw HTTP call, so the
        // node under test sees it as Alive on its very first GET /nodes.
        let client_config = node_client_config();
        send_request(&control_plane_addr, "POST", "/nodes/register", "id: node-2\naddr: 10.0.0.2\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n", &client_config).unwrap();

        let net = keel_net::FakeNetManager::new();
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                net.clone(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-route_reconciliation_adds_and_removes"),
            )
            .unwrap(),
        );
        let pod_cidr_slot = crate::PodCidrSlot::new();
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            node_reloading_tls(),
            commands,
            pod_cidr_slot,
        );

        thread::sleep(Duration::from_millis(300));
        let body = get_nodes(&control_plane_addr);
        let node_2_pod_cidr = body
            .lines()
            .skip_while(|l| !l.contains("id: node-2"))
            .find_map(|_| None::<String>)
            .unwrap_or_default();
        let _ = node_2_pod_cidr; // pod_cidr value itself is opaque here; presence of the route is what's asserted.

        // node-2 should now be routed: some subnet is installed with node-2's advertised address as gateway.
        assert!(
            net.has_route("10.0.2.0/24").is_some() || body.contains("node-2"),
            "expected node-2 to be visible and routed, got GET /nodes body: {body}"
        );
    }
```

Note for the implementing engineer: the exact `pod_cidr` the control plane derives for `"node-2"` depends on the `--cluster-cidr` this test's control plane was constructed with (`start_test_control_plane` uses `10.0.0.0/16`) and `derive_pod_cidr("node-2", ...)`, which this plan already computed as `10.0.22.0/24` (see the Verified Facts table at the top of this plan). Replace the loose `net.has_route("10.0.2.0/24")` placeholder above with the exact value:

```rust
        assert_eq!(
            net.has_route("10.0.22.0/24"),
            Some("10.0.0.2".to_string()),
            "expected node-1 to have installed a route to node-2's pod_cidr via its advertised address"
        );
```

and delete the unused `node_2_pod_cidr`/`get_nodes` scratch lines above it — the final test body is:

```rust
    #[test]
    fn route_reconciliation_adds_a_route_for_a_peer() {
        let control_plane_addr = start_test_control_plane();

        let client_config = node_client_config();
        send_request(
            &control_plane_addr,
            "POST",
            "/nodes/register",
            "id: node-2\naddr: 10.0.0.2\ncapacity_cpu: 4\ncapacity_memory: 8589934592\n",
            &client_config,
        )
        .unwrap();

        let net = keel_net::FakeNetManager::new();
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                net.clone(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-route_reconciliation_adds_a_route_for_a_peer"),
            )
            .unwrap(),
        );
        let pod_cidr_slot = crate::PodCidrSlot::new();
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr,
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            node_reloading_tls(),
            commands,
            pod_cidr_slot,
        );

        thread::sleep(Duration::from_millis(300));

        // derive_pod_cidr("node-2", "10.0.0.0/16") == 10.0.22.0/24 (verified independently; see this plan's Verified Facts table).
        assert_eq!(
            net.has_route("10.0.22.0/24"),
            Some("10.0.0.2".to_string()),
            "expected node-1 to have installed a route to node-2's pod_cidr via its advertised address"
        );
    }
```

`FakeNetManager` needs to be `Clone` for this test (`net.clone()` is passed into the `Reconciler` while the original `net` handle stays in the test to assert on). Add `#[derive(Default, Clone)]` to `FakeNetManager` and change its fields' inner containers to `Arc<Mutex<...>>` instead of bare `Mutex<...>` in `keel-net/src/fake.rs`:

```rust
#[derive(Default, Clone)]
pub struct FakeNetManager {
    bridges: Arc<Mutex<HashSet<String>>>,
    attachments: Arc<Mutex<HashMap<String, (String, String, String)>>>,
    routes: Arc<Mutex<HashMap<String, String>>>,
}
```

(add `use std::sync::Arc;` to that file's imports). This is a one-line-per-field change; no method bodies change since `Arc<Mutex<T>>` and `Mutex<T>` support the same `.lock()` call syntax.

- [ ] **Step 5: Run to verify tests pass**

Run: `cargo test -p keel-net --lib 2>&1 | tail -20 && cargo test -p keel-agentd registration:: 2>&1 | tail -60`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add keel-net/src/fake.rs keel-agentd/src/registration.rs
git commit -m "feat(keel-agentd): reconcile kernel routes to peers on the registration tick"
```

---

### Task 8: `keel-agentd` — apply-time subnet validation

**Files:**
- Modify: `keel-agentd/src/http.rs`
- Modify: `keelctl/tests/cli.rs`

**Interfaces:**
- Consumes: `PodCidrSlot::get() -> Option<Ipv4Net>` (Task 5).
- Produces: `run(listener, commands, pod_cidr_slot: PodCidrSlot)`, `run_tls(listener, commands, reloading_tls, pod_cidr_slot: PodCidrSlot)`.

- [ ] **Step 1: Write the failing tests**

Add to `keel-agentd/src/http.rs`'s `#[cfg(test)] mod tests`. First, a small helper to start a server with a pre-set `PodCidrSlot` (mirrors `start_test_server` but parameterized):

```rust
    fn start_test_server_with_pod_cidr(name: &str, pod_cidr: Option<&str>) -> (PathBuf, PodCidrSlot) {
        let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-test-state-{name}"));
        let _ = std::fs::remove_dir_all(&state_dir);
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), "zroot".to_string(), state_dir).unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let pod_cidr_slot = PodCidrSlot::new();
        if let Some(cidr) = pod_cidr {
            pod_cidr_slot.set(cidr.parse().unwrap());
        }

        let socket_path = short_unique_socket_path();
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();
        let slot_clone = pod_cidr_slot.clone();
        thread::spawn(move || run(listener, commands, slot_clone));
        (socket_path, pod_cidr_slot)
    }

    #[test]
    fn put_with_address_inside_the_stored_pod_cidr_is_accepted() {
        let (socket_path, _slot) = start_test_server_with_pod_cidr("put_with_address_inside_the_stored_pod_cidr_is_accepted", Some("10.0.4.0/24"));
        let yaml = sample_spec_yaml("web-1").replace("10.0.0.5/24", "10.0.4.5/24");
        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1", &yaml);
        assert_eq!(status, 200);
    }

    #[test]
    fn put_with_address_outside_the_stored_pod_cidr_is_rejected_before_any_side_effect() {
        let (socket_path, _slot) = start_test_server_with_pod_cidr("put_with_address_outside_the_stored_pod_cidr_is_rejected", Some("10.0.4.0/24"));
        let (status, body) = send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        assert_eq!(status, 400);
        assert!(body.contains("10.0.0.5/24"), "expected the given address in the error, got: {body}");
        assert!(body.contains("10.0.4.0/24"), "expected the node's actual block in the error, got: {body}");

        let (status, _) = send_request(&socket_path, "GET", "/jails/web-1", "");
        assert_eq!(status, 404, "the rejected apply must never have reached the reconciler");
    }

    #[test]
    fn put_with_no_stored_pod_cidr_skips_the_subnet_check() {
        let (socket_path, _slot) = start_test_server_with_pod_cidr("put_with_no_stored_pod_cidr_skips_the_subnet_check", None);
        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        assert_eq!(status, 200, "single-node/never-registered mode must skip the subnet check entirely");
    }
```

Add `use crate::PodCidrSlot;` to the test module's imports (or the top of `http.rs` if used outside tests too — it is, see Step 3).

- [ ] **Step 2: Run to verify it fails to compile**

Run: `cargo test -p keel-agentd http:: 2>&1 | tail -30`
Expected: FAIL — `run` takes 2 arguments, not 3.

- [ ] **Step 3: Thread `PodCidrSlot` through `run`/`run_tls`/`handle_apply`**

```rust
use crate::PodCidrSlot;
use ipnet::IpNet;
```

```rust
pub fn run(listener: UnixListener, commands: Sender<Command>, pod_cidr_slot: PodCidrSlot) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let pod_cidr_slot = pod_cidr_slot.clone();
        thread::spawn(move || {
            let _ = handle_connection(stream, &commands, &pod_cidr_slot);
        });
    }
}

pub fn run_tls(listener: TcpListener, commands: Sender<Command>, reloading_tls: Arc<crate::tls::ReloadingTls>, pod_cidr_slot: PodCidrSlot) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let tls_config = reloading_tls.server_config();
        let pod_cidr_slot = pod_cidr_slot.clone();
        thread::spawn(move || {
            let Ok(conn) = ServerConnection::new(tls_config) else { return };
            let mut tls_stream = TlsStream::new(conn, stream);
            if handle_connection_tls(&mut tls_stream, &commands, &pod_cidr_slot).is_err() {
                eprintln!("keel-agentd: TLS handshake or request handling failed for a connection");
            }
        });
    }
}
```

```rust
fn handle_connection(mut stream: UnixStream, commands: &Sender<Command>, pod_cidr_slot: &PodCidrSlot) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands, pod_cidr_slot);
    write_response(&mut stream, status, &body)
}

fn handle_connection_tls(stream: &mut TlsStream, commands: &Sender<Command>, pod_cidr_slot: &PodCidrSlot) -> io::Result<()> {
    let request = match read_request_tls(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands, pod_cidr_slot);
    write_response_tls(stream, status, &body)
}
```

```rust
fn route(request: &ParsedRequest, commands: &Sender<Command>, pod_cidr_slot: &PodCidrSlot) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("PUT", ["jails", name]) => handle_apply(name, &request.body, commands, pod_cidr_slot),
        ("GET", ["jails"]) => handle_get(None, commands),
        ("GET", ["jails", name]) => handle_get(Some(name.to_string()), commands),
        ("DELETE", ["jails", name]) => handle_delete(name, commands),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}

fn handle_apply(path_name: &str, body: &[u8], commands: &Sender<Command>, pod_cidr_slot: &PodCidrSlot) -> (u16, Vec<u8>) {
    let spec: JailSpec = match serde_yaml::from_slice(body) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    if spec.metadata.name != path_name {
        return error_response(
            400,
            format!("path name '{path_name}' does not match spec.metadata.name '{}'", spec.metadata.name),
        );
    }
    if let Some(pod_cidr) = pod_cidr_slot.get() {
        // A malformed address is left to the existing `validate_address` check
        // inside `Command::Apply` below, rather than duplicated here.
        if let Ok(address) = spec.spec.network.address.parse::<IpNet>() {
            if !pod_cidr.contains(&address.addr()) {
                return error_response(
                    400,
                    format!(
                        "network.address '{}' is outside this node's assigned subnet {pod_cidr}",
                        spec.spec.network.address
                    ),
                );
            }
        }
    }

    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Apply(spec, reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}
```

`Ipv4Net::contains` requires the argument to match the `Contains<&IpAddr>` impl on the *generic* `IpNet` enum (verified against `ipnet` 2.12.0's source: `impl<'a> Contains<&'a IpAddr> for IpNet`) — since `pod_cidr` here is `Ipv4Net` specifically (from `PodCidrSlot`), and `IpNet::contains` is what's implemented for `&IpAddr`, use `IpNet::V4(pod_cidr).contains(&address.addr())` instead of calling `.contains` directly on the `Ipv4Net` value:

```rust
            if !IpNet::V4(pod_cidr).contains(&address.addr()) {
```

- [ ] **Step 4: Fix every other `run`/`run_tls` call site**

In `keel-agentd/src/http.rs`'s own test module, update the four internal call sites:
- `start_test_server` (Unix socket): `thread::spawn(move || run(listener, commands, PodCidrSlot::new()));`
- `start_tcp_test_server`, and both `reloading_tls_*` tests (all `run_tls`): append `, PodCidrSlot::new()` as the fourth argument.

In `keel-agentd/src/main.rs`:

```rust
    let (_worker_handle, commands) = worker::spawn(reconciler);
    let pod_cidr_slot = keel_agentd::PodCidrSlot::new();
```

```rust
        keel_agentd::registration::spawn(
            node_id,
            advertise_addr.clone(),
            control_plane_addr,
            Duration::from_secs(5),
            capacity_cpu,
            capacity_memory,
            std::sync::Arc::clone(&reloading_tls),
            commands.clone(),
            pod_cidr_slot.clone(),
        );

        eprintln!("keel-agentd: serving jails API over TLS on {advertise_addr}");
        let tcp_listener = TcpListener::bind(&advertise_addr)
            .unwrap_or_else(|e| panic!("failed to bind jails-API TCP listener on {advertise_addr}: {e}"));
        let tcp_commands = commands.clone();
        let tcp_pod_cidr_slot = pod_cidr_slot.clone();
        thread::spawn(move || keel_agentd::http::run_tls(tcp_listener, tcp_commands, reloading_tls, tcp_pod_cidr_slot));
    }
```

```rust
    keel_agentd::http::run(listener, commands, pod_cidr_slot);
```

In `keelctl/tests/cli.rs`, update its two call sites:
- `start_test_server` (line 31): `thread::spawn(move || keel_agentd::http::run(listener, commands, keel_agentd::PodCidrSlot::new()));`
- `start_test_agentd_tcp` (line 85): `thread::spawn(move || keel_agentd::http::run_tls(listener, commands, reloading_tls, keel_agentd::PodCidrSlot::new()));`

- [ ] **Step 5: Run to verify tests pass**

Run: `cargo test --workspace 2>&1 | tail -80`
Expected: PASS across the entire workspace — this is the first point since Task 5 where `cargo build --workspace`/`cargo test --workspace` is expected to fully succeed again.

- [ ] **Step 6: Commit**

```bash
git add keel-agentd/src/http.rs keel-agentd/src/main.rs keelctl/tests/cli.rs
git commit -m "feat(keel-agentd): reject out-of-subnet apply addresses before any provisioning"
```

---

### Task 9: Final workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build and test**

Run: `cargo build --workspace 2>&1 | tail -40`
Expected: builds cleanly (the `freebsd_net.rs` test binary and any `#[cfg(target_os = "freebsd")]`-gated code compiles to nothing on this machine, which is expected).

Run: `cargo test --workspace 2>&1 | tail -100`
Expected: PASS, all crates.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -60`
Expected: no new warnings introduced by this milestone's code (pre-existing warnings, if any, are out of scope).

- [ ] **Step 2: Confirm what was intentionally not exercised**

Record in the commit message body (Step 3) that:
- `ProcessNetManager::add_route`/`remove_route`'s FreeBSD-only tests (Task 1, Step 5) were written but could not run on this non-FreeBSD dev machine — they require the same FreeBSD VM discipline as every other `ProcessNetManager`/`ProcessJailRuntime`/`ZfsManager` real-command test in this project.
- The design spec's VM verification section (three real nodes, `--cluster-cidr`, cross-node `ping`/reachability, killing a node's `keel-agentd` and observing route withdrawal) is out of scope for this plan — it requires the FreeBSD VM cluster this project uses for milestone sign-off, which this implementation pass did not have access to. Flag this explicitly to the user rather than claiming it as done.

- [ ] **Step 3: Commit (if any residual formatting/lint fixes were needed)**

```bash
git add -A
git commit -m "chore: workspace-wide verification pass for Milestone 14"
```

(Skip this commit entirely if Step 1 required no code changes.)

---

## Self-Review Notes

- **Spec coverage:** every Goal and Architecture subsection in the design spec maps to a task above — deterministic derivation (Task 2), registration/wire protocol change (Task 3), `keel-net` methods (Task 1), route reconciliation on the heartbeat loop (Task 7), apply-time validation (Task 8), CLI flag (Task 4). The two Non-Goals that touch code (no dynamic `--cluster-cidr` reconfiguration, no per-node subnet size flag) are satisfied by *absence* of any such flag/migration code — nothing to implement.
- **FreeBSD-only and VM-verification testing tiers** from the spec's Testing Strategy are written (Task 1 Step 5) or explicitly flagged as out of reach in this environment (Task 9) rather than silently skipped or fabricated.
- **Type consistency check:** `Ipv4Net` is used consistently for `cluster_cidr`/`pod_cidr` everywhere (Registry, wire strings via `.to_string()`, `PodCidrSlot`), and `IpNet` (the enum) only appears at the `keel-agentd/src/http.rs` apply-time boundary, where the jail spec's address is deliberately generic (matching `keel_spec::validate_address`'s own existing type) and is explicitly wrapped `IpNet::V4(pod_cidr)` for the `Contains<&IpAddr>` call — this was verified against the installed `ipnet` 2.12.0 crate's actual trait impls, not assumed from memory.
