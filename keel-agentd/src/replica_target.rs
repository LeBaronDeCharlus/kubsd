use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaTarget {
    pub replica_name: String,
    pub volume_dataset: String,
    pub source_node_addr: String,
    pub last_snapshot: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_target_round_trips_through_yaml() {
        let target = ReplicaTarget {
            replica_name: "db-0".to_string(),
            volume_dataset: "zroot/keel/volumes/db-0-data".to_string(),
            source_node_addr: "10.0.0.4:7621".to_string(),
            last_snapshot: None,
        };
        let yaml = serde_yaml::to_string(&target).unwrap();
        let parsed: ReplicaTarget = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, target);
    }
}
