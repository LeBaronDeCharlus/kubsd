use keel_controlplane::placements::Placements;
use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

struct Config {
    addr: String,
    auth_token_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self { addr: "0.0.0.0:7620".to_string(), auth_token_file: None }
    }
}

fn parse_args() -> Config {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Config {
    let mut config = Config::default();
    let mut args = args;
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--addr" => config.addr = value,
            "--auth-token-file" => config.auth_token_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.auth_token_file.is_none() {
        panic!("--auth-token-file is required");
    }
    config
}

fn main() {
    let config = parse_args();
    let auth_token_file = config.auth_token_file.expect("validated as required in parse_args_from");
    let token = keel_controlplane::auth::load_token(&auth_token_file)
        .unwrap_or_else(|e| panic!("failed to load auth token: {e}"));
    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, Arc::new(token));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_the_auth_token_file_flag() {
        let config = parse_args_from(args(&["--auth-token-file", "/etc/keel/token"]));
        assert_eq!(config.auth_token_file, Some(PathBuf::from("/etc/keel/token")));
    }

    #[test]
    #[should_panic(expected = "--auth-token-file is required")]
    fn missing_auth_token_file_panics() {
        parse_args_from(args(&["--addr", "0.0.0.0:7620"]));
    }
}
