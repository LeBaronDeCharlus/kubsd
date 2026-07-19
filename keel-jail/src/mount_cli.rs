use crate::MountError;
use crate::MountManager;
use std::path::Path;
use std::process::{Command, Output};

pub struct CliMountManager;

impl CliMountManager {
    pub fn new() -> Self {
        Self
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, MountError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| MountError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), MountError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(MountError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

impl Default for CliMountManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MountManager for CliMountManager {
    fn mount_nullfs(&self, source: &Path, target: &Path) -> Result<(), MountError> {
        Self::run_checked("mount", &["-t", "nullfs", &source.to_string_lossy(), &target.to_string_lossy()])
    }

    fn unmount(&self, target: &Path) -> Result<(), MountError> {
        let output = Self::run("umount", &[&target.to_string_lossy()])?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // FreeBSD's `umount` prints `umount: <path>: not currently mounted`
        // and exits non-zero for a target that isn't mounted — the same
        // "already in the desired state" tolerance `Reconciler::delete`
        // needs for volumes it never got as far as mounting.
        if stderr.contains("not currently mounted") {
            return Err(MountError::NotMounted(target.to_path_buf()));
        }
        Err(MountError::CommandFailed(
            format!("umount {}", target.display()),
            output.status,
            stderr.into_owned(),
        ))
    }

    fn is_mounted(&self, target: &Path) -> Result<bool, MountError> {
        let output = Self::run("mount", &["-p"])?;
        if !output.status.success() {
            return Err(MountError::CommandFailed(
                "mount -p".to_string(),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let target_str = target.to_string_lossy();
        // `mount -p`'s output is tab-separated: `device  mountpoint  fstype ...`.
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split('\t').nth(1) == Some(target_str.as_ref())))
    }
}
