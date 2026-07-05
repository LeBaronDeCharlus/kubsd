#![cfg(target_os = "freebsd")]

use kubsd_net::{NetManager, ProcessNetManager};
use std::process::Command;

// Run as root on the FreeBSD VM: `sudo cargo test -p kubsd-net --test freebsd_net`

fn destroy_interface_if_exists(name: &str) {
    let _ = Command::new("ifconfig").args([name, "destroy"]).output();
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
