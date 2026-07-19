#![cfg(target_os = "freebsd")]

use keel_jail::{CliMountManager, MountManager};
use std::path::Path;

// Run as root on the FreeBSD VM: `sudo cargo test -p keel-jail --test freebsd_mount`
// (mount(8)/umount(8) require root privileges).
//
// Requires a `zroot/keel/volumes` dataset to exist (same one-time bootstrap
// keel-zfs's freebsd_zfs test documents), used here purely as a real
// directory to nullfs-mount from — this test never calls into keel-zfs
// itself, it only needs a real source path that already exists.

#[test]
fn ensure_mount_point_creates_missing_parent_directories() {
    let mounts = CliMountManager::new();
    let target = Path::new("/tmp/keel-mount-test-ensure-mount-point/nested/data");
    let _ = std::fs::remove_dir_all("/tmp/keel-mount-test-ensure-mount-point");

    mounts.ensure_mount_point(target).expect("ensure_mount_point should succeed");
    assert!(target.is_dir());
}

#[test]
fn mount_nullfs_then_is_mounted_then_unmount_round_trips_through_the_real_kernel() {
    let mounts = CliMountManager::new();
    let source = Path::new("/zroot/keel/volumes");
    let target = Path::new("/tmp/keel-mount-test-round-trip");
    let _ = std::process::Command::new("umount").arg(target).output();
    std::fs::create_dir_all(target).unwrap();

    assert_eq!(mounts.is_mounted(target).unwrap(), false, "must not be mounted before mount_nullfs");

    mounts.mount_nullfs(source, target).expect("mount_nullfs should succeed");
    assert_eq!(mounts.is_mounted(target).unwrap(), true);

    mounts.unmount(target).expect("unmount should succeed");
    assert_eq!(mounts.is_mounted(target).unwrap(), false);
}

#[test]
fn unmount_on_a_never_mounted_target_returns_not_mounted() {
    let mounts = CliMountManager::new();
    let target = Path::new("/tmp/keel-mount-test-never-mounted");
    std::fs::create_dir_all(target).unwrap();

    match mounts.unmount(target) {
        Err(keel_jail::MountError::NotMounted(p)) => assert_eq!(p, target),
        other => panic!("expected NotMounted for a target that was never mounted, got: {other:?}"),
    }
}
