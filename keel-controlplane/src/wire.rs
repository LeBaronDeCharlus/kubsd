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
        let heartbeat = Heartbeat { committed_cpu: 2.0, committed_memory: 1024 * 1024 * 1024 };
        let yaml = serde_yaml::to_string(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, heartbeat);
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
}
