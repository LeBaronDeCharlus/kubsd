use crate::JailError;
use crate::JailRuntime;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct FakeJail {
    #[allow(dead_code)]
    rootfs: PathBuf,
    running: bool,
    pcpu_percent: Option<u32>,
    memory_bytes: Option<u64>,
}

#[derive(Default)]
pub struct FakeJailRuntime {
    jails: Mutex<HashMap<String, FakeJail>>,
}

impl FakeJailRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

impl JailRuntime for FakeJailRuntime {
    fn create(&self, name: &str, rootfs: &Path) -> Result<(), JailError> {
        self.jails.lock().unwrap().insert(
            name.to_string(),
            FakeJail { rootfs: rootfs.to_path_buf(), running: false, pcpu_percent: None, memory_bytes: None },
        );
        Ok(())
    }

    fn start_command(&self, name: &str, _command: &[String]) -> Result<(), JailError> {
        let mut jails = self.jails.lock().unwrap();
        let jail = jails.get_mut(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        jail.running = true;
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), JailError> {
        self.jails.lock().unwrap().remove(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        Ok(())
    }

    fn is_running(&self, name: &str) -> Result<bool, JailError> {
        Ok(self.jails.lock().unwrap().get(name).map(|j| j.running).unwrap_or(false))
    }

    fn set_resource_limits(&self, name: &str, pcpu_percent: u32, memory_bytes: u64) -> Result<(), JailError> {
        let mut jails = self.jails.lock().unwrap();
        let jail = jails.get_mut(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        jail.pcpu_percent = Some(pcpu_percent);
        jail.memory_bytes = Some(memory_bytes);
        Ok(())
    }

    fn remove_resource_limits(&self, name: &str) -> Result<(), JailError> {
        let mut jails = self.jails.lock().unwrap();
        let jail = jails.get_mut(name).ok_or_else(|| JailError::NotFound(name.to_string()))?;
        jail.pcpu_percent = None;
        jail.memory_bytes = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_is_running_is_false_until_start_command() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        assert_eq!(runtime.is_running("test-1").unwrap(), false);
    }

    #[test]
    fn start_command_makes_is_running_true() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        runtime.start_command("test-1", &["/bin/sh".to_string()]).unwrap();
        assert_eq!(runtime.is_running("test-1").unwrap(), true);
    }

    #[test]
    fn destroy_removes_the_jail() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        runtime.destroy("test-1").unwrap();
        assert_eq!(runtime.is_running("test-1").unwrap(), false);
    }

    #[test]
    fn operations_on_unknown_jail_return_not_found() {
        let runtime = FakeJailRuntime::new();
        assert!(matches!(runtime.start_command("missing", &[]), Err(JailError::NotFound(_))));
        assert!(matches!(runtime.destroy("missing"), Err(JailError::NotFound(_))));
        assert!(matches!(runtime.set_resource_limits("missing", 100, 1024), Err(JailError::NotFound(_))));
        assert!(matches!(runtime.remove_resource_limits("missing"), Err(JailError::NotFound(_))));
    }

    #[test]
    fn set_and_remove_resource_limits_do_not_error_on_known_jail() {
        let runtime = FakeJailRuntime::new();
        runtime.create("test-1", Path::new("/tmp/rootfs")).unwrap();
        runtime.set_resource_limits("test-1", 200, 512 * 1024 * 1024).unwrap();
        runtime.remove_resource_limits("test-1").unwrap();
    }
}
