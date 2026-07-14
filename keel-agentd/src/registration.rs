use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    token: String,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr, capacity_cpu, capacity_memory, &token) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id, &commands, &token) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("keel-agentd: heartbeat failed: {e}");
                        registered = false;
                    }
                }
            }
            thread::sleep(heartbeat_interval);
        }
    })
}

fn register_once(
    control_plane_addr: &str,
    node_id: &str,
    advertise_addr: &str,
    capacity_cpu: f64,
    capacity_memory: u64,
    token: &str,
) -> Result<(), String> {
    let body = format!(
        "id: {node_id}\naddr: {advertise_addr}\ncapacity_cpu: {capacity_cpu}\ncapacity_memory: {capacity_memory}\n"
    );
    send_request(control_plane_addr, "POST", "/nodes/register", &body, token)
}

fn heartbeat_once(control_plane_addr: &str, node_id: &str, commands: &Sender<crate::worker::Command>, token: &str) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, token)
}

fn send_request(addr: &str, method: &str, path: &str, body: &str, token: &str) -> Result<(), String> {
    let mut stream =
        TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&response).map_err(|e| format!("malformed response: {e}"))? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err("incomplete response from control plane".to_string()),
    };
    let status = parsed.code.unwrap_or(0);
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("control plane returned status {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_controlplane::placements::Placements;
    use keel_controlplane::registry::Registry;
    use keel_controlplane::worker;
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn start_test_control_plane(token: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let token = std::sync::Arc::new(token.to_string());
        thread::spawn(move || keel_controlplane::http::run(listener, commands, token));
        addr
    }

    fn get_nodes(control_plane_addr: &str, token: &str) -> String {
        let mut stream = TcpStream::connect(control_plane_addr).unwrap();
        stream
            .write_all(format!("GET /nodes HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n").as_bytes())
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).ok();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        String::from_utf8_lossy(&response).to_string()
    }

    #[test]
    fn registers_and_then_keeps_heartbeating() {
        let control_plane_addr = start_test_control_plane("test-token");
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                keel_net::FakeNetManager::new(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-registers_and_then_keeps_heartbeating"),
            )
            .unwrap(),
        );
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            "test-token".to_string(),
            commands,
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr, "test-token");
        assert!(body.contains("node-1"), "expected node-1 to have registered, got: {body}");
        assert!(body.contains("Alive"), "expected node-1 to be Alive, got: {body}");
        assert!(body.contains("capacity_cpu: 4"), "expected reported capacity in body: {body}");
    }

    #[test]
    fn heartbeats_report_the_reconcilers_committed_resources() {
        let control_plane_addr = start_test_control_plane("test-token");
        let zfs = keel_zfs::FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = crate::Reconciler::new(
            keel_jail::FakeJailRuntime::new(),
            zfs,
            keel_net::FakeNetManager::new(),
            "zroot".to_string(),
            std::env::temp_dir().join("keel-agentd-registration-test-heartbeats_report_the_reconcilers_committed_resources"),
        )
        .unwrap();
        let (_worker_handle, commands) = crate::worker::spawn(reconciler);

        let (apply_tx, apply_rx) = mpsc::channel();
        commands
            .send(crate::worker::Command::Apply(
                keel_spec::JailSpec {
                    api_version: "keel/v1".to_string(),
                    kind: "Jail".to_string(),
                    metadata: keel_spec::Metadata { name: "web-1".to_string() },
                    spec: keel_spec::Spec {
                        image: "base/14.2-web".to_string(),
                        command: vec!["/usr/local/bin/myapp".to_string()],
                        network: keel_spec::NetworkSpec {
                            vnet: true,
                            bridge: "keel0".to_string(),
                            address: "10.0.0.5/24".to_string(),
                        },
                        resources: keel_spec::ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                        restart_policy: keel_spec::RestartPolicy::Always,
                    },
                },
                apply_tx,
            ))
            .unwrap();
        apply_rx.recv().unwrap().unwrap();

        let control_plane_addr_clone = control_plane_addr.clone();
        let _handle =
            spawn("node-1".to_string(), "10.0.0.1".to_string(), control_plane_addr_clone, Duration::from_millis(50), 4.0, 8 * 1024 * 1024 * 1024, "test-token".to_string(), commands);

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr, "test-token");
        assert!(body.contains("committed_cpu: 2"), "expected committed_cpu from the applied jail, got: {body}");
        assert!(body.contains("committed_memory: 536870912"), "expected committed_memory from the applied jail, got: {body}");
    }

    #[test]
    fn registration_with_a_mismatched_token_never_registers() {
        let control_plane_addr = start_test_control_plane("correct-token");
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                keel_net::FakeNetManager::new(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-registration_with_a_mismatched_token_never_registers"),
            )
            .unwrap(),
        );
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            "wrong-token".to_string(),
            commands,
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr, "correct-token");
        assert!(!body.contains("node-1"), "expected node-1 to never register with a mismatched token, got: {body}");
    }
}
