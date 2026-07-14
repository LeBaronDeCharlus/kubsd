use keel_agentd::ErrorBody;
use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_SOCKET: &str = "/var/run/keel-agentd.sock";

#[derive(Debug, PartialEq)]
enum Target {
    Socket(PathBuf),
    ControlPlane { addr: String, node: Option<String>, token: String },
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let socket = extract_socket_flag(&mut args).unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    let control_plane_addr = extract_flag(&mut args, "--control-plane-addr");
    let node = extract_flag(&mut args, "--node");
    let auth_token_file = extract_flag(&mut args, "--auth-token-file");

    let target = match resolve_target(socket, control_plane_addr, node, auth_token_file) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::FAILURE;
        }
    };

    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "apply" => run_apply(&target, rest),
        Some((cmd, rest)) if cmd == "get" => run_get(&target, rest),
        Some((cmd, rest)) if cmd == "delete" => run_delete(&target, rest),
        _ => {
            eprintln!(
                "usage: keelctl <apply -f FILE|get [name]|delete NAME> [--socket PATH|--control-plane-addr ADDR --node ID]"
            );
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn extract_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.remove(index);
    Some(args.remove(index))
}

fn extract_socket_flag(args: &mut Vec<String>) -> Option<PathBuf> {
    extract_flag(args, "--socket").map(PathBuf::from)
}

fn resolve_target(
    socket: PathBuf,
    control_plane_addr: Option<String>,
    node: Option<String>,
    auth_token_file: Option<String>,
) -> Result<Target, String> {
    match (control_plane_addr, node, auth_token_file) {
        (Some(addr), node, Some(path)) => {
            let token = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {path}: {e}"))?
                .trim()
                .to_string();
            Ok(Target::ControlPlane { addr, node, token })
        }
        (Some(_), _, None) => Err("--auth-token-file is required with --control-plane-addr".to_string()),
        (None, Some(_), _) => Err("--node requires --control-plane-addr".to_string()),
        (None, None, _) => Ok(Target::Socket(socket)),
    }
}

fn jails_path(target: &Target, suffix: &str) -> String {
    match target {
        Target::Socket(_) => suffix.to_string(),
        Target::ControlPlane { node: Some(node), .. } => format!("/nodes/{node}{suffix}"),
        Target::ControlPlane { node: None, .. } => suffix.to_string(),
    }
}

fn dispatch(target: &Target, method: &str, path: &str, body: &str) -> Result<String, String> {
    match target {
        Target::Socket(socket) => send_request(socket, method, path, body),
        Target::ControlPlane { addr, token, .. } => send_request_tcp(addr, method, path, body, token),
    }
}

fn run_apply(target: &Target, args: &[String]) -> Result<String, String> {
    let index = args.iter().position(|a| a == "-f").ok_or("apply requires -f FILE")?;
    let file = args.get(index + 1).ok_or("apply requires -f FILE")?;
    let yaml = std::fs::read_to_string(file).map_err(|e| format!("failed to read {file}: {e}"))?;
    let spec = keel_spec::parse_and_validate(&yaml).map_err(|e| format!("invalid spec: {e}"))?;
    let path = jails_path(target, &format!("/jails/{}", spec.metadata.name));
    dispatch(target, "PUT", &path, &yaml).map(|_| String::new())
}

fn run_get(target: &Target, args: &[String]) -> Result<String, String> {
    let suffix = match args.first() {
        Some(name) => format!("/jails/{name}"),
        None => "/jails".to_string(),
    };
    let path = jails_path(target, &suffix);
    dispatch(target, "GET", &path, "")
}

fn run_delete(target: &Target, args: &[String]) -> Result<String, String> {
    let name = args.first().ok_or("delete requires a jail name")?;
    let path = jails_path(target, &format!("/jails/{name}"));
    dispatch(target, "DELETE", &path, "").map(|_| String::new())
}

fn send_request(socket: &PathBuf, method: &str, path: &str, body: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("failed to connect to {}: {e}", socket.display()))?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;
    parse_response(&response)
}

fn send_request_tcp(addr: &str, method: &str, path: &str, body: &str, token: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("failed to send request: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("failed to read response: {e}"))?;
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> Result<String, String> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(response).map_err(|e| format!("malformed response: {e}"))? {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Err("incomplete response from server".to_string()),
    };
    let status = parsed.code.unwrap_or(0);
    let response_body = String::from_utf8_lossy(&response[header_len..]).to_string();

    if (200..300).contains(&status) {
        Ok(response_body)
    } else {
        let error: ErrorBody =
            serde_yaml::from_str(&response_body).unwrap_or(ErrorBody { error: response_body });
        Err(error.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_control_plane_flags_yields_socket_target() {
        let target = resolve_target(PathBuf::from("/var/run/keel-agentd.sock"), None, None, None).unwrap();
        assert_eq!(target, Target::Socket(PathBuf::from("/var/run/keel-agentd.sock")));
    }

    #[test]
    fn node_without_control_plane_addr_is_an_error() {
        let err = resolve_target(PathBuf::from("/var/run/keel-agentd.sock"), None, Some("node-1".to_string()), None)
            .unwrap_err();
        assert_eq!(err, "--node requires --control-plane-addr");
    }

    #[test]
    fn control_plane_addr_without_auth_token_file_is_an_error() {
        let err = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            Some("10.0.0.1:7620".to_string()),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, "--auth-token-file is required with --control-plane-addr");
    }

    #[test]
    fn control_plane_addr_with_auth_token_file_reads_the_token() {
        let dir = std::env::temp_dir().join(format!("keelctl-resolve-target-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_path = dir.join("token");
        std::fs::write(&token_path, "secret123\n").unwrap();

        let target = resolve_target(
            PathBuf::from("/var/run/keel-agentd.sock"),
            Some("10.0.0.1:7620".to_string()),
            Some("node-1".to_string()),
            Some(token_path.to_str().unwrap().to_string()),
        )
        .unwrap();
        assert_eq!(
            target,
            Target::ControlPlane {
                addr: "10.0.0.1:7620".to_string(),
                node: Some("node-1".to_string()),
                token: "secret123".to_string(),
            }
        );
    }
}
