use thiserror::Error;

#[derive(Debug, Error)]
pub enum JailError {
    #[error("failed to spawn `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed with exit status {1}: {2}")]
    CommandFailed(String, std::process::ExitStatus, String),
    #[error("jail '{0}' not found")]
    NotFound(String),
}
