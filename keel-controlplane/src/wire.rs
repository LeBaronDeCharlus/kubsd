use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub id: String,
    pub addr: String,
    pub capacity_cpu: f64,
    pub capacity_memory: u64,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum NodeState {
    Alive,
    Dead,
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_registration_round_trips_through_yaml() {
        let registration = NodeRegistration {
            id: "node-1".to_string(),
            addr: "192.168.64.4".to_string(),
            capacity_cpu: 4.0,
            capacity_memory: 8 * 1024 * 1024 * 1024,
        };
        let yaml = serde_yaml::to_string(&registration).unwrap();
        let parsed: NodeRegistration = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, registration);
    }

    #[test]
    fn node_status_round_trips_through_yaml() {
        let status = NodeStatus {
            id: "node-1".to_string(),
            addr: "192.168.64.4".to_string(),
            pod_cidr: "10.0.4.0/24".to_string(),
            status: NodeState::Alive,
            last_seen_secs: 3,
            capacity_cpu: 4.0,
            capacity_memory: 8 * 1024 * 1024 * 1024,
            committed_cpu: 1.5,
            committed_memory: 512 * 1024 * 1024,
        };
        let yaml = serde_yaml::to_string(&status).unwrap();
        let parsed: NodeStatus = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn register_response_round_trips_through_yaml() {
        let response = RegisterResponse { pod_cidr: "10.0.4.0/24".to_string() };
        let yaml = serde_yaml::to_string(&response).unwrap();
        let parsed: RegisterResponse = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn heartbeat_round_trips_through_yaml() {
        let heartbeat = Heartbeat { committed_cpu: 2.0, committed_memory: 1024 * 1024 * 1024, jails: vec![] };
        let yaml = serde_yaml::to_string(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, heartbeat);
    }

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

    #[test]
    fn node_state_dead_round_trips_through_yaml() {
        let yaml = serde_yaml::to_string(&NodeState::Dead).unwrap();
        let parsed: NodeState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, NodeState::Dead);
    }

    #[test]
    fn error_body_round_trips_through_yaml() {
        let body = ErrorBody { error: "unknown node 'node-9'".to_string() };
        let yaml = serde_yaml::to_string(&body).unwrap();
        let parsed: ErrorBody = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, body);
    }

    #[test]
    fn service_replica_round_trips_through_yaml() {
        let replica = ServiceReplica { name: "web-0".to_string(), node: "node-4".to_string(), address: "10.0.60.2".to_string() };
        let yaml = serde_yaml::to_string(&replica).unwrap();
        assert_eq!(serde_yaml::from_str::<ServiceReplica>(&yaml).unwrap(), replica);
    }

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
}
