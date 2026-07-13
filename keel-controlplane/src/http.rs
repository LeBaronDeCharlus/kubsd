use crate::wire::{ErrorBody, NodeRegistration};
use crate::worker::{Command, ScheduleOrResolveError};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

pub fn run(listener: TcpListener, commands: Sender<Command>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        thread::spawn(move || {
            let _ = handle_connection(stream, &commands);
        });
    }
}

struct ParsedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response(&mut stream, status, &body)
}

fn read_request(stream: &mut TcpStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length) = loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let content_length = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_MESSAGE_BYTES {
                    return Ok(None);
                }
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Ok(None),
        }
    };

    let total_len = header_len + content_length;
    if total_len > MAX_MESSAGE_BYTES {
        return Ok(None);
    }
    while buf.len() < total_len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_len..total_len].to_vec();
    Ok(Some(ParsedRequest { method, path, body }))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["nodes", "register"]) => handle_register(&request.body, commands),
        ("POST", ["nodes", id, "heartbeat"]) => handle_heartbeat(id, commands),
        ("GET", ["nodes"]) => handle_list(commands),
        ("PUT", ["nodes", id, "jails", name]) => {
            let (status, body) = handle_forward(id, "PUT", &format!("/jails/{name}"), &request.body, commands);
            if (200..300).contains(&status) {
                send_record_placement(name, id, commands);
            }
            (status, body)
        }
        ("GET", ["nodes", id, "jails"]) => handle_forward(id, "GET", "/jails", &[], commands),
        ("GET", ["nodes", id, "jails", name]) => {
            handle_forward(id, "GET", &format!("/jails/{name}"), &[], commands)
        }
        ("DELETE", ["nodes", id, "jails", name]) => {
            let (status, body) = handle_forward(id, "DELETE", &format!("/jails/{name}"), &[], commands);
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, body)
        }
        ("PUT", ["jails", name]) => handle_scheduled_apply(name, &request.body, commands),
        ("GET", ["jails", name]) => handle_scheduled_read(name, commands),
        ("DELETE", ["jails", name]) => handle_scheduled_delete(name, commands),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}

