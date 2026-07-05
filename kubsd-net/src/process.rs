use crate::NetError;
use crate::NetManager;
use std::process::{Command, Output};

pub struct ProcessNetManager;

impl ProcessNetManager {
    pub fn new() -> Self {
        Self
    }

    fn run(program: &str, args: &[&str]) -> Result<Output, NetError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| NetError::Spawn(program.to_string(), e))
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<(), NetError> {
        let output = Self::run(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("{program} {}", args.join(" ")),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn stderr_contains(output: &Output, needle: &str) -> bool {
        String::from_utf8_lossy(&output.stderr).contains(needle)
    }
}

impl Default for ProcessNetManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NetManager for ProcessNetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError> {
        let check = Self::run("ifconfig", &[bridge])?;
        if check.status.success() {
            return Ok(());
        }
        let created = Self::run("ifconfig", &["bridge", "create"])?;
        if !created.status.success() {
            return Err(NetError::CommandFailed(
                "ifconfig bridge create".to_string(),
                created.status,
                String::from_utf8_lossy(&created.stderr).into_owned(),
            ));
        }
        let created_name = String::from_utf8_lossy(&created.stdout).trim().to_string();
        Self::run_checked("ifconfig", &[&created_name, "name", bridge])?;
        Self::run_checked("ifconfig", &[bridge, "up"])
    }

    fn attach_jail(&self, _jail_name: &str, _bridge: &str, _epair_base: &str, _address: &str) -> Result<(), NetError> {
        unimplemented!("added in Task 4")
    }

    fn detach_jail(&self, _epair_base: &str) -> Result<(), NetError> {
        unimplemented!("added in Task 5")
    }
}
