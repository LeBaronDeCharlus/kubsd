pub mod cli;
pub mod error;
pub mod fake;

pub use cli::CliZfsManager;
pub use error::ZfsError;
pub use fake::FakeZfsManager;

pub trait ZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError>;

    /// The `target_dataset`'s parent must already exist — this method does
    /// not create parent datasets (no `-p`).
    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError>;
    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError>;
}
