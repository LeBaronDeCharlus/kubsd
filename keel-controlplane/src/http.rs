use crate::wire::{ErrorBody, NodeRegistration};
use crate::worker::Command;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Sender};
use std::thread;

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
        500 => "Internal Server Error",
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
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
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
    use crate::registry::Registry;
    use crate::worker;

    fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (_worker_handle, commands) = worker::spawn(Registry::new());
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
        let (status, _) = send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1\n");
        assert_eq!(status, 200);

        let (status, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(status, 200);
        assert!(body.contains("node-1"), "expected node-1 in body: {body}");
        assert!(body.contains("Alive"), "expected Alive status in body: {body}");
    }

    #[test]
    fn reregistering_the_same_id_updates_its_address_without_duplicating() {
        let addr = start_test_server();
        send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1\n");
        send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.2\n");

        let (_, body) = send_request(&addr, "GET", "/nodes", "");
        assert_eq!(body.matches("node-1").count(), 1, "expected exactly one node-1 entry, got body: {body}");
        assert!(body.contains("10.0.0.2"), "expected refreshed address in body: {body}");
    }

    #[test]
    fn heartbeat_on_a_registered_node_returns_200() {
        let addr = start_test_server();
        send_request(&addr, "POST", "/nodes/register", "id: node-1\naddr: 10.0.0.1\n");

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
}
