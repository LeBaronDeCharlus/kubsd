use keel_agentd::{worker, Reconciler};
use keel_jail::FakeJailRuntime;
use keel_net::FakeNetManager;
use keel_zfs::FakeZfsManager;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

fn start_test_server(name: &str) -> PathBuf {
    let state_dir = std::env::temp_dir().join(format!("keelctl-test-state-{name}"));
    let _ = std::fs::remove_dir_all(&state_dir);
    let zfs = FakeZfsManager::new();
    zfs.seed_dataset("zroot/keel/base/14.2-web");
    let reconciler =
        Reconciler::new(FakeJailRuntime::new(), zfs, FakeNetManager::new(), "zroot".to_string(), state_dir)
            .unwrap();
    let (_worker_handle, commands) = worker::spawn(reconciler);

    // A short, non-descriptive filename (not the full test name) — macOS/BSD
    // cap Unix socket paths at ~104 bytes (SUN_LEN), and the default macOS
    // TMPDIR (/var/folders/.../T/) already uses ~50 of them.
    let socket_path = short_unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    thread::spawn(move || keel_agentd::http::run(listener, commands));
    socket_path
}

fn short_unique_socket_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ka-{}-{}.sock", std::process::id(), id))
}

fn write_spec_file(test_name: &str, jail_name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("keelctl-test-spec-{test_name}.yaml"));
    let yaml = format!(
        "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: {jail_name}\nspec:\n  image: base/14.2-web\n  command: [\"/usr/local/bin/myapp\"]\n  network:\n    vnet: true\n    bridge: keel0\n    address: 10.0.0.5/24\n  resources:\n    cpu: \"2\"\n    memory: 512M\n  restartPolicy: Always\n"
    );
    std::fs::write(&path, yaml).unwrap();
    path
}

fn run_keelctl(socket_path: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_keelctl"))
        .args(args)
        .arg("--socket")
        .arg(socket_path)
        .output()
        .expect("failed to run keelctl binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn apply_get_delete_round_trip() {
    let socket_path = start_test_server("apply_get_delete_round_trip");
    let spec_path = write_spec_file("apply_get_delete_round_trip", "web-1");

    let (ok, _, stderr) = run_keelctl(&socket_path, &["apply", "-f", spec_path.to_str().unwrap()]);
    assert!(ok, "apply failed: {stderr}");

    let (ok, stdout, stderr) = run_keelctl(&socket_path, &["get", "web-1"]);
    assert!(ok, "get failed: {stderr}");
    assert!(stdout.contains("running: true"), "expected running: true, got: {stdout}");

    let (ok, _, stderr) = run_keelctl(&socket_path, &["delete", "web-1"]);
    assert!(ok, "delete failed: {stderr}");

    let (ok, _, stderr) = run_keelctl(&socket_path, &["get", "web-1"]);
    assert!(!ok, "expected get on a deleted jail to fail");
    assert!(stderr.contains("not found"), "expected 'not found' in stderr, got: {stderr}");
}

#[test]
fn apply_rejects_a_file_with_an_invalid_spec() {
    let socket_path = start_test_server("apply_rejects_a_file_with_an_invalid_spec");
    let path = std::env::temp_dir().join("keelctl-test-invalid-spec.yaml");
    std::fs::write(&path, "not: valid: yaml: [").unwrap();

    let (ok, _, stderr) = run_keelctl(&socket_path, &["apply", "-f", path.to_str().unwrap()]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn get_lists_multiple_applied_jails() {
    let socket_path = start_test_server("get_lists_multiple_applied_jails");
    run_keelctl(&socket_path, &["apply", "-f", write_spec_file("get_lists_multiple_applied_jails_1", "web-1").to_str().unwrap()]);
    run_keelctl(&socket_path, &["apply", "-f", write_spec_file("get_lists_multiple_applied_jails_2", "web-2").to_str().unwrap()]);

    let (ok, stdout, stderr) = run_keelctl(&socket_path, &["get"]);
    assert!(ok, "get failed: {stderr}");
    assert!(stdout.contains("web-1"));
    assert!(stdout.contains("web-2"));
}
