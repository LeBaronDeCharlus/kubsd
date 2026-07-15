use crate::tls;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

// each parameter is independently needed by the registration loop; bundling into a struct would be over-engineering for this single call site
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
    capacity_cpu: f64,
    capacity_memory: u64,
    client_config: Arc<rustls::ClientConfig>,
    commands: Sender<crate::worker::Command>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr, capacity_cpu, capacity_memory, &client_config) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id, &commands, &client_config) {
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
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let body = format!(
        "id: {node_id}\naddr: {advertise_addr}\ncapacity_cpu: {capacity_cpu}\ncapacity_memory: {capacity_memory}\n"
    );
    send_request(control_plane_addr, "POST", "/nodes/register", &body, client_config)
}

fn heartbeat_once(
    control_plane_addr: &str,
    node_id: &str,
    commands: &Sender<crate::worker::Command>,
    client_config: &Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    commands
        .send(crate::worker::Command::CommittedResources(tx))
        .map_err(|_| "worker is not running".to_string())?;
    let (committed_cpu, committed_memory) = rx.recv().map_err(|_| "worker did not respond".to_string())?;
    let body = format!("committed_cpu: {committed_cpu}\ncommitted_memory: {committed_memory}\n");
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), &body, client_config)
}

fn send_request(addr: &str, method: &str, path: &str, body: &str, client_config: &Arc<rustls::ClientConfig>) -> Result<(), String> {
    let server_name = tls::server_name_from_addr(addr)?;
    let tcp_stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let conn = rustls::ClientConnection::new(Arc::clone(client_config), server_name).map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);

    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.sock.shutdown(std::net::Shutdown::Write).ok();

    // Read until the peer closes the connection. rustls surfaces a plain TCP
    // close that lacks a TLS `close_notify` alert as `ErrorKind::UnexpectedEof`
    // rather than `Ok(0)`, matching keel-controlplane's own `forward()`.
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("failed to read response: {e}")),
        }
    }

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

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/tls")).join(name)
    }

    fn start_test_control_plane() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        let tls_config = std::sync::Arc::new(
            crate::tls::load_server_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
                .unwrap(),
        );
        let client_config = std::sync::Arc::new(
            crate::tls::load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
                .unwrap(),
        );
        thread::spawn(move || keel_controlplane::http::run(listener, commands, tls_config, client_config));
        addr
    }

    fn node_client_config() -> std::sync::Arc<rustls::ClientConfig> {
        std::sync::Arc::new(
            crate::tls::load_client_config(&fixture("fixture-node.crt"), &fixture("fixture-node.key"), &fixture("ca.crt"))
                .unwrap(),
        )
    }

    fn wrong_ca_client_config() -> std::sync::Arc<rustls::ClientConfig> {
        std::sync::Arc::new(
            crate::tls::load_client_config(&fixture("wrong-ca-node.crt"), &fixture("wrong-ca-node.key"), &fixture("ca.crt"))
                .unwrap(),
        )
    }

    fn get_nodes(control_plane_addr: &str) -> String {
        let server_name = crate::tls::server_name_from_addr(control_plane_addr).unwrap();
        let tcp_stream = TcpStream::connect(control_plane_addr).unwrap();
        let conn = rustls::ClientConnection::new(node_client_config(), server_name).unwrap();
        let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
        stream
            .write_all(b"GET /nodes HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        stream.sock.shutdown(std::net::Shutdown::Write).ok();
        let mut response = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("failed to read response: {e}"),
            }
        }
        String::from_utf8_lossy(&response).to_string()
    }

    #[test]
    fn registers_and_then_keeps_heartbeating() {
        let control_plane_addr = start_test_control_plane();
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
            node_client_config(),
            commands,
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(body.contains("node-1"), "expected node-1 to have registered, got: {body}");
        assert!(body.contains("Alive"), "expected node-1 to be Alive, got: {body}");
        assert!(body.contains("capacity_cpu: 4"), "expected reported capacity in body: {body}");
    }

    #[test]
    fn heartbeats_report_the_reconcilers_committed_resources() {
        let control_plane_addr = start_test_control_plane();
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
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr_clone,
            Duration::from_millis(50),
            4.0,
            8 * 1024 * 1024 * 1024,
            node_client_config(),
            commands,
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(body.contains("committed_cpu: 2"), "expected committed_cpu from the applied jail, got: {body}");
        assert!(body.contains("committed_memory: 536870912"), "expected committed_memory from the applied jail, got: {body}");
    }

    #[test]
    fn registration_with_a_wrong_ca_certificate_never_registers() {
        let control_plane_addr = start_test_control_plane();
        let (_worker_handle, commands) = crate::worker::spawn(
            crate::Reconciler::new(
                keel_jail::FakeJailRuntime::new(),
                keel_zfs::FakeZfsManager::new(),
                keel_net::FakeNetManager::new(),
                "zroot".to_string(),
                std::env::temp_dir().join("keel-agentd-registration-test-registration_with_a_wrong_ca_certificate_never_registers"),
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
            wrong_ca_client_config(),
            commands,
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(!body.contains("node-1"), "expected node-1 to never register with a wrong-CA certificate, got: {body}");
    }
}
