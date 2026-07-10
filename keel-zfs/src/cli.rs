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
        // Immediately after a jail using this dataset as its rootfs is torn
        // down (`jail -r`), the kernel can take a brief moment to release
        // the mount's last references even though `jail -r` and the
        // process's own reaping have both already completed — `zfs
        // destroy` fails with "dataset is busy" in that narrow window.
        // Reproduced directly against the real VM during Milestone 5
        // verification (the busy state reliably clears within well under
        // a second). Retry briefly rather than failing a caller (like
        // `Reconciler::delete`) that chains this right after destroying
        // the owning jail.
        let mut last_err = None;
        for _ in 0..10 {
            match Self::run_checked(&["destroy", dataset]) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let is_busy = matches!(&e, ZfsError::CommandFailed(_, _, stderr) if stderr.contains("busy"));
                    last_err = Some(e);
                    if !is_busy {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        Err(last_err.unwrap())
    }

    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError> {
        let snapshot = format!("{base_dataset}@keel");
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
