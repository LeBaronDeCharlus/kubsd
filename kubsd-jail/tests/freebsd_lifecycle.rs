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
