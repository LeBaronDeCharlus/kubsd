#![cfg(target_os = "freebsd")]

use kubsd_zfs::{CliZfsManager, ZfsManager};

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-zfs --test freebsd_zfs`
// Requires zroot/kubsd/base/test to already exist (created in Milestone 2 Task 1).

#[test]
fn dataset_exists_reports_true_for_the_test_base_and_false_for_garbage() {
    let zfs = CliZfsManager::new();
    assert_eq!(zfs.dataset_exists("zroot/kubsd/base/test").unwrap(), true);
    assert_eq!(zfs.dataset_exists("zroot/kubsd/does-not-exist").unwrap(), false);
}

#[test]
fn destroy_dataset_removes_a_dataset_created_for_the_test() {
    let zfs = CliZfsManager::new();
    let scratch = "zroot/kubsd/jails/destroy-test-scratch";
    let _ = std::process::Command::new("zfs").args(["destroy", scratch]).output();
    std::process::Command::new("zfs")
        .args(["create", scratch])
        .output()
        .expect("zfs create should run");

    assert_eq!(zfs.dataset_exists(scratch).unwrap(), true);
    zfs.destroy_dataset(scratch).expect("destroy_dataset should succeed");
    assert_eq!(zfs.dataset_exists(scratch).unwrap(), false);
}
