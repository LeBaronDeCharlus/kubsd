use crate::wire::{NodeState, NodeStatus};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEAD_THRESHOLD: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct NodeRecord {
    addr: String,
    last_heartbeat: Instant,
}

#[derive(Debug, Default)]
pub struct Registry {
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

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: String, addr: String, now: Instant) {
        self.nodes.insert(id, NodeRecord { addr, last_heartbeat: now });
    }

    pub fn heartbeat(&mut self, id: &str, now: Instant) -> Result<(), UnknownNode> {
        match self.nodes.get_mut(id) {
            Some(record) => {
                record.last_heartbeat = now;
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
                    status: if elapsed < DEAD_THRESHOLD { NodeState::Alive } else { NodeState::Dead },
                    last_seen_secs: elapsed.as_secs(),
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

    #[test]
    fn register_then_list_shows_the_node_as_alive() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-1".to_string(), "192.168.64.4".to_string(), now);

        let statuses = registry.list(now);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-1");
        assert_eq!(statuses[0].addr, "192.168.64.4");
        assert_eq!(statuses[0].status, NodeState::Alive);
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn reregistering_an_existing_id_refreshes_its_address_and_heartbeat() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let t1 = t0 + Duration::from_secs(5);
        registry.register("node-1".to_string(), "10.0.0.2".to_string(), t1);

        let statuses = registry.list(t1);
        assert_eq!(statuses.len(), 1, "re-registering must not create a second entry");
        assert_eq!(statuses[0].addr, "10.0.0.2");
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn heartbeat_on_a_known_node_updates_its_last_heartbeat() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let t1 = t0 + Duration::from_secs(10);
        registry.heartbeat("node-1", t1).unwrap();

        let statuses = registry.list(t1);
        assert_eq!(statuses[0].last_seen_secs, 0);
    }

    #[test]
    fn heartbeat_on_an_unknown_node_returns_unknown_node_error() {
        let mut registry = Registry::new();
        let err = registry.heartbeat("missing", Instant::now()).unwrap_err();
        assert_eq!(err, UnknownNode("missing".to_string()));
        assert_eq!(err.to_string(), "unknown node 'missing'");
    }

    #[test]
    fn list_reports_dead_once_a_node_exceeds_the_dead_threshold() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let just_under = t0 + Duration::from_secs(14);
        assert_eq!(registry.list(just_under)[0].status, NodeState::Alive);

        let at_threshold = t0 + DEAD_THRESHOLD;
        assert_eq!(registry.list(at_threshold)[0].status, NodeState::Dead);
    }

    #[test]
    fn list_is_sorted_by_id() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-2".to_string(), "10.0.0.2".to_string(), now);
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), now);

        let statuses = registry.list(now);
        assert_eq!(statuses.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["node-1", "node-2"]);
    }

    #[test]
    fn list_on_an_empty_registry_is_empty() {
        let registry = Registry::new();
        assert_eq!(registry.list(Instant::now()), vec![]);
    }

    #[test]
    fn resolve_on_an_unknown_node_returns_unknown_error() {
        let registry = Registry::new();
        let err = registry.resolve("missing", Instant::now()).unwrap_err();
        assert_eq!(err, ResolveError::Unknown("missing".to_string()));
    }

    #[test]
    fn resolve_on_an_alive_node_returns_its_address() {
        let mut registry = Registry::new();
        let now = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), now);
        assert_eq!(registry.resolve("node-1", now), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_on_a_dead_node_returns_dead_error_with_elapsed_seconds() {
        let mut registry = Registry::new();
        let t0 = Instant::now();
        registry.register("node-1".to_string(), "10.0.0.1".to_string(), t0);

        let at_threshold = t0 + DEAD_THRESHOLD;
        let err = registry.resolve("node-1", at_threshold).unwrap_err();
        assert_eq!(
            err,
            ResolveError::Dead { id: "node-1".to_string(), last_seen_secs: DEAD_THRESHOLD.as_secs() }
        );
    }
}
