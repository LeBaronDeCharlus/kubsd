use crate::ZfsError;
use crate::ZfsManager;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::sync::{Arc, Mutex};

#[derive(Default, Clone)]
pub struct FakeZfsManager {
    datasets: Arc<Mutex<HashSet<String>>>,
    snapshots: Arc<Mutex<HashSet<String>>>,
    busy: Arc<Mutex<HashSet<String>>>,
}

impl FakeZfsManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: seed a base dataset as if it already existed on the pool.
    pub fn seed_dataset(&self, dataset: &str) {
        self.datasets.lock().unwrap().insert(dataset.to_string());
    }

    /// Test helper: makes `destroy_dataset` return `ZfsError::Busy` for
    /// this dataset instead of removing it — simulates a volume still
    /// nullfs-mounted by a running jail, since this in-memory fake has no
    /// real mount awareness of its own.
    pub fn mark_busy(&self, dataset: &str) {
        self.busy.lock().unwrap().insert(dataset.to_string());
    }
}

impl ZfsManager for FakeZfsManager {
    fn dataset_exists(&self, dataset: &str) -> Result<bool, ZfsError> {
        Ok(self.datasets.lock().unwrap().contains(dataset))
    }

    fn clone_from_base(&self, base_dataset: &str, target_dataset: &str) -> Result<(), ZfsError> {
        let datasets = self.datasets.lock().unwrap();
        if !datasets.contains(base_dataset) {
            return Err(ZfsError::NotFound(base_dataset.to_string()));
        }
        drop(datasets);
        self.datasets.lock().unwrap().insert(target_dataset.to_string());
        Ok(())
    }

    fn create_volume(&self, dataset: &str, _quota: &str) -> Result<(), ZfsError> {
        self.datasets.lock().unwrap().insert(dataset.to_string());
        Ok(())
    }

    fn destroy_dataset(&self, dataset: &str) -> Result<(), ZfsError> {
        if self.busy.lock().unwrap().contains(dataset) {
            return Err(ZfsError::Busy(dataset.to_string()));
        }
        if self.datasets.lock().unwrap().remove(dataset) {
            Ok(())
        } else {
            Err(ZfsError::NotFound(dataset.to_string()))
        }
    }

    fn snapshot(&self, dataset: &str, snapshot: &str) -> Result<(), ZfsError> {
        if !self.datasets.lock().unwrap().contains(dataset) {
            return Err(ZfsError::NotFound(dataset.to_string()));
        }
        self.snapshots.lock().unwrap().insert(format!("{dataset}@{snapshot}"));
        Ok(())
    }

    fn send_snapshot(&self, dataset: &str, snapshot: &str, base: Option<&str>, out: &mut dyn Write) -> Result<(), ZfsError> {
        let key = format!("{dataset}@{snapshot}");
        if !self.snapshots.lock().unwrap().contains(&key) {
            return Err(ZfsError::NotFound(key));
        }
        if let Some(base_snapshot) = base {
            let base_key = format!("{dataset}@{base_snapshot}");
            if !self.snapshots.lock().unwrap().contains(&base_key) {
                return Err(ZfsError::NotFound(base_key));
            }
        }
        let marker = format!(
            "keel-zfs-fake-send:{dataset}@{snapshot}:base={}\n",
            base.unwrap_or("none")
        );
        out.write_all(marker.as_bytes()).map_err(|e| ZfsError::Spawn("fake zfs send".to_string(), e))
    }

    fn receive_snapshot(&self, dataset: &str, input: &mut dyn Read) -> Result<(), ZfsError> {
        let mut buf = String::new();
        input.read_to_string(&mut buf).map_err(|e| ZfsError::Spawn("fake zfs receive".to_string(), e))?;
        if !buf.starts_with("keel-zfs-fake-send:") {
            return Err(ZfsError::CommandFailed(
                "zfs receive (fake)".to_string(),
                std::process::ExitStatus::from_raw(256),
                "malformed stream".to_string(),
            ));
        }
        self.datasets.lock().unwrap().insert(dataset.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_exists_is_false_until_seeded() {
        let zfs = FakeZfsManager::new();
        assert_eq!(zfs.dataset_exists("zroot/keel/base/test").unwrap(), false);
        zfs.seed_dataset("zroot/keel/base/test");
        assert_eq!(zfs.dataset_exists("zroot/keel/base/test").unwrap(), true);
    }

    #[test]
    fn clone_from_base_requires_existing_base() {
        let zfs = FakeZfsManager::new();
        assert!(matches!(
            zfs.clone_from_base("zroot/keel/base/test", "zroot/keel/jails/web-1"),
            Err(ZfsError::NotFound(_))
        ));
    }

    #[test]
    fn clone_from_base_creates_target_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/test");
        zfs.clone_from_base("zroot/keel/base/test", "zroot/keel/jails/web-1").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/keel/jails/web-1").unwrap(), true);
    }

    #[test]
    fn destroy_dataset_removes_it() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/jails/web-1");
        zfs.destroy_dataset("zroot/keel/jails/web-1").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/keel/jails/web-1").unwrap(), false);
    }

