use crate::JailError;
use crate::JailRuntime;
use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::Mutex;

pub struct ProcessJailRuntime {
    children: Mutex<Vec<Child>>,
}

impl ProcessJailRuntime {
    pub fn new() -> Self {
        Self { children: Mutex::new(Vec::new()) }
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, JailError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| JailError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), JailError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(JailError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn reap_finished_children(&self) {
        let mut children = self.children.lock().unwrap();
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
    }
}

impl Default for ProcessJailRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl JailRuntime for ProcessJailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError> {
        let path_arg = format!("path={}", rootfs.display());
        Self::run_checked("jail", &["-c", &format!("name={name}"), &path_arg, "persist"])
    }

    fn destroy(&self, name: &str) -> Result<(), JailError> {
        Self::run_checked("jail", &["-r", name])
    }

    fn is_running(&self, name: &str) -> Result<bool, JailError> {
        // Unlike `zfs list`, `jls` returns exit code 1 both when the jail
        // doesn't exist and on a usage error, so we can't distinguish them
        // by exit code. Since our own arguments here are fixed and known
        // to be valid, a usage error would indicate a code bug, not a
        // runtime condition — treating any failure as "doesn't exist" is
        // an accepted, deliberate trade-off, not an oversight.
        let jls = Self::run("jls", &["-j", name, "jid"])?;
        if !jls.status.success() {
            return Ok(false);
        }
        let jid = String::from_utf8_lossy(&jls.stdout).trim().to_string();
        if jid.is_empty() {
            return Ok(false);
        }
        let ps = Self::run("ps", &["-J", &jid, "-o", "state="])?;
        let has_live_process = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .any(|state| {
                let state = state.trim();
                !state.is_empty() && !state.starts_with('Z')
            });
        Ok(has_live_process)
    }

    fn start_command(&self, name: &str, command: &[String]) -> Result<(), JailError> {
        self.reap_finished_children();
        let mut cmd = Command::new("jexec");
        cmd.arg(name);
        cmd.args(command);
        let child = cmd.spawn().map_err(|e| JailError::Spawn("jexec".to_string(), e))?;
        self.children.lock().unwrap().push(child);
        Ok(())
    }

    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError> {
        Self::run_checked("rctl", &["-a", &format!("jail:{name}:pcpu:deny={pcpu_percent}")])?;
        Self::run_checked("rctl", &["-a", &format!("jail:{name}:vmemoryuse:deny={memory_bytes}")])
    }

    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError> {
        Self::run_checked("rctl", &["-r", &format!("jail:{name}")])
    }
}
