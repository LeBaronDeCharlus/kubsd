use crate::record::JailRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailStatus {
    pub record: JailRecord,
    pub running: bool,
    pub backoff: BackoffStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackoffStatus {
    pub retry_in_secs: Option<u64>,
    pub current_delay_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{JailSpec, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};

    fn sample_record() -> JailRecord {
        JailRecord {
            spec: JailSpec {
                api_version: "keel/v1".to_string(),
                kind: "Jail".to_string(),
                metadata: Metadata { name: "web-1".to_string() },
                spec: Spec {
                    image: "base/14.2-web".to_string(),
                    command: vec!["/usr/local/bin/myapp".to_string()],
                    network: NetworkSpec {
                        vnet: true,
                        bridge: "keel0".to_string(),
                        address: "10.0.0.5/24".to_string(),
                    },
                    resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                    restart_policy: RestartPolicy::Always,
                },
            },
            epair_ordinal: 1,
        }
    }

    #[test]
    fn jail_status_round_trips_through_yaml() {
        let status = JailStatus {
            record: sample_record(),
            running: true,
            backoff: BackoffStatus { retry_in_secs: Some(4), current_delay_secs: Some(8) },
        };
        let yaml = serde_yaml::to_string(&status).unwrap();
        let parsed: JailStatus = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn backoff_status_default_is_no_cooldown() {
        let status = BackoffStatus::default();
        assert_eq!(status.retry_in_secs, None);
        assert_eq!(status.current_delay_secs, None);
    }

    #[test]
    fn error_body_round_trips_through_yaml() {
        let body = ErrorBody { error: "jail 'web-1' not found in desired state".to_string() };
        let yaml = serde_yaml::to_string(&body).unwrap();
        let parsed: ErrorBody = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, body);
    }
}
