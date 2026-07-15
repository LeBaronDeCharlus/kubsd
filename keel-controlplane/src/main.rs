use keel_controlplane::placements::Placements;
use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

struct Config {
    addr: String,
    tls_ca_file: Option<PathBuf>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:7620".to_string(),
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
        }
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
            "--tls-ca-file" => config.tls_ca_file = Some(PathBuf::from(value)),
            "--tls-cert-file" => config.tls_cert_file = Some(PathBuf::from(value)),
            "--tls-key-file" => config.tls_key_file = Some(PathBuf::from(value)),
            other => panic!("unknown flag: {other}"),
        }
    }
    if config.tls_ca_file.is_none() || config.tls_cert_file.is_none() || config.tls_key_file.is_none() {
        panic!("--tls-ca-file, --tls-cert-file, and --tls-key-file are all required");
    }
    config
}

fn main() {
    let config = parse_args();
    let ca_file = config.tls_ca_file.expect("validated as required in parse_args_from");
    let cert_file = config.tls_cert_file.expect("validated as required in parse_args_from");
    let key_file = config.tls_key_file.expect("validated as required in parse_args_from");

    let tls_config = keel_controlplane::tls::load_server_config(&cert_file, &key_file, &ca_file)
        .unwrap_or_else(|e| panic!("failed to load TLS server config: {e}"));
    let client_config = keel_controlplane::tls::load_client_config(&cert_file, &key_file, &ca_file)
        .unwrap_or_else(|e| panic!("failed to load TLS client config: {e}"));

    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new(), Placements::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands, Arc::new(tls_config), Arc::new(client_config));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_the_tls_flags() {
        let config = parse_args_from(args(&[
            "--tls-ca-file", "/etc/keel/ca.crt",
            "--tls-cert-file", "/etc/keel/controlplane.crt",
            "--tls-key-file", "/etc/keel/controlplane.key",
        ]));
        assert_eq!(config.tls_ca_file, Some(PathBuf::from("/etc/keel/ca.crt")));
        assert_eq!(config.tls_cert_file, Some(PathBuf::from("/etc/keel/controlplane.crt")));
        assert_eq!(config.tls_key_file, Some(PathBuf::from("/etc/keel/controlplane.key")));
    }

    #[test]
    #[should_panic(expected = "--tls-ca-file, --tls-cert-file, and --tls-key-file are all required")]
    fn missing_any_tls_flag_panics() {
        parse_args_from(args(&["--tls-ca-file", "/etc/keel/ca.crt"]));
    }
}
