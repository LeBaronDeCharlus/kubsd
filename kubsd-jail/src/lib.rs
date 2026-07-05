pub mod error;
pub mod fake;

pub use error::JailError;
pub use fake::FakeJailRuntime;

use std::path::Path;

pub trait JailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError>;
    fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError>;
    fn destroy(&self, name: &str) -> Result<(), JailError>;
    fn is_running(&self, name: &str) -> Result<bool, JailError>;
    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError>;
    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError>;
}
