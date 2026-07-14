use crate::auth;
use crate::reconciler::ReconcileError;
use crate::wire::ErrorBody;
use crate::worker::Command;
use keel_spec::JailSpec;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

pub fn run(listener: UnixListener, commands: Sender<Command>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        thread::spawn(move || {
            let _ = handle_connection(stream, &commands);
        });
    }
}

pub fn run_tcp(listener: TcpListener, commands: Sender<Command>, token: Arc<String>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let commands = commands.clone();
        let token = Arc::clone(&token);
        thread::spawn(move || {
            let _ = handle_connection_tcp(stream, &commands, &token);
        });
    }
}

struct ParsedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
    auth_header: Option<String>,
}

fn handle_connection(mut stream: UnixStream, commands: &Sender<Command>) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route(&request, commands);
    write_response(&mut stream, status, &body)
}

fn handle_connection_tcp(mut stream: TcpStream, commands: &Sender<Command>, token: &str) -> io::Result<()> {
    let request = match read_request_tcp(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let (status, body) = route_authenticated(&request, commands, token);
    write_response_tcp(&mut stream, status, &body)
}

fn read_request(stream: &mut UnixStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length, auth_header) = loop {
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
                let auth_header = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(|v| v.trim().to_string());
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length, auth_header);
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
    Ok(Some(ParsedRequest { method, path, body, auth_header }))
}

fn read_request_tcp(stream: &mut TcpStream) -> io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let (method, path, header_len, content_length, auth_header) = loop {
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
                let auth_header = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(|v| v.trim().to_string());
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("").to_string();
                break (method, path, header_len, content_length, auth_header);
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
    Ok(Some(ParsedRequest { method, path, body, auth_header }))
}

fn write_response(stream: &mut UnixStream, status: u16, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/yaml\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_response_tcp(stream: &mut TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
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
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn route(request: &ParsedRequest, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let segments: Vec<&str> =
        request.path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("PUT", ["jails", name]) => handle_apply(name, &request.body, commands),
        ("GET", ["jails"]) => handle_get(None, commands),
        ("GET", ["jails", name]) => handle_get(Some(name.to_string()), commands),
        ("DELETE", ["jails", name]) => handle_delete(name, commands),
        _ => error_response(404, format!("no route for {} {}", request.method, request.path)),
    }
}

fn route_authenticated(request: &ParsedRequest, commands: &Sender<Command>, token: &str) -> (u16, Vec<u8>) {
    if !auth::check(request.auth_header.as_deref(), token) {
        return error_response(401, "missing or invalid auth token".to_string());
    }
    route(request, commands)
}

fn handle_apply(path_name: &str, body: &[u8], commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let spec: JailSpec = match serde_yaml::from_slice(body) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("invalid YAML: {e}")),
    };
    if spec.metadata.name != path_name {
        return error_response(
            400,
            format!("path name '{path_name}' does not match spec.metadata.name '{}'", spec.metadata.name),
        );
    }

    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Apply(spec, reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}

