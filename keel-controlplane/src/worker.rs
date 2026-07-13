use crate::placements::Placements;
use crate::registry::{Registry, ResolveError, UnknownNode};
use crate::scheduler::{self, ScheduleError};
use crate::wire::{NodeState, NodeStatus};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleOrResolveError {
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("no known placement for jail '{0}'")]
    NotPlaced(String),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

pub enum Command {
    Register(String, String, Sender<()>),
    Heartbeat(String, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
    ResolveOrSchedule(String, Sender<Result<(String, String), ScheduleOrResolveError>>),
    ResolvePlacement(String, Sender<Result<(String, String), PlacementError>>),
    RecordPlacement(String, String, Sender<()>),
    RemovePlacement(String, Sender<()>),
}

pub fn spawn(mut registry: Registry, mut placements: Placements) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, &mut placements, command);
        }
    });
    (handle, tx)
}

fn handle_command(registry: &mut Registry, placements: &mut Placements, command: Command) {
    match command {
        Command::Register(id, addr, reply) => {
            // Stopgap literals, not defaults: `Command::Register` doesn't carry
            // capacity data yet (that's Task 6's job of threading it through from
            // the wire layer). Mirrors the same stopgap pattern Task 3 used in
            // `Registry::list()` pending Task 4.
            registry.register(id, addr, 0.0, 0, Instant::now());
            let _ = reply.send(());
        }
        Command::Heartbeat(id, reply) => {
            // Stopgap literals, not defaults: see the comment on `Command::Register`
            // above. `Command::Heartbeat` doesn't carry committed-resource data yet.
            let result = registry.heartbeat(&id, 0.0, 0, Instant::now());
            let _ = reply.send(result);
        }
        Command::List(reply) => {
            let _ = reply.send(registry.list(Instant::now()));
        }
        Command::Resolve(id, reply) => {
            let result = registry.resolve(&id, Instant::now());
            let _ = reply.send(result);
        }
        Command::ResolveOrSchedule(jail_name, reply) => {
            let now = Instant::now();
            let result = if let Some(node_id) = placements.get(&jail_name).map(|s| s.to_string()) {
                registry.resolve(&node_id, now).map(|addr| (node_id, addr)).map_err(ScheduleOrResolveError::from)
            } else {
                let alive_ids: Vec<String> = registry
                    .list(now)
                    .into_iter()
                    .filter(|status| status.status == NodeState::Alive)
                    .map(|status| status.id)
                    .collect();
                let counts = placements.counts();
                scheduler::pick_node(&alive_ids, &counts).map_err(ScheduleOrResolveError::from).and_then(
                    |node_id| {
                        registry
                            .resolve(&node_id, now)
                            .map(|addr| (node_id, addr))
                            .map_err(ScheduleOrResolveError::from)
                    },
                )
            };
            let _ = reply.send(result);
        }
        Command::ResolvePlacement(jail_name, reply) => {
            let result = match placements.get(&jail_name).map(|s| s.to_string()) {
                None => Err(PlacementError::NotPlaced(jail_name)),
                Some(node_id) => registry
                    .resolve(&node_id, Instant::now())
                    .map(|addr| (node_id, addr))
                    .map_err(PlacementError::from),
            };
            let _ = reply.send(result);
        }
        Command::RecordPlacement(jail_name, node_id, reply) => {
            placements.set(jail_name, node_id);
            let _ = reply.send(());
        }
        Command::RemovePlacement(jail_name, reply) => {
            placements.remove(&jail_name);
            let _ = reply.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_command_makes_the_node_visible_in_list() {
        let commands = spawn(Registry::new(), Placements::new()).1;

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
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("missing".to_string(), hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_err());
    }

    #[test]
    fn heartbeat_command_on_a_registered_node_succeeds() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("node-1".to_string(), hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_ok());
    }

    #[test]
    fn list_command_on_a_fresh_worker_is_empty() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        assert_eq!(list_rx.recv().unwrap(), vec![]);
    }

    #[test]
    fn resolve_command_on_a_registered_alive_node_returns_its_address() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register("node-1".to_string(), "10.0.0.1".to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("node-1".to_string(), resolve_tx)).unwrap();
        assert_eq!(resolve_rx.recv().unwrap(), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_command_on_an_unknown_node_returns_an_error() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("missing".to_string(), resolve_tx)).unwrap();
        assert!(resolve_rx.recv().unwrap().is_err());
    }

    fn register_node(commands: &Sender<Command>, id: &str, addr: &str) {
        let (reg_tx, reg_rx) = mpsc::channel();
        commands.send(Command::Register(id.to_string(), addr.to_string(), reg_tx)).unwrap();
        reg_rx.recv().unwrap();
    }

    #[test]
    fn resolve_or_schedule_on_a_fresh_jail_name_schedules_onto_the_least_loaded_alive_node() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1");
        register_node(&commands, "node-2", "10.0.0.2");

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("existing".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-2".to_string(), "10.0.0.2".to_string())));
    }

    #[test]
    fn resolve_or_schedule_on_an_already_placed_jail_is_sticky() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1");
        register_node(&commands, "node-2", "10.0.0.2");

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-1".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-1".to_string(), "10.0.0.1".to_string())));
    }

    #[test]
    fn resolve_or_schedule_with_no_alive_nodes_returns_no_available_nodes() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(ScheduleOrResolveError::Schedule(ScheduleError::NoAvailableNodes)));
    }

    #[test]
    fn resolve_placement_on_an_unplaced_jail_returns_not_placed() {
        let commands = spawn(Registry::new(), Placements::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(PlacementError::NotPlaced("web-1".to_string())));
    }

    #[test]
    fn record_then_remove_placement_is_reflected_by_resolve_placement() {
        let commands = spawn(Registry::new(), Placements::new()).1;
        register_node(&commands, "node-1", "10.0.0.1");

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-1".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-1".to_string(), "10.0.0.1".to_string())));

        let (rem_tx, rem_rx) = mpsc::channel();
        commands.send(Command::RemovePlacement("web-1".to_string(), rem_tx)).unwrap();
        rem_rx.recv().unwrap();

        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Err(PlacementError::NotPlaced("web-1".to_string())));
    }
}
