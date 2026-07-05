pub mod error;
pub mod fake;
pub mod process;

pub use error::NetError;
pub use fake::FakeNetManager;
pub use process::ProcessNetManager;

pub trait NetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError>;
    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError>;
    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError>;
}
