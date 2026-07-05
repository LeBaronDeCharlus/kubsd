#![cfg(target_os = "freebsd")]

use kubsd_jail::{JailRuntime, ProcessJailRuntime};
use std::path::Path;

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-jail --test freebsd_lifecycle`
// (jail(8)/jls(8) require root privileges).

#[test]
fn create_destroy_and_is_running_lifecycle() {
    let runtime = ProcessJailRuntime::new();
    let name = "kubsd-test-lifecycle";
    let rootfs = Path::new("/tmp/kubsd-test-lifecycle-rootfs");
    std::fs::create_dir_all(rootfs).unwrap();

    // Clean up any leftover jail from a previous failed run.
    let _ = runtime.destroy(name);

    runtime.create(name, rootfs).expect("create should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "no command started yet");

    runtime.destroy(name).expect("destroy should succeed");
    assert_eq!(runtime.is_running(name).unwrap(), false, "destroyed jail is not running");
}
