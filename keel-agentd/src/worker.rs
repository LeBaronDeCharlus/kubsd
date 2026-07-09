use crate::reconciler::{ReconcileError, Reconciler};
use crate::wire::JailStatus;
use keel_jail::JailRuntime;
use keel_net::NetManager;
use keel_spec::JailSpec;
use keel_zfs::ZfsManager;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub enum Command {
    Apply(JailSpec, Sender<Result<(), ReconcileError>>),
    Get(Option<String>, Sender<Vec<JailStatus>>),
    Delete(String, Sender<Result<(), ReconcileError>>),
    Tick,
}

pub fn spawn<J, Z, N>(mut reconciler: Reconciler<J, Z, N>) -> (JoinHandle<()>, Sender<Command>)
where
    J: JailRuntime + Send + 'static,
    Z: ZfsManager + Send + 'static,
    N: NetManager + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut reconciler, command);
        }
    });
    (handle, tx)
}

fn handle_command<J: JailRuntime, Z: ZfsManager, N: NetManager>(
    reconciler: &mut Reconciler<J, Z, N>,
    command: Command,
) {
    match command {
        Command::Apply(spec, reply) => {
            let result = reconciler.apply(spec);
            // Reconcile immediately so a client's apply/delete call
            // observes its effects by the time it gets a response,
            // rather than waiting for the next timer tick. The resulting
            // per-jail failures (if any) are already surfaced via each
            // jail's backoff status on a later `get`, so they're
            // discarded here (same treatment as a plain `Tick`).
            let _ = reconciler.reconcile(Instant::now());
            let _ = reply.send(result);
        }
        Command::Delete(name, reply) => {
            let result = reconciler.delete(&name);
            let _ = reconciler.reconcile(Instant::now());
            let _ = reply.send(result);
        }
        Command::Get(name, reply) => {
            let now = Instant::now();
            let statuses = match name {
                Some(n) => reconciler.get(&n, now).into_iter().collect(),
                None => reconciler.list(now),
            };
            let _ = reply.send(statuses);
        }
        Command::Tick => {
            let _ = reconciler.reconcile(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_jail::FakeJailRuntime;
    use keel_net::FakeNetManager;
    use keel_spec::{Metadata, NetworkSpec, RestartPolicy, ResourcesSpec, Spec};
    use keel_zfs::FakeZfsManager;
    use std::path::PathBuf;

    fn sample_spec(name: &str) -> JailSpec {
        JailSpec {
            api_version: "keel/v1".to_string(),
            kind: "Jail".to_string(),
            metadata: Metadata { name: name.to_string() },
            spec: Spec {
                image: "base/14.2-web".to_string(),
                command: vec!["/usr/local/bin/myapp".to_string()],
                network: NetworkSpec {
                    vnet: true,
                    bridge: "keel0".to_string(),
                    address: "10.0.0.5/24".to_string(),
                },
                resources: ResourcesSpec { cpu: "2".to_string(), memory: "512M".to_string() },
                restart_policy: RestartPolicy::Always,
            },
        }
    }

    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-agentd-worker-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn spawn_test_worker(name: &str) -> Sender<Command> {
        let zfs = FakeZfsManager::new();
        zfs.seed_dataset("zroot/keel/base/14.2-web");
        let reconciler = Reconciler::new(
            FakeJailRuntime::new(),
            zfs,
            FakeNetManager::new(),
            "zroot".to_string(),
            test_state_dir(name),
        )
        .unwrap();
        let (_handle, commands) = spawn(reconciler);
        commands
    }

    #[test]
    fn apply_command_persists_and_reconciles_immediately() {
        let commands = spawn_test_worker("apply_command_persists_and_reconciles_immediately");

        let (reply_tx, reply_rx) = mpsc::channel();
        commands.send(Command::Apply(sample_spec("web-1"), reply_tx)).unwrap();
        assert!(reply_rx.recv().unwrap().is_ok());

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(Some("web-1".to_string()), get_tx)).unwrap();
        let statuses = get_rx.recv().unwrap();
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].running, "expected apply to trigger an immediate reconcile that provisions the jail");
    }

    #[test]
    fn invalid_apply_command_returns_an_error_without_crashing_the_worker() {
        let commands = spawn_test_worker("invalid_apply_command_returns_an_error_without_crashing_the_worker");
        let mut invalid = sample_spec("web-1");
        invalid.metadata.name = "Invalid_Name".to_string();

        let (reply_tx, reply_rx) = mpsc::channel();
        commands.send(Command::Apply(invalid, reply_tx)).unwrap();
        assert!(matches!(reply_rx.recv().unwrap(), Err(ReconcileError::InvalidSpec(_))));

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(None, get_tx)).unwrap();
        assert!(get_rx.recv().unwrap().is_empty());
    }

    #[test]
    fn delete_command_removes_the_record() {
        let commands = spawn_test_worker("delete_command_removes_the_record");

        let (apply_tx, apply_rx) = mpsc::channel();
        commands.send(Command::Apply(sample_spec("web-1"), apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();

        let (delete_tx, delete_rx) = mpsc::channel();
        commands.send(Command::Delete("web-1".to_string(), delete_tx)).unwrap();
        assert!(delete_rx.recv().unwrap().is_ok());

        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(None, get_tx)).unwrap();
        assert!(get_rx.recv().unwrap().is_empty());
    }

    #[test]
    fn delete_command_on_unknown_name_returns_not_found() {
        let commands = spawn_test_worker("delete_command_on_unknown_name_returns_not_found");
        let (delete_tx, delete_rx) = mpsc::channel();
        commands.send(Command::Delete("missing".to_string(), delete_tx)).unwrap();
        assert!(matches!(delete_rx.recv().unwrap(), Err(ReconcileError::NotFound(_))));
    }

    #[test]
    fn tick_command_is_processed_without_blocking_subsequent_commands() {
        let commands = spawn_test_worker("tick_command_is_processed_without_blocking_subsequent_commands");
        commands.send(Command::Tick).unwrap();

        // mpsc is FIFO: this Get is only answered once Tick has already
        // been processed, proving Tick doesn't hang or crash the worker.
        let (get_tx, get_rx) = mpsc::channel();
        commands.send(Command::Get(None, get_tx)).unwrap();
        assert!(get_rx.recv().unwrap().is_empty());
    }
}
