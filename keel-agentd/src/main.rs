use keel_agentd::worker::{self, Command};
use keel_agentd::Reconciler;
use keel_jail::ProcessJailRuntime;
use keel_net::ProcessNetManager;
use keel_zfs::CliZfsManager;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

struct Config {
    pool: String,
    state_dir: PathBuf,
    socket: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pool: "zroot".to_string(),
            state_dir: PathBuf::from("/var/db/keel"),
            socket: PathBuf::from("/var/run/keel-agentd.sock"),
        }
    }
}

fn parse_args() -> Config {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--pool" => config.pool = value,
            "--state-dir" => config.state_dir = PathBuf::from(value),
            "--socket" => config.socket = PathBuf::from(value),
            other => panic!("unknown flag: {other}"),
        }
    }
    config
}

fn main() {
    let config = parse_args();

    let reconciler = Reconciler::new(
        ProcessJailRuntime::new(),
        CliZfsManager::new(),
        ProcessNetManager::new(),
        config.pool.clone(),
        config.state_dir.clone(),
    )
    .expect("failed to initialize reconciler from on-disk state");

    eprintln!(
        "keel-agentd: starting (pool={}, state_dir={}, socket={})",
        config.pool,
        config.state_dir.display(),
        config.socket.display()
    );

    let (_worker_handle, commands) = worker::spawn(reconciler);

    let timer_commands = commands.clone();
    thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        if timer_commands.send(Command::Tick).is_err() {
            break;
        }
    });

    if config.socket.exists() {
        std::fs::remove_file(&config.socket).expect("failed to remove stale socket file");
    }
    let listener = UnixListener::bind(&config.socket).expect("failed to bind Unix socket");
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))
        .expect("failed to set socket permissions");

    keel_agentd::http::run(listener, commands);
}
