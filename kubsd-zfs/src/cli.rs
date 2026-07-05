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
        if output.status.success() {
            return Ok(true);
        }
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        Err(ZfsError::CommandFailed(
            format!("zfs list -H -o name {dataset}"),
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        Self::run_checked(&["destroy", dataset])
    }

    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError> {
        let snapshot = format!("{base_dataset}@kubsd");
        if !self.dataset_exists(&snapshot)? {
            if let Err(e) = Self::run_checked(&["snapshot", &snapshot]) {
                // Lost a race with a concurrent caller cloning the same base:
                // if the snapshot exists now anyway, proceed; otherwise this
                // was a real failure (e.g. the base dataset doesn't exist).
                if !self.dataset_exists(&snapshot)? {
                    return Err(e);
                }
            }
        }
        Self::run_checked(&["clone", &snapshot, target_dataset])
    }
}
