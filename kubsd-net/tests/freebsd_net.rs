#![cfg(target_os = "freebsd")]

use kubsd_net::{NetManager, ProcessNetManager};
use kubsd_jail::{JailRuntime, ProcessJailRuntime};
use std::path::Path;
use std::process::Command;

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-net --test freebsd_net`

fn destroy_interface_if_exists(name: &str) {
    let _ = Command::new("ifconfig").args([name, "destroy"]).output();
}

fn make_test_jail(name: &str) -> ProcessJailRuntime {
    let jails = ProcessJailRuntime::new();
    let _ = jails.destroy(name);
    let rootfs = Path::new("/tmp").join(format!("{name}-rootfs"));
    let bin_dir = rootfs.join("sbin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::copy("/rescue/ifconfig", bin_dir.join("ifconfig")).expect("copy /rescue/ifconfig into test rootfs");
    jails.create(name, &rootfs, true).expect("create should succeed"); // vnet: true
    jails
}

#[test]
fn ensure_bridge_exists_creates_and_is_idempotent() {
    let net = ProcessNetManager::new();
    let bridge = "kubsd-test-br0";
    destroy_interface_if_exists(bridge);

    net.ensure_bridge_exists(bridge).expect("first call should create the bridge");
    let check = Command::new("ifconfig").arg(bridge).output().expect("ifconfig should run");
    assert!(check.status.success(), "bridge should exist after ensure_bridge_exists");

    net.ensure_bridge_exists(bridge).expect("second call should be a no-op success");

    destroy_interface_if_exists(bridge);
}

#[test]
fn attach_jail_wires_up_epair_and_configures_address() {
    let net = ProcessNetManager::new();
    let bridge = "kubsd-test-br1";
    let jail_name = "kubsd-net-test-attach";
    let epair_base = "epair50";

    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
    let jails = make_test_jail(jail_name);

    net.ensure_bridge_exists(bridge).expect("bridge should be created");
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.5/24")
        .expect("attach_jail should succeed");

    let inside = Command::new("jexec")
        .args([jail_name, "/sbin/ifconfig", &format!("{epair_base}b")])
        .output()
        .expect("jexec ifconfig should run");
    let inside_output = String::from_utf8_lossy(&inside.stdout);
    assert!(inside_output.contains("10.99.0.5"), "expected configured address in: {inside_output}");

    jails.destroy(jail_name).expect("cleanup destroy should succeed");
    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
}

#[test]
fn attach_jail_tolerates_retry_after_epair_already_created() {
    let net = ProcessNetManager::new();
    let bridge = "kubsd-test-br2";
    let jail_name = "kubsd-net-test-retry";
    let epair_base = "epair51";

    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
    let jails = make_test_jail(jail_name);

    net.ensure_bridge_exists(bridge).expect("bridge should be created");
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.6/24")
        .expect("first attach_jail should succeed");

    // Simulate a retry after an interrupted prior attempt: calling
    // attach_jail again for the same epair_base (now fully wired into the
    // jail) must not error.
    net.attach_jail(jail_name, bridge, epair_base, "10.99.0.6/24")
        .expect("retried attach_jail should tolerate already-attached state");

    jails.destroy(jail_name).expect("cleanup destroy should succeed");
    destroy_interface_if_exists(&format!("{epair_base}a"));
    destroy_interface_if_exists(bridge);
}
