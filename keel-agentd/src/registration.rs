use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub fn spawn(
    node_id: String,
    advertise_addr: String,
    control_plane_addr: String,
    heartbeat_interval: Duration,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut registered = false;
        loop {
            if !registered {
                match register_once(&control_plane_addr, &node_id, &advertise_addr) {
                    Ok(()) => registered = true,
                    Err(e) => eprintln!("keel-agentd: registration failed: {e}"),
                }
            } else {
                match heartbeat_once(&control_plane_addr, &node_id) {
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

fn register_once(control_plane_addr: &str, node_id: &str, advertise_addr: &str) -> Result<(), String> {
    let body = format!("id: {node_id}\naddr: {advertise_addr}\n");
    send_request(control_plane_addr, "POST", "/nodes/register", &body)
}

fn heartbeat_once(control_plane_addr: &str, node_id: &str) -> Result<(), String> {
    send_request(control_plane_addr, "POST", &format!("/nodes/{node_id}/heartbeat"), "")
}

fn send_request(addr: &str, method: &str, path: &str, body: &str) -> Result<(), String> {
    let mut stream =
        TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
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

    fn start_test_control_plane() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        thread::spawn(move || keel_controlplane::http::run(listener, commands));
        addr
    }

    fn get_nodes(control_plane_addr: &str) -> String {
        let mut stream = TcpStream::connect(control_plane_addr).unwrap();
        stream
            .write_all(b"GET /nodes HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).ok();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        String::from_utf8_lossy(&response).to_string()
    }

    #[test]
    fn registers_and_then_keeps_heartbeating() {
        let control_plane_addr = start_test_control_plane();
        let _handle = spawn(
            "node-1".to_string(),
            "10.0.0.1".to_string(),
            control_plane_addr.clone(),
            Duration::from_millis(50),
        );

        thread::sleep(Duration::from_millis(200));
        let body = get_nodes(&control_plane_addr);
        assert!(body.contains("node-1"), "expected node-1 to have registered, got: {body}");
        assert!(body.contains("Alive"), "expected node-1 to be Alive, got: {body}");
    }
}