    #[test]
    fn destroy_dataset_on_unknown_dataset_returns_not_found() {
        let zfs = FakeZfsManager::new();
        assert!(matches!(zfs.destroy_dataset("zroot/keel/jails/missing"), Err(ZfsError::NotFound(_))));
    }

    #[test]
    fn create_volume_creates_the_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.create_volume("zroot/keel/volumes/web-data", "1G").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), true);
    }

    #[test]
    fn create_volume_is_idempotent_on_an_already_existing_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.create_volume("zroot/keel/volumes/web-data", "1G").unwrap();
        zfs.create_volume("zroot/keel/volumes/web-data", "1G").unwrap();
        assert_eq!(zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), true);
    }

    #[test]
    fn destroy_dataset_on_a_busy_dataset_returns_busy_and_leaves_it_present() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.mark_busy("zroot/keel/volumes/web-data");
        assert!(matches!(zfs.destroy_dataset("zroot/keel/volumes/web-data"), Err(ZfsError::Busy(_))));
        assert_eq!(zfs.dataset_exists("zroot/keel/volumes/web-data").unwrap(), true);
    }

    #[test]
    fn snapshot_requires_an_existing_dataset() {
        let zfs = FakeZfsManager::new();
        assert!(matches!(zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-1"), Err(ZfsError::NotFound(_))));
    }

    #[test]
    fn send_snapshot_requires_the_snapshot_to_exist() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        let mut out = Vec::new();
        assert!(matches!(
            zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-1", None, &mut out),
            Err(ZfsError::NotFound(_))
        ));
    }

    #[test]
    fn send_snapshot_full_then_receive_snapshot_creates_the_target_dataset() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-1").unwrap();

        let mut stream = Vec::new();
        zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-1", None, &mut stream).unwrap();

        let target = FakeZfsManager::new();
        target.receive_snapshot("zroot/keel/volumes/web-0-data", &mut stream.as_slice()).unwrap();
        assert!(target.dataset_exists("zroot/keel/volumes/web-0-data").unwrap());
    }

    #[test]
    fn send_snapshot_incremental_requires_the_base_snapshot_to_exist() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-2").unwrap();

        let mut out = Vec::new();
        assert!(matches!(
            zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-2", Some("keel-repl-1"), &mut out),
            Err(ZfsError::NotFound(_))
        ));
    }

    #[test]
    fn send_snapshot_incremental_succeeds_once_the_base_exists() {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/volumes/web-data");
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-1").unwrap();
        zfs.snapshot("zroot/keel/volumes/web-data", "keel-repl-2").unwrap();

        let mut out = Vec::new();
        zfs.send_snapshot("zroot/keel/volumes/web-data", "keel-repl-2", Some("keel-repl-1"), &mut out).unwrap();
        assert!(!out.is_empty(), "expected the fake to still write a synthetic byte marker for an incremental send");
    }

    #[test]
    fn receive_snapshot_on_a_malformed_stream_fails_without_creating_the_dataset() {
        let zfs = FakeZfsManager::new();
        let mut garbage: &[u8] = b"not a real send stream";
        assert!(matches!(zfs.receive_snapshot("zroot/keel/volumes/web-0-data", &mut garbage), Err(ZfsError::CommandFailed(_, _, _))));
        assert!(!zfs.dataset_exists("zroot/keel/volumes/web-0-data").unwrap());
    }

    #[test]
    fn clone_shares_the_same_underlying_state() {
        let zfs = FakeZfsManager::new();
        let clone = zfs.clone();
        clone.seed_dataset("zroot/keel/volumes/shared");
        assert!(zfs.dataset_exists("zroot/keel/volumes/shared").unwrap(), "expected a clone's mutation to be visible through the original handle");
    }
}