fn handle_get(name: Option<String>, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Get(name.clone(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    let statuses = match reply_rx.recv() {
        Ok(s) => s,
        Err(_) => return error_response(500, "reconciler worker did not respond".to_string()),
    };
    match name {
        Some(n) => match statuses.into_iter().next() {
            Some(status) => yaml_response(200, &status),
            None => error_response(404, format!("jail '{n}' not found")),
        },
        None => yaml_response(200, &statuses),
    }
}

fn handle_delete(name: &str, commands: &Sender<Command>) -> (u16, Vec<u8>) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if commands.send(Command::Delete(name.to_string(), reply_tx)).is_err() {
        return error_response(500, "reconciler worker is not running".to_string());
    }
    match reply_rx.recv() {
        Ok(Ok(())) => (200, Vec::new()),
        Ok(Err(e)) => error_response(status_for_error(&e), e.to_string()),
        Err(_) => error_response(500, "reconciler worker did not respond".to_string()),
    }
}

fn status_for_error(error: &ReconcileError) -> u16 {
    match error {
        ReconcileError::InvalidSpec(keel_spec::SpecError::ImmutableField(_)) => 409,
        ReconcileError::InvalidSpec(_) => 400,
        ReconcileError::NotFound(_) => 404,
        _ => 500,
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
    use crate::reconciler::Reconciler;
    use crate::worker;
    use keel_jail::FakeJailRuntime;
    use keel_net::FakeNetManager;
    use keel_zfs::FakeZfsManager;
    use std::path::PathBuf;

    fn sample_spec_yaml(name: &str) -> String {
        format!(
            "apiVersion: keel/v1\nkind: Jail\nmetadata:\n  name: {name}\nspec:\n  image: base/14.2-web\n  command: [\"/usr/local/bin/myapp\"]\n  network:\n    vnet: true\n    bridge: keel0\n    address: 10.0.0.5/24\n  resources:\n    cpu: \"2\"\n    memory: 512M\n  restartPolicy: Always\n"
        )
    }

    fn short_unique_socket_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ka-{}-{}.sock", std::process::id(), id))
    }

    fn start_test_server(name: &str) -> PathBuf {
        let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-test-state-{name}"));
        let _ = std::fs::remove_dir_all(&state_dir);
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(
            FakeJailRuntime::new(),
            zfs,
            FakeNetManager::new(),
            "zroot".to_string(),
            state_dir,
        )
        .unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let socket_path = short_unique_socket_path();
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();
        thread::spawn(move || run(listener, commands));
        socket_path
    }

    fn send_request(socket_path: &PathBuf, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = UnixStream::connect(socket_path).unwrap();
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).unwrap();
        // Under heavy scheduling contention (observed intermittently during
        // Milestone 5 VM verification, running the full workspace suite
        // concurrently), the server can finish reading the request,
        // respond, and close its end before we get here — the socket is
        // then already fully disconnected, a harmless race since our goal
        // (signal EOF so the server stops expecting more body) is already
        // moot if the server's done reading.
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
    fn put_valid_spec_returns_200_and_provisions_the_jail() {
        let socket_path = start_test_server("put_valid_spec_returns_200_and_provisions_the_jail");
        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        assert_eq!(status, 200);

        let (status, body) = send_request(&socket_path, "GET", "/jails/web-1", "");
        assert_eq!(status, 200);
        assert!(body.contains("running: true"), "expected running: true in body: {body}");
    }

    #[test]
    fn put_with_mismatched_path_and_body_name_returns_400() {
        let socket_path = start_test_server("put_with_mismatched_path_and_body_name_returns_400");
        let (status, body) = send_request(&socket_path, "PUT", "/jails/other-name", &sample_spec_yaml("web-1"));
        assert_eq!(status, 400);
        assert!(body.contains("does not match"));
    }

    #[test]
    fn put_changing_an_immutable_field_returns_409() {
        let socket_path = start_test_server("put_changing_an_immutable_field_returns_409");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));

        let changed_yaml = sample_spec_yaml("web-1").replace("base/14.2-web", "base/different-image");
        let (status, _) = send_request(&socket_path, "PUT", "/jails/web-1", &changed_yaml);
        assert_eq!(status, 409);
    }

    #[test]
    fn get_on_unknown_name_returns_404() {
        let socket_path = start_test_server("get_on_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "GET", "/jails/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn delete_on_unknown_name_returns_404() {
        let socket_path = start_test_server("delete_on_unknown_name_returns_404");
        let (status, _) = send_request(&socket_path, "DELETE", "/jails/missing", "");
        assert_eq!(status, 404);
    }

    #[test]
    fn delete_removes_a_provisioned_jail() {
        let socket_path = start_test_server("delete_removes_a_provisioned_jail");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        let (status, _) = send_request(&socket_path, "DELETE", "/jails/web-1", "");
        assert_eq!(status, 200);

        let (status, _) = send_request(&socket_path, "GET", "/jails/web-1", "");
        assert_eq!(status, 404, "deleted jail should no longer be found");
    }

    #[test]
    fn get_jails_lists_all_applied_jails() {
        let socket_path = start_test_server("get_jails_lists_all_applied_jails");
        send_request(&socket_path, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        send_request(&socket_path, "PUT", "/jails/web-2", &sample_spec_yaml("web-2"));

        let (status, body) = send_request(&socket_path, "GET", "/jails", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-1"));
        assert!(body.contains("web-2"));
    }

    #[test]
    fn oversized_content_length_closes_the_connection_without_reading_the_body() {
        let socket_path =
            start_test_server("oversized_content_length_closes_the_connection_without_reading_the_body");
        let mut stream = UnixStream::connect(&socket_path).unwrap();
        let request = format!(
            "PUT /jails/web-1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            MAX_MESSAGE_BYTES + 1
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).ok();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(
            response.is_empty(),
            "server should close the connection without responding to an oversized request, got: {response:?}"
        );
    }

    const TEST_TOKEN: &str = "test-token-123";

    fn start_tcp_test_server(name: &str) -> String {
        let state_dir = std::env::temp_dir().join(format!("keel-agentd-http-tcp-test-state-{name}"));
        let _ = std::fs::remove_dir_all(&state_dir);
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(
            FakeJailRuntime::new(),
            zfs,
            FakeNetManager::new(),
            "zroot".to_string(),
            state_dir,
        )
        .unwrap();
        let (_worker_handle, commands) = worker::spawn(reconciler);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let token = Arc::new(TEST_TOKEN.to_string());
        thread::spawn(move || run_tcp(listener, commands, token));
        addr
    }

    fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TEST_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
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

    fn send_request_tcp_raw(addr: &str, method: &str, path: &str, body: &str, auth_header: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let auth_line = match auth_header {
            Some(h) => format!("Authorization: {h}\r\n"),
            None => String::new(),
        };
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth_line}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
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
    fn put_valid_spec_over_tcp_returns_200_and_provisions_the_jail() {
        let addr = start_tcp_test_server("put_valid_spec_over_tcp_returns_200_and_provisions_the_jail");
        let (status, _) = send_request_tcp(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        assert_eq!(status, 200);

        let (status, body) = send_request_tcp(&addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 200);
        assert!(body.contains("running: true"), "expected running: true in body: {body}");
    }

    #[test]
    fn get_jails_over_tcp_lists_all_applied_jails() {
        let addr = start_tcp_test_server("get_jails_over_tcp_lists_all_applied_jails");
        send_request_tcp(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        send_request_tcp(&addr, "PUT", "/jails/web-2", &sample_spec_yaml("web-2"));

        let (status, body) = send_request_tcp(&addr, "GET", "/jails", "");
        assert_eq!(status, 200);
        assert!(body.contains("web-1"));
        assert!(body.contains("web-2"));
    }

    #[test]
    fn delete_over_tcp_removes_a_provisioned_jail() {
        let addr = start_tcp_test_server("delete_over_tcp_removes_a_provisioned_jail");
        send_request_tcp(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"));
        let (status, _) = send_request_tcp(&addr, "DELETE", "/jails/web-1", "");
        assert_eq!(status, 200);

        let (status, _) = send_request_tcp(&addr, "GET", "/jails/web-1", "");
        assert_eq!(status, 404, "deleted jail should no longer be found");
    }

    #[test]
    fn put_over_tcp_without_auth_header_returns_401() {
        let addr = start_tcp_test_server("put_over_tcp_without_auth_header_returns_401");
        let (status, _) = send_request_tcp_raw(&addr, "PUT", "/jails/web-1", &sample_spec_yaml("web-1"), None);
        assert_eq!(status, 401);
    }

    #[test]
    fn get_jails_over_tcp_with_wrong_auth_token_returns_401() {
        let addr = start_tcp_test_server("get_jails_over_tcp_with_wrong_auth_token_returns_401");
        let (status, _) = send_request_tcp_raw(&addr, "GET", "/jails", "", Some("Bearer wrong-token"));
        assert_eq!(status, 401);
    }
}
