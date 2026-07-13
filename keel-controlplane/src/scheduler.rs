use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no alive nodes available to schedule onto")]
    NoAvailableNodes,
}

pub fn pick_node(alive_ids: &[String], counts: &HashMap<&str, usize>) -> Result<String, ScheduleError> {
    alive_ids
        .iter()
        .min_by_key(|id| (counts.get(id.as_str()).copied().unwrap_or(0), (*id).clone()))
        .cloned()
        .ok_or(ScheduleError::NoAvailableNodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_alive_nodes_returns_no_available_nodes_error() {
        let alive_ids: Vec<String> = vec![];
        let counts = HashMap::new();
        assert_eq!(pick_node(&alive_ids, &counts), Err(ScheduleError::NoAvailableNodes));
    }

    #[test]
    fn a_single_alive_node_is_always_picked() {
        let alive_ids = vec!["node-1".to_string()];
        let counts = HashMap::new();
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-1".to_string()));
    }

    #[test]
    fn the_node_with_the_fewest_recorded_jails_wins() {
        let alive_ids = vec!["node-1".to_string(), "node-2".to_string()];
        let mut counts = HashMap::new();
        counts.insert("node-1", 3);
        counts.insert("node-2", 1);
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-2".to_string()));
    }

    #[test]
    fn ties_are_broken_by_ascending_node_id() {
        let alive_ids = vec!["node-2".to_string(), "node-1".to_string()];
        let counts = HashMap::new();
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-1".to_string()));
    }

    #[test]
    fn a_dead_node_with_a_lower_count_is_never_picked_since_it_is_absent_from_alive_ids() {
        let alive_ids = vec!["node-2".to_string()];
        let mut counts = HashMap::new();
        counts.insert("node-1", 0);
        counts.insert("node-2", 5);
        assert_eq!(pick_node(&alive_ids, &counts), Ok("node-2".to_string()));
    }
}
