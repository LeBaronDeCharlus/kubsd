use crate::registry::{Registry, ResolveError, UnknownNode};
use crate::wire::NodeStatus;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub enum Command {
    Register(String, String, Sender<()>),
    Heartbeat(String, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
}

pub fn spawn(mut registry: Registry) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, command);
        }
    });
    (handle, tx)
}

fn handle_command(registry: &mut Registry, command: Command) {
    match command {
        Command::Register(id, addr, reply) => {
            registry.register(id, addr, Instant::now());
            let _ = reply.send(());
        }
        Command::Heartbeat(id, reply) => {
            let result = registry.heartbeat(&id, Instant::now());
            let _ = reply.send(result);
        }
        Command::List(reply) => {
            let _ = reply.send(registry.list(Instant::now()));
        }
        Command::Resolve(id, reply) => {
            let result = registry.resolve(&id, Instant::now());
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_command_makes_the_node_visible_in_list() {
        let commands = spawn(Registry::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        let statuses = list_rx.recv().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-1");
    }

    #[test]
    fn heartbeat_command_on_unknown_id_returns_an_error() {
        let commands = spawn(Registry::new()).1;

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("missing".to_string(), hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_err());
    }

    #[test]
    fn heartbeat_command_on_a_registered_node_succeeds() {
        let commands = spawn(Registry::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("node-1".to_string(), hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_ok());
    }

    #[test]
    fn list_command_on_a_fresh_worker_is_empty() {
        let commands = spawn(Registry::new()).1;

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        assert_eq!(list_rx.recv().unwrap(), vec![]);
    }

    #[test]
    fn resolve_command_on_a_registered_alive_node_returns_its_address() {
        let commands = spawn(Registry::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("node-1".to_string(), resolve_tx)).unwrap();
        assert_eq!(resolve_rx.recv().unwrap(), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_command_on_an_unknown_node_returns_an_error() {
        let commands = spawn(Registry::new()).1;

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("missing".to_string(), resolve_tx)).unwrap();
        assert!(resolve_rx.recv().unwrap().is_err());
    }
}
