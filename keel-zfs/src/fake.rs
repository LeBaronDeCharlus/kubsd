use crate::ZfsError;
use crate::ZfsManager;
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeZfsManager {
    datasets: Mutex<HashSet<String>>,
    busy: Mutex<HashSet<String>>,
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
}
