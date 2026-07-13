#![cfg(target_os = "freebsd")]

use keel_zfs::{CliZfsManager, ZfsManager};

// Run as root on the FreeBSD VM: `sudo cargo test -p keel-zfs --test freebsd_zfs`
// Requires zroot/keel/base/test to already exist (created in Milestone 2 Task 1).

#[test]
fn dataset_exists_reports_true_for_the_test_base_and_false_for_garbage() {
    let zfs = CliZfsManager::new();
    assert_eq!(zfs.dataset_exists("zroot/keel/base/test").unwrap(), true);
    assert_eq!(zfs.dataset_exists("zroot/keel/does-not-exist").unwrap(), false);
}

#[test]
fn destroy_dataset_removes_a_dataset_created_for_the_test() {
    let zfs = CliZfsManager::new();
    let scratch = "zroot/keel/jails/destroy-test-scratch";
    let _ = std::process::Command::new("zfs").args(["destroy", scratch]).output();
    std::process::Command::new("zfs")
        .args(["create", scratch])
        .output()
        .expect("zfs create should run");

    assert_eq!(zfs.dataset_exists(scratch).unwrap(), true);
    zfs.destroy_dataset(scratch).expect("destroy_dataset should succeed");
    assert_eq!(zfs.dataset_exists(scratch).unwrap(), false);
}

#[test]
fn destroy_dataset_on_a_never_created_dataset_returns_not_found() {
    // Reproduces a real bug found while verifying Milestone 8's keel-jail
    // NotFound fix end-to-end: `destroy_dataset` always mapped a failing
    // `zfs destroy` to `ZfsError::CommandFailed`, never `ZfsError::
    // NotFound`, so `Reconciler::delete`'s documented tolerance for a
    // record whose provisioning failed before this dataset was ever
    // cloned never actually engaged against the real ZFS manager, only
    // against `FakeZfsManager`'s test double.
    let zfs = CliZfsManager::new();
    let dataset = "zroot/keel/jails/destroy-never-created-scratch";
    let _ = zfs.destroy_dataset(dataset);

    match zfs.destroy_dataset(dataset) {
        Err(keel_zfs::ZfsError::NotFound(d)) => assert_eq!(d, dataset),
        other => panic!("expected NotFound for a dataset that was never created, got: {other:?}"),
    }
}

#[test]
fn clone_from_base_creates_a_usable_clone() {
    let zfs = CliZfsManager::new();
    let target = "zroot/keel/jails/clone-test-scratch";
    let _ = zfs.destroy_dataset(target);

    zfs.clone_from_base("zroot/keel/base/test", target).expect("clone_from_base should succeed");
    assert_eq!(zfs.dataset_exists(target).unwrap(), true);

    zfs.destroy_dataset(target).expect("cleanup destroy should succeed");
}

#[test]
fn clone_from_base_reuses_existing_snapshot_on_second_call() {
    let zfs = CliZfsManager::new();
    let target_a = "zroot/keel/jails/clone-test-scratch-a";
    let target_b = "zroot/keel/jails/clone-test-scratch-b";
    let _ = zfs.destroy_dataset(target_a);
    let _ = zfs.destroy_dataset(target_b);

    zfs.clone_from_base("zroot/keel/base/test", target_a).expect("first clone should succeed");
    zfs.clone_from_base("zroot/keel/base/test", target_b).expect("second clone should succeed and reuse the snapshot");

    zfs.destroy_dataset(target_a).expect("cleanup a should succeed");
    zfs.destroy_dataset(target_b).expect("cleanup b should succeed");
}
