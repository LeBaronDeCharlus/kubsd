pub mod error;
pub mod resources;
pub mod types;
pub mod validate;

pub use error::SpecError;
pub use resources::{cores_to_pcpu_percent, parse_cpu_cores, parse_memory_bytes};
pub use types::{
    JailSpec, JailTemplate, Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, ServiceSpec,
    ServiceSpecBody, Spec, TemplateNetworkSpec, VolumeMount,
};
pub use validate::{validate_address, validate_name, validate_transition, validate_volumes};

pub fn parse_and_validate(yaml: &str) -> Result<JailSpec, SpecError> {
    let spec: JailSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    validate::validate_address(&spec.spec.network.address)?;
    resources::parse_cpu_cores(&spec.spec.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.resources.memory)?;
    validate::validate_volumes(&spec.spec.volumes)?;
    Ok(spec)
}

pub fn parse_and_validate_service(yaml: &str) -> Result<ServiceSpec, SpecError> {
    let spec: ServiceSpec = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    validate::validate_name(&spec.metadata.name)?;
    resources::parse_cpu_cores(&spec.spec.template.resources.cpu)?;
    resources::parse_memory_bytes(&spec.spec.template.resources.memory)?;
    validate::validate_volumes(&spec.spec.template.volumes)?;
    if spec.spec.port == 0 {
        return Err(SpecError::InvalidPort(0));
    }
    Ok(spec)
}

/// Reads just the `kind` field out of a YAML document, without requiring the
/// rest of it to parse as any particular spec type — used by `keelctl` to
/// decide whether to parse the rest as a `JailSpec` or a `ServiceSpec`.
pub fn sniff_kind(yaml: &str) -> Result<String, SpecError> {
    #[derive(serde::Deserialize)]
    struct KindOnly {
        kind: String,
    }
    let sniff: KindOnly = serde_yaml::from_str(yaml).map_err(|e| SpecError::Yaml(e.to_string()))?;
    Ok(sniff.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parse_and_validate_service_accepts_a_well_formed_service() {
        let spec = parse_and_validate_service(VALID_SERVICE_YAML).unwrap();
        assert_eq!(spec.metadata.name, "web");
        assert_eq!(spec.spec.replicas, 2);
    }

    #[test]
    fn parse_and_validate_service_rejects_an_invalid_name() {
        let yaml = VALID_SERVICE_YAML.replace("name: web", "name: Invalid_Name");
        assert!(matches!(parse_and_validate_service(&yaml), Err(SpecError::InvalidName(_))));
    }

    #[test]
    fn parse_and_validate_service_rejects_invalid_resources() {
        let yaml = VALID_SERVICE_YAML.replace("cpu: \"1\"", "cpu: \"0\"");
        assert!(matches!(parse_and_validate_service(&yaml), Err(SpecError::InvalidCpu(_))));
    }

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

    #[test]
    fn sniff_kind_reads_jail() {
        let yaml = "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: web-1\n";
        assert_eq!(sniff_kind(yaml).unwrap(), "Jail");
    }

    #[test]
    fn sniff_kind_reads_service_without_needing_the_rest_of_the_document_to_parse_as_a_jail() {
        assert_eq!(sniff_kind(VALID_SERVICE_YAML).unwrap(), "Service");
    }

    #[test]
    fn sniff_kind_on_malformed_yaml_is_an_error() {
        assert!(sniff_kind("not: valid: yaml: [").is_err());
    }

    #[test]
    fn parse_and_validate_service_rejects_a_malformed_template_volume() {
        let yaml = VALID_SERVICE_YAML.replace(
            "    restartPolicy: Always\n",
            "    restartPolicy: Always\n    volumes:\n      - name: Invalid_Name\n        mountPath: /data\n        size: 1G\n",
        );
        assert!(matches!(parse_and_validate_service(&yaml), Err(SpecError::InvalidName(_))));
    }
}
