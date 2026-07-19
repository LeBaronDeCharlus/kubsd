use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MountError {
    #[error("failed to spawn `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed with exit status {1}: {2}")]
    CommandFailed(String, std::process::ExitStatus, String),
    #[error("'{0}' is not currently mounted")]
    NotMounted(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
