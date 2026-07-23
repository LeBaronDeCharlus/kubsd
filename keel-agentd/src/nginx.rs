use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NginxError {
    #[error("failed to write nginx config: {0}")]
    Write(String),
    #[error("nginx -t validation failed: {0}")]
    ValidationFailed(String),
    #[error("nginx -s reload failed: {0}")]
    ReloadFailed(String),
}

pub trait NginxController {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError>;
    fn test_config(&self, jail_name: &str) -> Result<(), NginxError>;
    fn reload(&self, jail_name: &str) -> Result<(), NginxError>;
}

#[derive(Default)]
pub struct FakeNginxController {
    written: Mutex<std::collections::HashMap<String, String>>,
    fail_test: Mutex<bool>,
    reload_count: Mutex<std::collections::HashMap<String, u32>>,
}

impl FakeNginxController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail_test(&self, fail: bool) {
        *self.fail_test.lock().unwrap() = fail;
    }

    pub fn last_written_config(&self, jail_name: &str) -> Option<String> {
        self.written.lock().unwrap().get(jail_name).cloned()
    }

    pub fn reload_count(&self, jail_name: &str) -> u32 {
        *self.reload_count.lock().unwrap().get(jail_name).unwrap_or(&0)
    }
}

impl NginxController for FakeNginxController {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError> {
        self.written.lock().unwrap().insert(jail_name.to_string(), config.to_string());
        Ok(())
    }

    fn test_config(&self, _jail_name: &str) -> Result<(), NginxError> {
        if *self.fail_test.lock().unwrap() {
            return Err(NginxError::ValidationFailed("simulated nginx -t failure".to_string()));
        }
        Ok(())
    }

    fn reload(&self, jail_name: &str) -> Result<(), NginxError> {
        *self.reload_count.lock().unwrap().entry(jail_name.to_string()).or_insert(0) += 1;
        Ok(())
    }
}

impl NginxController for std::sync::Arc<FakeNginxController> {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError> {
        (**self).write_config(jail_name, config)
    }
    fn test_config(&self, jail_name: &str) -> Result<(), NginxError> {
        (**self).test_config(jail_name)
    }
    fn reload(&self, jail_name: &str) -> Result<(), NginxError> {
        (**self).reload(jail_name)
    }
}

pub struct JexecNginxController {
    pool: String,
}

impl JexecNginxController {
    pub fn new(pool: String) -> Self {
        Self { pool }
    }

    fn run_jexec(&self, jail_name: &str, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
        std::process::Command::new("jexec").arg(jail_name).args(args).output()
    }
}

impl NginxController for JexecNginxController {
    fn write_config(&self, jail_name: &str, config: &str) -> Result<(), NginxError> {
        let spec_name = jail_name.strip_prefix("keel-").unwrap_or(jail_name);
        let rootfs = crate::record::jail_rootfs_path(&self.pool, spec_name);
        let config_dir = rootfs.join("usr/local/etc/nginx");
        std::fs::create_dir_all(&config_dir).map_err(|e| NginxError::Write(e.to_string()))?;
        let final_path = config_dir.join("nginx.conf");
        let tmp_path = config_dir.join("nginx.conf.tmp");
        std::fs::write(&tmp_path, config).map_err(|e| NginxError::Write(e.to_string()))?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| NginxError::Write(e.to_string()))?;
        Ok(())
    }

    fn test_config(&self, jail_name: &str) -> Result<(), NginxError> {
        let output = self.run_jexec(jail_name, &["/usr/local/sbin/nginx", "-t"]).map_err(|e| NginxError::ValidationFailed(e.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NginxError::ValidationFailed(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }

    fn reload(&self, jail_name: &str) -> Result<(), NginxError> {
        let output = self.run_jexec(jail_name, &["/usr/local/sbin/nginx", "-s", "reload"]).map_err(|e| NginxError::ReloadFailed(e.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NginxError::ReloadFailed(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_config_then_last_written_config_returns_it() {
        let nginx = FakeNginxController::new();
        nginx.write_config("keel-ingress", "config-v1").unwrap();
        assert_eq!(nginx.last_written_config("keel-ingress"), Some("config-v1".to_string()));
    }

    #[test]
    fn test_config_fails_when_set_to_fail() {
        let nginx = FakeNginxController::new();
        nginx.set_fail_test(true);
        assert!(nginx.test_config("keel-ingress").is_err());
    }

    #[test]
    fn reload_count_increments_on_each_call() {
        let nginx = FakeNginxController::new();
        nginx.reload("keel-ingress").unwrap();
        nginx.reload("keel-ingress").unwrap();
        assert_eq!(nginx.reload_count("keel-ingress"), 2);
    }
}
