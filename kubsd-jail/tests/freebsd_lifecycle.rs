#![cfg(target_os = "freebsd")]

use kubsd_jail::{JailRuntime, ProcessJailRuntime};
use std::path::Path;
use std::{thread, time::Duration};

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-jail --test freebsd_lifecycle`
// (jail(8)/jls(8)/jexec(8) require root privileges).

#[test]
fn create_destroy_and_is_running_lifecycle() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-lifecycle";
    let rootfs = Path::new("/tmp/kubsd-test-lifecycle-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    let _ = runtime.destroy(name);

    runtime.create(name, rootfs).expect("create should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "no command started yet");

    runtime.destroy(name).expect("destroy should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "destroyed jail is not running");
}

#[test]
fn start_command_makes_is_running_true() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-start-command";
    let rootfs = Path::new("/tmp/kubsd-test-start-command-rootfs");
    let bin_dir = rootfs.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::copy("/rescue/sh", bin_dir.join("sh")).expect("copy /rescue/sh into test rootfs");
    std::fs::copy("/rescue/sleep", bin_dir.join("sleep")).expect("copy /rescue/sleep into test rootfs");

    let _ = runtime.destroy(name);
    runtime.create(name, rootfs).expect("create should succeed");

    runtime
        .start_command(name, &["/bin/sh".to_string(), "-c".to_string(), "sleep 30".to_string()])
        .expect("start_command should succeed");

    // Give jexec a moment to actually fork/exec before checking.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(runtime.is_running(name).unwrap(), true, "sleep 30 should still be running");

    runtime.destroy(name).expect("destroy should succeed");
}

#[test]
fn set_and_remove_resource_limits() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-resource-limits";
    let rootfs = Path::new("/tmp/kubsd-test-resource-limits-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    let _ = runtime.remove_resource_limits(name);
    let _ = runtime.destroy(name);
    runtime.create(name, rootfs).expect("create should succeed");

    runtime.set_resource_limits(name, 200, 512 * 1024 * 1024).expect("set_resource_limits should succeed");

    let output = std::process::Command::new("rctl")
        .arg(format!("jail:{name}"))
        .output()
        .expect("rctl should run");
    let rules = String::from_utf8_lossy(&output.stdout);
    assert!(rules.contains("pcpu:deny=200"), "expected pcpu rule in: {rules}");
    assert!(rules.contains("vmemoryuse:deny=536870912"), "expected vmemoryuse rule in: {rules}");

    runtime.remove_resource_limits(name).expect("remove_resource_limits should succeed");
    let output = std::process::Command::new("rctl")
        .arg(format!("jail:{name}"))
        .output()
        .expect("rctl should run");
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty(), "rules should be gone after removal");

    runtime.destroy(name).expect("destroy should succeed");
}

#[test]
fn remove_resource_limits_on_jail_with_no_limits_set_is_a_no_op_success() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-no-limits-set";
    let rootfs = Path::new("/tmp/kubsd-test-no-limits-set-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    let _ = runtime.destroy(name);
    runtime.create(name, rootfs).expect("create should succeed");

    runtime.remove_resource_limits(name).expect("removing limits that were never set should succeed, not error");

    runtime.destroy(name).expect("destroy should succeed");
}
