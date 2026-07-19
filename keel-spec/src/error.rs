use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum SpecError {
    #[error("failed to parse YAML: {0}")]
    Yaml(String),
    #[error("invalid jail name '{0}': must be 1-63 lowercase alphanumeric characters or hyphens, starting and ending with an alphanumeric character")]
    InvalidName(String),
    #[error("invalid network address '{0}': {1}")]
    InvalidAddress(String, String),
    #[error("invalid cpu value '{0}': must be a positive number of cores")]
    InvalidCpu(String),
    #[error("invalid memory value '{0}': expected a number optionally followed by K, M, or G")]
    InvalidMemory(String),
    #[error("invalid port {0}: must be non-zero")]
    InvalidPort(u16),
    #[error("duplicate volume name '{0}' in spec.volumes")]
    DuplicateVolumeName(String),
    #[error("field '{0}' cannot be changed after the jail is created; delete and re-apply instead")]
    ImmutableField(&'static str),
}
