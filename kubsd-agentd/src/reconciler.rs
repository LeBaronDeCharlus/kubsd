use crate::backoff::BackoffState;
use crate::record::{self, JailRecord};
use crate::store::{self, StoreError};
use kubsd_jail::JailRuntime;
use kubsd_net::NetManager;
use kubsd_spec::JailSpec;
use kubsd_zfs::ZfsManager;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
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

    fn configure_networking_and_limits(&mut self, name: &str, record: &JailRecord) -> Result<(), ReconcileError> {
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        self.net.ensure_bridge_exists(&record.spec.spec.network.bridge)?;
        self.net.attach_jail(
            &jail_name,
            &record.spec.spec.network.bridge,
            &epair_base,
            &record.spec.spec.network.address,
        )?;
        let pcpu_percent = kubsd_spec::cores_to_pcpu_percent(kubsd_spec::parse_cpu_cores(&record.spec.spec.resources.cpu)?);
        let memory_bytes = kubsd_spec::parse_memory_bytes(&record.spec.spec.resources.memory)?;
        self.jails.set_resource_limits(&jail_name, pcpu_percent, memory_bytes)?;
        Ok(())
    }

    fn provision(&mut self, name: &str, record: &JailRecord) -> Result<(), ReconcileError> {
        let jail_name = record::jail_name(name);
        let base_dataset = record::base_dataset_path(&self.pool, &record.spec.spec.image);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);
        let rootfs = record::jail_rootfs_path(&self.pool, name);

        if !self.zfs.dataset_exists(&base_dataset)? {
            return Err(ReconcileError::BaseImageNotFound(base_dataset));
        }
        self.zfs.clone_from_base(&base_dataset, &jail_dataset)?;
        self.jails.create(&jail_name, &rootfs, record.spec.spec.network.vnet)?;
        self.configure_networking_and_limits(name, record)?;
        self.jails.start_command(&jail_name, &record.spec.spec.command)?;
        Ok(())
    }

    fn restart(&mut self, name: &str, record: &JailRecord) -> Result<(), ReconcileError> {
        let jail_name = record::jail_name(name);
        self.configure_networking_and_limits(name, record)?;
        self.jails.start_command(&jail_name, &record.spec.spec.command)?;
        Ok(())
    }

    /// Best-effort cleanup after a failed `provision`. Every call here
    /// discards its `Result` (`let _ =`): `detach_jail` is intrinsically
    /// idempotent, and `destroy`/`destroy_dataset`/`remove_resource_limits`
    /// return `NotFound` for a step that never completed — discarding the
    /// result either way makes it safe to call all four unconditionally
    /// even when only some steps of `provision` actually ran. A rollback
    /// failure for a reason other than "already gone" is handled by the
    /// normal per-jail backoff on the next reconciliation pass, not
    /// specially here.
    fn rollback_provision(&mut self, name: &str, record: &JailRecord) {
        let jail_name = record::jail_name(name);
        let epair_base = record::epair_base_name(record.epair_ordinal);
        let jail_dataset = record::jail_dataset_path(&self.pool, name);
        let _ = self.net.detach_jail(&epair_base);
        let _ = self.jails.destroy(&jail_name);
        let _ = self.zfs.destroy_dataset(&jail_dataset);
        let _ = self.jails.remove_resource_limits(&jail_name);
    }

    pub fn reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)> {
        let names: Vec<String> = self.records.keys().cloned().collect();
        let mut failures = Vec::new();
        for name in names {
            if let Err(e) = self.reconcile_one(&name, now) {
                failures.push((name, e));
            }
        }
        failures
    }

    fn reconcile_one(&mut self, name: &str, now: Instant) -> Result<(), ReconcileError> {
        let record = self.records[name].clone();
        let jail_name = record::jail_name(name);

        let can_retry = self.backoff.entry(name.to_string()).or_insert_with(BackoffState::new).can_retry(now);
        if !can_retry {
            return Ok(());
        }

        let exists = self.jails.jail_exists(&jail_name)?;

        if !exists {
            let result = self.provision(name, &record);
            self.backoff.get_mut(name).unwrap().record_attempt(now);
            if result.is_err() {
                self.rollback_provision(name, &record);
            }
            result
        } else {
            let running = self.jails.is_running(&jail_name)?;
            if running {
                let pcpu_percent =
                    kubsd_spec::cores_to_pcpu_percent(kubsd_spec::parse_cpu_cores(&record.spec.spec.resources.cpu)?);
                let memory_bytes = kubsd_spec::parse_memory_bytes(&record.spec.spec.resources.memory)?;
                self.jails.set_resource_limits(&jail_name, pcpu_percent, memory_bytes)?;
                Ok(())
            } else if record.spec.spec.restart_policy == kubsd_spec::RestartPolicy::Never {
                Ok(())
            } else {
                let result = self.restart(name, &record);
                self.backoff.get_mut(name).unwrap().record_attempt(now);
                result
            }
        }
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
    use std::time::Duration;

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

    #[test]
    fn provision_drives_zfs_jail_net_and_command_in_order() {
        let dir = test_state_dir("provision_drives_zfs_jail_net_and_command_in_order");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let record = reconciler.records["web-1"].clone();

        reconciler.provision("web-1", &record).unwrap();

        let jail_name = record::jail_name("web-1");
        assert_eq!(reconciler.jails.jail_exists(&jail_name).unwrap(), true);
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
        assert_eq!(
            reconciler.zfs.dataset_exists(&record::jail_dataset_path("zroot", "web-1")).unwrap(),
            true
        );
    }

    #[test]
    fn provision_fails_clearly_when_base_image_missing() {
        let dir = test_state_dir("provision_fails_clearly_when_base_image_missing");
        let mut reconciler = new_reconciler(dir);
        reconciler.apply(sample_spec("web-1")).unwrap();
        let record = reconciler.records["web-1"].clone();

        let result = reconciler.provision("web-1", &record);
        assert!(matches!(result, Err(ReconcileError::BaseImageNotFound(_))));
    }

    #[test]
    fn rollback_provision_cleans_up_after_partial_failure() {
        let dir = test_state_dir("rollback_provision_cleans_up_after_partial_failure");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let record = reconciler.records["web-1"].clone();

        // Simulate a partial failure: dataset clone + jail create succeed,
        // then provisioning would fail at the networking step in the real
        // system. We can't easily force FakeNetManager to fail without a
        // missing bridge, so instead verify rollback's own idempotent
        // cleanup behavior directly: calling it after a successful
        // provision should fully undo it.
        reconciler.provision("web-1", &record).unwrap();
        reconciler.rollback_provision("web-1", &record);

        let jail_name = record::jail_name("web-1");
        assert_eq!(reconciler.jails.jail_exists(&jail_name).unwrap(), false);
        assert_eq!(
            reconciler.zfs.dataset_exists(&record::jail_dataset_path("zroot", "web-1")).unwrap(),
            false
        );
    }

    #[test]
    fn reconcile_provisions_a_missing_jail() {
        let dir = test_state_dir("reconcile_provisions_a_missing_jail");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();

        let failures = reconciler.reconcile(Instant::now());

        assert!(failures.is_empty(), "expected no failures, got: {failures:?}");
        let jail_name = record::jail_name("web-1");
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_reports_base_image_not_found_without_stopping_other_jails() {
        let dir = test_state_dir("reconcile_reports_base_image_not_found_without_stopping_other_jails");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap(); // has a seeded base image
        let mut broken = sample_spec("web-2");
        broken.spec.image = "base/does-not-exist".to_string();
        reconciler.apply(broken).unwrap(); // base image never seeded

        let failures = reconciler.reconcile(Instant::now());

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "web-2");
        assert!(matches!(failures[0].1, ReconcileError::BaseImageNotFound(_)));
        // web-1 should still have been provisioned successfully.
        assert_eq!(reconciler.jails.is_running(&record::jail_name("web-1")).unwrap(), true);
    }

    #[test]
    fn reconcile_restarts_a_crashed_jail() {
        let dir = test_state_dir("reconcile_restarts_a_crashed_jail");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let t0 = Instant::now();
        reconciler.reconcile(t0);

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);

        // The initial provisioning call above already armed a 1s backoff
        // cooldown (record_attempt runs unconditionally after provisioning,
        // per BackoffState's contract) — advance past it so this restart
        // attempt isn't itself suppressed by that cooldown.
        reconciler.reconcile(t0 + Duration::from_secs(1));
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_respects_backoff_cooldown_between_restarts() {
        let dir = test_state_dir("reconcile_respects_backoff_cooldown_between_restarts");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let t0 = Instant::now();
        reconciler.reconcile(t0);

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        // Still within the 1s cooldown armed by the initial provisioning
        // call above — this reconcile is a no-op, same as the next one.
        reconciler.reconcile(t0);

        reconciler.jails.mark_exited(&jail_name);
        reconciler.reconcile(t0); // still within cooldown — should NOT restart yet
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);

        reconciler.reconcile(t0 + Duration::from_secs(1)); // cooldown passed
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_never_policy_leaves_a_crashed_jail_alone() {
        let dir = test_state_dir("reconcile_never_policy_leaves_a_crashed_jail_alone");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        let mut spec = sample_spec("web-1");
        spec.spec.restart_policy = RestartPolicy::Never;
        reconciler.apply(spec).unwrap();
        reconciler.reconcile(Instant::now());

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        reconciler.reconcile(Instant::now());

        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);
    }

    #[test]
    fn reconcile_arms_backoff_even_when_restart_attempt_fails() {
        let dir = test_state_dir("reconcile_arms_backoff_even_when_restart_attempt_fails");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        let t0 = Instant::now();
        reconciler.reconcile(t0); // provisions web-1

        let jail_name = record::jail_name("web-1");
        reconciler.jails.mark_exited(&jail_name);
        reconciler.jails.fail_start_command(&jail_name, true);

        // Past the 1s cooldown armed by the initial provisioning.
        let failures = reconciler.reconcile(t0 + Duration::from_secs(1));
        assert_eq!(failures.len(), 1, "expected the failed restart attempt to be reported: {failures:?}");

        // If backoff was correctly armed by the failed restart attempt,
        // an immediate retry at the same instant must be suppressed —
        // proven by the fault (still armed) NOT firing again.
        let retried = reconciler.reconcile(t0 + Duration::from_secs(1));
        assert!(retried.is_empty(), "expected the retry to be suppressed by backoff, got: {retried:?}");
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), false);

        // Once the cooldown clears and the fault is removed, it should recover.
        reconciler.jails.fail_start_command(&jail_name, false);
        let recovered = reconciler.reconcile(t0 + Duration::from_secs(10));
        assert!(recovered.is_empty(), "expected recovery, got: {recovered:?}");
        assert_eq!(reconciler.jails.is_running(&jail_name).unwrap(), true);
    }

    #[test]
    fn reconcile_is_a_no_op_when_jail_already_matches_desired_state() {
        let dir = test_state_dir("reconcile_is_a_no_op_when_jail_already_matches_desired_state");
        let mut reconciler = new_reconciler(dir);
        reconciler.zfs.seed_dataset("zroot/kubsd/base/14.2-web");
        reconciler.apply(sample_spec("web-1")).unwrap();
        reconciler.reconcile(Instant::now());

        let failures = reconciler.reconcile(Instant::now());
        assert!(failures.is_empty(), "expected no failures, got: {failures:?}");
        assert_eq!(reconciler.jails.is_running(&record::jail_name("web-1")).unwrap(), true);
    }
}
