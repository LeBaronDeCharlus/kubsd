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

    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError> {
        let bridge_check = Self::run("ifconfig", &[bridge])?;
        if !bridge_check.status.success() {
            return Err(NetError::NotFound(bridge.to_string()));
        }

        let epair_a = format!("{epair_base}a");
        let epair_b = format!("{epair_base}b");

        let create = Self::run("ifconfig", &[epair_base, "create"])?;
        if !create.status.success() && !Self::stderr_contains(&create, "already exists") {
            return Err(NetError::CommandFailed(
                format!("ifconfig {epair_base} create"),
                create.status,
                String::from_utf8_lossy(&create.stderr).into_owned(),
            ));
        }

        let addm = Self::run("ifconfig", &[bridge, "addm", &epair_a])?;
        if !addm.status.success() && !Self::stderr_contains(&addm, "File exists") {
            return Err(NetError::CommandFailed(
                format!("ifconfig {bridge} addm {epair_a}"),
                addm.status,
                String::from_utf8_lossy(&addm.stderr).into_owned(),
            ));
        }

        Self::run_checked("ifconfig", &[&epair_a, "up"])?;

        let vnet_move = Self::run("ifconfig", &[&epair_b, "vnet", jail_name])?;
        if !vnet_move.status.success() {
            // Might already be moved from an interrupted prior attempt —
            // check whether it's already correctly placed in the target jail.
            let already_there = Self::run("jexec", &[jail_name, "/sbin/ifconfig", &epair_b])?;
            if !already_there.status.success() {
                return Err(NetError::CommandFailed(
                    format!("ifconfig {epair_b} vnet {jail_name}"),
                    vnet_move.status,
                    String::from_utf8_lossy(&vnet_move.stderr).into_owned(),
                ));
            }
        }

        Self::run_checked("jexec", &[jail_name, "/sbin/ifconfig", &epair_b, "inet", address])?;
        Self::run_checked("jexec", &[jail_name, "/sbin/ifconfig", &epair_b, "up"])
    }

    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError> {
        let epair_a = format!("{epair_base}a");
        let output = Self::run("ifconfig", &[&epair_a, "destroy"])?;
        if output.status.success() || Self::stderr_contains(&output, "does not exist") {
            Ok(())
        } else {
            Err(NetError::CommandFailed(
                format!("ifconfig {epair_a} destroy"),
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}