fn handle_scheduled_apply(name: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ResolveOrSchedule(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let (node_id, addr) = match reply_rx.recv() {
        Ok(Ok(pair)) => pair,
        Ok(Err(ScheduleOrResolveError::Schedule(e))) => return error_response(503, e.to_string()),
        Ok(Err(ScheduleOrResolveError::Resolve(e))) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, "PUT", &format!("/jails/{name}"), body) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_record_placement(name, &node_id, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_read(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "GET", &format!("/jails/{name}"), &[]) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn handle_scheduled_delete(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (node_id, addr) = match resolve_placement(name, commands) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    match forward(&addr, "DELETE", &format!("/jails/{name}"), &[]) {
        Ok((status, response_body)) => {
            if (200..300).contains(&status) {
                send_remove_placement(name, commands);
            }
            (status, response_body)
        }
        Err(e) => error_response(500, format!("failed to reach node '{node_id}' at {addr}: {e}")),
    }
}

fn resolve_placement(name: &str, commands: &Sender<Command>) -> Result<(String, String), (u16, Vec<u8>)> {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::ResolvePlacement(name.to_string(), reply_tx)).is_err() {
        return Err(error_response(500, "control plane worker is not running".to_string()));
    }
    match reply_rx.recv() {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(e)) => Err(error_response(404, e.to_string())),
        Err(_) => Err(error_response(500, "control plane worker did not respond".to_string())),
    }
}

fn send_record_placement(name: &str, node_id: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RecordPlacement(name.to_string(), node_id.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}

fn send_remove_placement(name: &str, commands: &Sender<Command>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::RemovePlacement(name.to_string(), reply_tx)).is_ok() {
        let _ = reply_rx.recv();
    }
}

fn handle_register(body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let registration: NodeRegistration = match serde_yaml::from_slice(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Register(registration.id, registration.addr, reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(()) => (200, Vec::new()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_heartbeat(id: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Heartbeat(id.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(404, e.to_string()),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

fn handle_list(commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::List(reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(statuses) => yaml_response(200, &statuses),
        Err(_) => error_response(500, "control plane worker did not respond".to_string()),
    }
}

const FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const FORWARD_READ_TIMEOUT: Duration = Duration::from_secs(5);

fn handle_forward(id: &str, method: &str, path: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Resolve(id.to_string(), reply_tx)).is_err() {
        return error_response(500, "control plane worker is not running".to_string());
    }
    let addr = match reply_rx.recv() {
        Ok(Ok(addr)) => addr,
        Ok(Err(e)) => return error_response(404, e.to_string()),
        Err(_) => return error_response(500, "control plane worker did not respond".to_string()),
    };
    match forward(&addr, method, path, body) {
        Ok((status, response_body)) => (status, response_body),
        Err(e) => error_response(500, format!("failed to reach node '{id}' at {addr}: {e}")),
    }
}

fn forward(addr: &str, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "could not resolve address".to_string())?;
    let mut stream =
        TcpStream::connect_timeout(&socket_addr, FORWARD_CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(FORWARD_READ_TIMEOUT)).ok();

    let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(&response).map_err(|e| e.to_string())? {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Err("incomplete response".to_string()),
    };
    let status = parsed.code.ok_or_else(|| "missing status code".to_string())?;
    Ok((status, response[header_len..].to_vec()))
}

fn error_response(status: u16, message: String) -> (u16, Vec<u8>) {
    let body = serde_yaml::to_string(&ErrorBody { error: message })
        .expect("ErrorBody serialization should not fail");
    (status, body.into_bytes())
}

fn yaml_response<T: serde::Serialize>(status: u16, value: &T) -> (u16, Vec<u8>) {
    let body = serde_yaml::to_string(value).expect("wire type serialization should not fail");
    (status, body.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placements::Placements;
    use crate::registry::Registry;
    use crate::worker;

    fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());
        thread::spawn(move || run(listener, commands));
        addr
    }

    fn send_request(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).unwrap();
        if let Err(e) = stream.shutdown(std::net::Shutdown::Write) {
            assert_eq!(e.kind(), std::io::ErrorKind::NotConnected, "unexpected shutdown error: {e}");
        }

        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();

        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut parsed = httparse::Response::new(&mut headers);
        let header_len = match parsed.parse(&response).unwrap() {
            httparse::Status::Complete(len) => len,
            httparse::Status::Partial => panic!("incomplete response: {response:?}"),
        };
        let status = parsed.code.unwrap();
        let body = String::from_utf8(response[header_len..].to_vec()).unwrap();
        (status, body)
    }

    #[test]
    fn register_returns_200_and_the_node_appears_in_get_nodes() {
        let addr = start_test_server();
        let (status, _) = send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
        );
        assert_eq!(status, 200);

        let (status, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert!(body.contains("node-1"), "expected node-1 in body: {body}");
        assert!(body.contains("Alive"), "expected Alive status in body: {body}");
    }

    #[test]
    fn reregistering_the_same_id_updates_its_address_without_duplicating() {
        let addr = start_test_server();
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
        );
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.2\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
        );

        let (_, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(body.matches("node-1").count(), 1, "expected exactly one node-1 entry, got body: {body}");
        assert!(body.contains("10.0.0.2"), "expected refreshed address in body: {body}");
    }

    #[test]
    fn heartbeat_on_a_registered_node_returns_200() {
        let addr = start_test_server();
        send_request(
            &addr,
            "POST",
            "/nodes/register",
            "id: node-1\naddr: 10.0.0.1\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n",
        );

        let (status, _) = send_request(&addr, "POST", "/nodes/node-1/heartbeat", "");
        assert_eq!(status, 200);
    }

    #[test]
    fn heartbeat_on_an_unknown_node_returns_404() {
        let addr = start_test_server();
        let (status, body) = send_request(&addr, "POST", "/nodes/missing/heartbeat", "");
        assert_eq!(status, 404);
        assert!(body.contains("missing"));
    }

    #[test]
    fn get_nodes_on_an_empty_registry_returns_200_with_an_empty_list() {
        let addr = start_test_server();
        let (status, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert_eq!(body.trim(), "[]");
    }

    #[test]
    fn register_with_invalid_yaml_returns_400() {
        let addr = start_test_server();
        let (status, _) = send_request(&addr, "POST", "/nodes/register", "not: valid: yaml: at: all: -");
        assert_eq!(status, 400);
    }

    fn start_fake_remote_agentd(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Drain the whole request (forward() sends it as two
                // separate write_all calls, headers then body, followed by
                // shutdown(Write)) before responding. Reading only once can
                // catch just the first TCP segment under load, leaving the
                // rest unread when this stream is dropped at the end of the
                // loop body — on a BSD-derived TCP stack that can turn the
                // close into an RST instead of a clean FIN, which the real
                // client then sees as a spurious connection reset.
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        addr
    }

    fn register_node(cp_addr: &str, id: &str, node_addr: &str) {
        send_request(
            cp_addr,
            "POST",
            "/nodes/register",
            &format!("id: {id}\naddr: {node_addr}\ncapacity_cpu: 4.0\ncapacity_memory: 8589934592\n"),
        );
    }

    #[test]
    fn forward_put_relays_status_and_body_from_the_target_node() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/nodes/node-1/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("running: true"), "expected relayed body, got: {body}");
    }

    #[test]
    fn forward_get_relays_status_and_body_from_the_target_node() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "jails: fake-list\n");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, body) = send_request(&cp_addr, "GET", "/nodes/node-1/jails", "");
        assert_eq!(status, 200);
        assert!(body.contains("fake-list"), "expected relayed body, got: {body}");
    }

    #[test]
    fn forward_delete_relays_status_from_the_target_node() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, _) = send_request(&cp_addr, "DELETE", "/nodes/node-1/jails/web-1", "");
        assert_eq!(status, 200);
    }

    #[test]
    fn forward_to_an_unknown_node_returns_404() {
        let cp_addr = start_test_server();
        let (status, body) = send_request(&cp_addr, "GET", "/nodes/missing/jails", "");
        assert_eq!(status, 404);
        assert!(body.contains("unknown node"), "expected 'unknown node' in body: {body}");
    }

    #[test]
    fn forward_to_a_node_with_nothing_listening_returns_500() {
        let cp_addr = start_test_server();
        register_node(&cp_addr, "node-1", "127.0.0.1:1");

        let (status, body) = send_request(&cp_addr, "GET", "/nodes/node-1/jails", "");
        assert_eq!(status, 500);
        assert!(body.contains("failed to reach node"), "expected forwarding failure in body: {body}");
    }

    #[test]
    fn scheduled_put_lands_on_the_lower_id_node_when_counts_are_equal() {
        let cp_addr = start_test_server();
        let node_a_addr = start_fake_remote_agentd(200, "node: node-a\n");
        let node_b_addr = start_fake_remote_agentd(200, "node: node-b\n");
        register_node(&cp_addr, "node-b", &node_b_addr);
        register_node(&cp_addr, "node-a", &node_a_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("node-a"), "expected the lower id (node-a) to win the tie, got: {body}");
    }

    #[test]
    fn scheduled_put_is_sticky_across_repeated_apply() {
        let cp_addr = start_test_server();
        let node_a_addr = start_fake_remote_agentd(200, "node: node-a\n");
        register_node(&cp_addr, "node-a", &node_a_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("node-a"));

        // node-0 joins with a lower id and zero recorded jails, and would win
        // a fresh scheduling decision -- but web-1 is already placed, so it
        // must stay put.
        let node_0_addr = start_fake_remote_agentd(200, "node: node-0\n");
        register_node(&cp_addr, "node-0", &node_0_addr);

        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);
        assert!(body.contains("node-a"), "expected sticky placement on node-a, got: {body}");
    }

    #[test]
    fn scheduled_get_and_delete_on_an_unplaced_jail_return_404() {
        let cp_addr = start_test_server();

        let (status, body) = send_request(&cp_addr, "GET", "/jails/missing", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");

        let (status, body) = send_request(&cp_addr, "DELETE", "/jails/missing", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");
    }

    #[test]
    fn scheduled_delete_removes_the_placement_so_a_later_get_returns_404() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "node: node-a\n");
        register_node(&cp_addr, "node-a", &node_addr);

        send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        let (status, _) = send_request(&cp_addr, "DELETE", "/jails/web-1", "");
        assert_eq!(status, 200);

        let (status, body) = send_request(&cp_addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 404);
        assert!(body.contains("no known placement"), "got: {body}");
    }

    #[test]
    fn scheduled_put_with_no_alive_nodes_returns_503() {
        let cp_addr = start_test_server();
        let (status, body) = send_request(&cp_addr, "PUT", "/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 503);
        assert!(body.contains("no alive nodes"), "got: {body}");
    }

    #[test]
    fn named_route_apply_and_scheduled_route_share_the_same_placement_table() {
        let cp_addr = start_test_server();
        let node_addr = start_fake_remote_agentd(200, "running: true\n");
        register_node(&cp_addr, "node-1", &node_addr);

        let (status, _) = send_request(&cp_addr, "PUT", "/nodes/node-1/jails/web-1", "apiVersion: keel/v1\n");
        assert_eq!(status, 200);

        let (status, body) = send_request(&cp_addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 200, "expected the scheduled GET to find the placement recorded by the named-node PUT");
        assert!(body.contains("running: true"), "got: {body}");
    }
}
