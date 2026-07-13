use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct Placements {
    by_jail: HashMap<String, String>,
}

impl Placements {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, jail_name: &str) -> Option<&str> {
        self.by_jail.get(jail_name).map(|s| s.as_str())
    }

    pub fn set(&mut self, jail_name: String, node_id: String) {
        self.by_jail.insert(jail_name, node_id);
    }

    pub fn remove(&mut self, jail_name: &str) {
        self.by_jail.remove(jail_name);
    }

    /// node_id -> number of jails currently recorded against it.
    pub fn counts(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for node_id in self.by_jail.values() {
            *counts.entry(node_id.as_str()).or_insert(0) += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_on_an_empty_table_returns_none() {
        let placements = Placements::new();
        assert_eq!(placements.get("web-1"), None);
    }

    #[test]
    fn set_then_get_returns_the_recorded_node() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        assert_eq!(placements.get("web-1"), Some("node-1"));
    }

    #[test]
    fn set_again_on_the_same_jail_overwrites_rather_than_duplicating() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.set("web-1".to_string(), "node-2".to_string());
        assert_eq!(placements.get("web-1"), Some("node-2"));
        assert_eq!(placements.counts().get("node-1"), None);
        assert_eq!(placements.counts().get("node-2"), Some(&1));
    }

    #[test]
    fn remove_clears_the_placement() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.remove("web-1");
        assert_eq!(placements.get("web-1"), None);
    }

    #[test]
    fn counts_aggregates_multiple_jails_on_the_same_node() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        placements.set("web-2".to_string(), "node-1".to_string());
        placements.set("web-3".to_string(), "node-2".to_string());
        let counts = placements.counts();
        assert_eq!(counts.get("node-1"), Some(&2));
        assert_eq!(counts.get("node-2"), Some(&1));
    }

    #[test]
    fn counts_on_an_empty_table_is_empty() {
        let placements = Placements::new();
        assert_eq!(placements.counts(), HashMap::new());
    }
}
