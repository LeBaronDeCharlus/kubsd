use crate::ZfsError;
use crate::ZfsManager;
use std::process::{Command, Output};

pub struct CliZfsManager;

impl CliZfsManager {
    pub fn new() -> Self {
        Self
    }

    fn run(args: &[&str]) -> Result<Output, ZfsError> {
        Command::new("zfs")
            .args(args)
            .output()
            .map_err(|e| ZfsError::Spawn("zfs".to_string(), e))
    }

    fn run_checked(args: &[&str]) -> Result<(), ZfsError> {
        let output = Self::run(args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ZfsError::CommandFailed(
                format!("zfs {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

impl Default for CliZfsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ZfsManager for CliZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError> {
        let output = Self::run(&["list", "-H", "-o", "name", dataset])?;
        Ok(output.status.success())
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        Self::run_checked(&["destroy", dataset])
    }

    fn clone_from_base(&self, _base_dataset: &str, _target_dataset: &str) -> Result<(), ZfsError> {
        unimplemented!("added in Task 8")
    }
}
