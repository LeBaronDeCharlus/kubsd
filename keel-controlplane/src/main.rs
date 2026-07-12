use keel_controlplane::registry::Registry;
use keel_controlplane::worker;
use std::net::TcpListener;

struct Config {
    addr: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { addr: "0.0.0.0:7620".to_string() }
    }
}

fn parse_args() -> Config {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--addr" => config.addr = value,
            other => panic!("unknown flag: {other}"),
        }
    }
    config
}

fn main() {
    let config = parse_args();
    eprintln!("keel-controlplane: starting (addr={})", config.addr);

    let (_worker_handle, commands) = worker::spawn(Registry::new());

    let listener = TcpListener::bind(&config.addr).expect("failed to bind TCP listener");
    keel_controlplane::http::run(listener, commands);
}
