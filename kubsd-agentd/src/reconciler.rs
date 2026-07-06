use crate::backoff::BackoffState;
use crate::record::{self, JailRecord};
use crate::store::{self, StoreError};
use kubsd_jail::JailRuntime;
use kubsd_net::NetManager;
use kubsd_spec::JailSpec;
use kubsd_zfs::ZfsManager;
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("spec validation failed: {0}")]
    InvalidSpec(#[from] kubsd_spec::SpecError),
    #[error("state store error: {0}")]
    Store(#[from] StoreError),
    #[error("jail runtime error: {0}")]
    Jail(#[from] kubsd_jail::JailError),
    #[error("zfs error: {0}")]
    Zfs(#[from] kubsd_zfs::ZfsError),
    #[error("network error: {0}")]
    Net(#[from] kubsd_net::NetError),
    #[error("jail '{0}' not found in desired state")]
    NotFound(String),
    #[error("base image dataset '{0}' does not exist")]
    BaseImageNotFound(String),
}

pub struct Reconciler<J: JailRuntime, Z: ZfsManager, N: NetManager> {
    jails: J,
    zfs: Z,
    net: N,
    pool: String,
    state_dir: PathBuf,
    records: HashMap<String, JailRecord>,
    backoff: HashMap<String, BackoffState>,
    next_epair_ordinal: u32,
}

impl<J: JailRuntime, Z: ZfsManager, N: NetManager> Reconciler<J, Z, N> {
    pub fn new(jails: J, zfs: Z, net: N, pool: String, state_dir: PathBuf) -> Result<Self, ReconcileError> {
        let loaded = store::load_all(&state_dir)?;
        let next_epair_ordinal = loaded.iter().map(|r| r.epair_ordinal).max().map(|m| m + 1).unwrap_or(1);
        let records = loaded.into_iter().map(|r| (r.spec.metadata.name.clone(), r)).collect();
        Ok(Self {
            jails,
            zfs,
            net,
            pool,
            state_dir,
            records,
            backoff: HashMap::new(),
            next_epair_ordinal,
        })
    }

    pub fn apply(&mut self, spec: JailSpec) -> Result<(), ReconcileError> {
        kubsd_spec::validate_name(&spec.metadata.name)?;
        kubsd_spec::validate_address(&spec.spec.network.address)?;
        kubsd_spec::parse_cpu_cores(&spec.spec.resources.cpu)?;
        kubsd_spec::parse_memory_bytes(&spec.spec.resources.memory)?;

        let epair_ordinal = if let Some(existing) = self.records.get(&spec.metadata.name) {
            kubsd_spec::validate_transition(&existing.spec, &spec)?;
            existing.epair_ordinal
        } else {
            let ordinal = self.next_epair_ordinal;
            self.next_epair_ordinal += 1;
            ordinal
        };

        let record = JailRecord { spec: spec.clone(), epair_ordinal };
        store::save(&self.state_dir, &record)?;
        self.records.insert(spec.metadata.name.clone(), record);
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), ReconcileError> {
        let record = self.records.get(name).ok_or_else(|| ReconcileError::NotFound(name.to_string()))?.clone();
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);

        self.net.detach_jail(&epair_base)?;
        // A record can reach `delete` having only gone through `apply`
        // (never `provision`, e.g. the daemon restarted before its first
        // reconcile pass): `destroy`, `destroy_dataset`, and
        // `remove_resource_limits` are not intrinsically idempotent, so
        // treat their `NotFound` as "already torn down" here.
        match self.jails.destroy(&jail_name) {
            Ok(()) | Err(kubsd_jail::JailError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        match self.zfs.destroy_dataset(&jail_dataset) {
            Ok(()) | Err(kubsd_zfs::ZfsError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        match self.jails.remove_resource_limits(&jail_name) {
            Ok(()) | Err(kubsd_jail::JailError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }

        store::remove(&self.state_dir, name)?;
        self.records.remove(name);
        self.backoff.remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubsd_jail::FakeJailRuntime;
    use kubsd_net::FakeNetManager;
    use kubsd_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
    use kubsd_zfs::FakeZfsManager;
    use std::fs;

    fn sample_spec(name: &str) -> JailSpec {
        JailSpec {
            api_version: "kubsd/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec {
                    vnet: true,
                    bridge: "kubsd0".to_string(),
                    address: "10.0.0.5/24".to_string(),
                },
                resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                restart_policy: RestartPolicy::Always,
            },
        }
    }

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kubsd-agentd-reconciler-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn new_reconciler(state_dir: PathBuf) -> Reconciler<FakeJailRuntime, FakeZfsManager, FakeNetManager> {
        Reconciler::new(
            FakeJailRuntime::new(),
            FakeZfsManager::new(),
            FakeNetManager::new(),
            "zroot".to_string(),
            state_dir,
        )
        .unwrap()
    }

    #[test]
    fn new_starts_with_no_records_on_an_empty_state_dir() {
        let dir = test_state_dir("new_starts_with_no_records_on_an_empty_state_dir");
        let reconciler = new_reconciler(dir);
        assert!(reconciler.records.is_empty());
        assert_eq!(reconciler.next_epair_ordinal, 1);
    }

    #[test]
    fn apply_persists_and_tracks_the_record() {
        let dir = test_state_dir("apply_persists_and_tracks_the_record");
        let mut reconciler = new_reconciler(dir.clone());
        reconciler.apply(sample_spec("web-1")).unwrap();

        assert_eq!(reconciler.records.len(), 1);
        assert_eq!(reconciler.records["web-1"].epair_ordinal, 1);

        let loaded = store::load_all(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].spec.metadata.name, "web-1");
    }

    #[test]
    fn apply_assigns_increasing_ordinals_to_new_jails() {
        let dir = test_state_dir("apply_assigns_increasing_ordinals_to_new_jails");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();
        reconciler.apply(sample_spec("web-2")).unwrap();
        assert_eq!(reconciler.records["web-1"].epair_ordinal, 1);
        assert_eq!(reconciler.records["web-2"].epair_ordinal, 2);
    }

    #[test]
    fn new_recovers_next_ordinal_from_disk() {
        let dir = test_state_dir("new_recovers_next_ordinal_from_disk");
        {
            let mut reconciler = new_reconciler(dir.clone());
            reconciler.apply(sample_spec("web-1")).unwrap();
            reconciler.apply(sample_spec("web-2")).unwrap();
        }
        // A fresh Reconciler over the same state_dir should pick up where the last one left off.
        let reconciler = new_reconciler(dir);
        assert_eq!(reconciler.next_epair_ordinal, 3);
    }

    #[test]
    fn apply_keeps_the_same_ordinal_on_reapply() {
        let dir = test_state_dir("apply_keeps_the_same_ordinal_on_reapply");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();
        let first_ordinal = reconciler.records["web-1"].epair_ordinal;

        let mut updated = sample_spec("web-1");
        updated.spec.resources.cpu = "4".to_string(); // mutable field, allowed
        reconciler.apply(updated).unwrap();

        assert_eq!(reconciler.records["web-1"].epair_ordinal, first_ordinal);
    }

    #[test]
    fn apply_rejects_immutable_field_change() {
        let dir = test_state_dir("apply_rejects_immutable_field_change");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();

        let mut changed = sample_spec("web-1");
        changed.spec.image = "base/different-image".to_string();
        let result = reconciler.apply(changed);
        assert!(matches!(result, Err(ReconcileError::InvalidSpec(_))));
    }

    #[test]
    fn apply_rejects_invalid_name() {
        let dir = test_state_dir("apply_rejects_invalid_name");
        let mut reconciler = new_reconciler(dir);
        let mut invalid = sample_spec("Invalid_Name");
        invalid.metadata.name = "Invalid_Name".to_string();
        let result = reconciler.apply(invalid);
        assert!(matches!(result, Err(ReconcileError::InvalidSpec(_))));
    }

    #[test]
    fn delete_on_unknown_name_returns_not_found() {
        let dir = test_state_dir("delete_on_unknown_name_returns_not_found");
        let mut reconciler = new_reconciler(dir);
        assert!(matches!(reconciler.delete("missing"), Err(ReconcileError::NotFound(_))));
    }

    #[test]
    fn delete_removes_the_record_from_memory_and_disk() {
        let dir = test_state_dir("delete_removes_the_record_from_memory_and_disk");
        let mut reconciler = new_reconciler(dir.clone());
        reconciler.apply(sample_spec("web-1")).unwrap();

        reconciler.delete("web-1").unwrap();

        assert!(!reconciler.records.contains_key("web-1"));
        assert_eq!(store::load_all(&dir).unwrap(), vec![]);
    }
}
