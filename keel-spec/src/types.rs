use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JailSpec {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub image: String,
    pub command: Vec<String>,
    pub network: NetworkSpec,
    pub resources: ResourcesSpec,
    #[serde(rename = "restartPolicy")]
    pub restart_policy: RestartPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkSpec {
    pub vnet: bool,
    pub bridge: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourcesSpec {
    pub cpu: String,
    pub memory: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_YAML: &str = r#"
apiVersion: keel/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.0.5/24
  resources:
    cpu: "2"
    memory: "512M"
  restartPolicy: Always
"#;

    #[test]
    fn parses_the_design_spec_example_yaml() {
        let spec: JailSpec = serde_yaml::from_str(EXAMPLE_YAML).unwrap();
        assert_eq!(spec.api_version, "keel/v1");
        assert_eq!(spec.kind, "Jail");
        assert_eq!(spec.metadata.name, "web-1");
        assert_eq!(spec.spec.image, "base/14.2-web");
        assert_eq!(spec.spec.command, vec!["/usr/local/bin/myapp".to_string()]);
        assert!(spec.spec.network.vnet);
        assert_eq!(spec.spec.network.bridge, "keel0");
        assert_eq!(spec.spec.network.address, "10.0.0.5/24");
        assert_eq!(spec.spec.resources.cpu, "2");
        assert_eq!(spec.spec.resources.memory, "512M");
        assert_eq!(spec.spec.restart_policy, RestartPolicy::Always);
    }
}
