use crate::placements::Placements;
use crate::registry::{PodCidrCollision, Registry, ResolveError, UnknownNode};
use crate::scheduler::{self, ScheduleError};
use crate::wire::{NodeState, NodeStatus};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use crate::addresses::UsedAddresses;
use crate::services::{self, Owner, Services};

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
    Register(String, String, f64, u64, Sender<Result<ipnet::Ipv4Net, PodCidrCollision>>),
    Heartbeat(String, f64, u64, Vec<crate::wire::JailHealth>, Sender<Result<(), UnknownNode>>),
    List(Sender<Vec<NodeStatus>>),
    Resolve(String, Sender<Result<String, ResolveError>>),
    ResolveOrSchedule(String, Sender<Result<(String, String), ScheduleOrResolveError>>),
    ResolvePlacement(String, Sender<Result<(String, String), PlacementError>>),
    RecordPlacement(String, String, Sender<()>),
    RemovePlacement(String, Sender<()>),
    OwnerOf(String, Sender<Option<Owner>>),
    ApplyService(String, u32, keel_spec::JailTemplate, Sender<Result<(), services::ApplyServiceError>>),
}

pub fn spawn(
    mut registry: Registry,
    mut placements: Placements,
    mut services: Services,
    mut used_addresses: UsedAddresses,
) -> (JoinHandle<()>, Sender<Command>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let handle = thread::spawn(move || {
        for command in rx {
            handle_command(&mut registry, &mut placements, &mut services, &mut used_addresses, command);
        }
    });
    (handle, tx)
}

fn handle_command(
    registry: &mut Registry,
    placements: &mut Placements,
    services: &mut Services,
    used_addresses: &mut UsedAddresses,
    command: Command,
) {
    match command {
        Command::Register(id, addr, capacity_cpu, capacity_memory, reply) => {
            let result = registry.register(id, addr, capacity_cpu, capacity_memory, Instant::now());
            let _ = reply.send(result);
        }
        Command::Heartbeat(id, committed_cpu, committed_memory, jails, reply) => {
            let result = registry.heartbeat(&id, committed_cpu, committed_memory, jails, Instant::now());
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
                let nodes: Vec<scheduler::NodeResources> = registry
                    .list(now)
                    .into_iter()
                    .filter(|status| status.status == NodeState::Alive)
                    .map(|status| scheduler::NodeResources {
                        id: status.id,
                        capacity_cpu: status.capacity_cpu,
                        capacity_memory: status.capacity_memory,
                        committed_cpu: status.committed_cpu,
                        committed_memory: status.committed_memory,
                    })
                    .collect();
                scheduler::pick_node(&nodes).map_err(ScheduleOrResolveError::from).and_then(
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
        Command::OwnerOf(name, reply) => {
            let _ = reply.send(services::owner_of(&name, placements, services));
        }
        Command::ApplyService(name, replicas, template, reply) => {
            let result = (|| {
                for i in 0..replicas {
                    let candidate = services::replica_name(&name, i);
                    if let Some(owner) = services::owner_of(&candidate, placements, services) {
                        let is_self = matches!(&owner, Owner::Service(other) if other == &name);
                        if !is_self {
                            return Err(services::ApplyServiceError::NameConflict { name: candidate, owner });
                        }
                    }
                }
                services.apply(name, replicas, template)
            })();
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addresses::UsedAddresses;
    use crate::services::{ApplyServiceError, Owner, Services};
    use keel_spec::{JailTemplate, ResourcesSpec, RestartPolicy, TemplateNetworkSpec};

    fn test_cluster_cidr() -> ipnet::Ipv4Net {
        "10.0.0.0/16".parse().unwrap()
    }

    fn template() -> JailTemplate {
        JailTemplate {
            image: "base/14.2-web".to_string(),
            command: vec!["/usr/local/bin/myapp".to_string()],
            network: TemplateNetworkSpec { vnet: true, bridge: "keel0".to_string() },
            resources: ResourcesSpec { cpu: "1".to_string(), memory: "256M".to_string() },
            restart_policy: RestartPolicy::Always,
        }
    }

    #[test]
    fn register_command_makes_the_node_visible_in_list() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(
                "node-1".to_string(),
                "10.0.0.1".to_string(),
                4.0,
                8 * 1024 * 1024 * 1024,
                reg_tx,
            ))
            .unwrap();
        reg_rx.recv().unwrap().unwrap();

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        let statuses = list_rx.recv().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "node-1");
    }

    #[test]
    fn heartbeat_command_on_unknown_id_returns_an_error() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("missing".to_string(), 0.0, 0, vec![], hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_err());
    }

    #[test]
    fn heartbeat_command_on_a_registered_node_succeeds() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(
                "node-1".to_string(),
                "10.0.0.1".to_string(),
                4.0,
                8 * 1024 * 1024 * 1024,
                reg_tx,
            ))
            .unwrap();
        reg_rx.recv().unwrap().unwrap();

        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat("node-1".to_string(), 0.0, 0, vec![], hb_tx)).unwrap();
        assert!(hb_rx.recv().unwrap().is_ok());
    }

    #[test]
    fn list_command_on_a_fresh_worker_is_empty() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (list_tx, list_rx) = mpsc::channel();
        commands.send(Command::List(list_tx)).unwrap();
        assert_eq!(list_rx.recv().unwrap(), vec![]);
    }

    #[test]
    fn resolve_command_on_a_registered_alive_node_returns_its_address() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(
                "node-1".to_string(),
                "10.0.0.1".to_string(),
                4.0,
                8 * 1024 * 1024 * 1024,
                reg_tx,
            ))
            .unwrap();
        reg_rx.recv().unwrap().unwrap();

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("node-1".to_string(), resolve_tx)).unwrap();
        assert_eq!(resolve_rx.recv().unwrap(), Ok("10.0.0.1".to_string()));
    }

    #[test]
    fn resolve_command_on_an_unknown_node_returns_an_error() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (resolve_tx, resolve_rx) = mpsc::channel();
        commands.send(Command::Resolve("missing".to_string(), resolve_tx)).unwrap();
        assert!(resolve_rx.recv().unwrap().is_err());
    }

    fn register_node(commands: &Sender<Command>, id: &str, addr: &str, capacity_cpu: f64, capacity_memory: u64) {
        let (reg_tx, reg_rx) = mpsc::channel();
        commands
            .send(Command::Register(id.to_string(), addr.to_string(), capacity_cpu, capacity_memory, reg_tx))
            .unwrap();
        reg_rx.recv().unwrap().unwrap();
    }

    fn heartbeat_node(commands: &Sender<Command>, id: &str, committed_cpu: f64, committed_memory: u64) {
        let (hb_tx, hb_rx) = mpsc::channel();
        commands.send(Command::Heartbeat(id.to_string(), committed_cpu, committed_memory, vec![], hb_tx)).unwrap();
        hb_rx.recv().unwrap().unwrap();
    }

    #[test]
    fn resolve_or_schedule_on_a_fresh_jail_name_with_equal_headroom_breaks_ties_by_ascending_id() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-1".to_string(), "10.0.0.1".to_string())));
    }

    #[test]
    fn resolve_or_schedule_on_a_fresh_jail_name_schedules_onto_the_node_with_more_headroom() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 100);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 100);
        heartbeat_node(&commands, "node-1", 3.0, 10); // 25% cpu headroom
        heartbeat_node(&commands, "node-2", 1.0, 10); // 75% cpu headroom

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-2".to_string(), "10.0.0.2".to_string())));
    }

    #[test]
    fn resolve_or_schedule_on_an_already_placed_jail_is_sticky() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);
        register_node(&commands, "node-2", "10.0.0.2", 4.0, 8 * 1024 * 1024 * 1024);

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-1".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(("node-1".to_string(), "10.0.0.1".to_string())));
    }

    #[test]
    fn resolve_or_schedule_with_no_alive_nodes_returns_no_available_nodes() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolveOrSchedule("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(ScheduleOrResolveError::Schedule(ScheduleError::NoAvailableNodes)));
    }

    #[test]
    fn resolve_placement_on_an_unplaced_jail_returns_not_placed() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ResolvePlacement("web-1".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Err(PlacementError::NotPlaced("web-1".to_string())));
    }

    #[test]
    fn record_then_remove_placement_is_reflected_by_resolve_placement() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;
        register_node(&commands, "node-1", "10.0.0.1", 4.0, 8 * 1024 * 1024 * 1024);

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

    #[test]
    fn apply_service_command_creates_a_new_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, template(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Ok(()));
    }

    #[test]
    fn apply_service_command_rejects_a_template_change_on_an_existing_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx1, rx1) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, template(), tx1)).unwrap();
        rx1.recv().unwrap().unwrap();

        let mut changed = template();
        changed.image = "base/different-image".to_string();
        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, changed, tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Err(ApplyServiceError::TemplateChanged("web".to_string())));
    }

    #[test]
    fn apply_service_command_rejects_a_name_already_used_by_an_unmanaged_jail() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), tx)).unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            Err(ApplyServiceError::NameConflict { name: "web-0".to_string(), owner: Owner::Unmanaged })
        );
    }

    #[test]
    fn apply_service_command_reapplying_the_same_service_with_more_replicas_does_not_conflict_with_itself() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx1, rx1) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), tx1)).unwrap();
        rx1.recv().unwrap().unwrap();

        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx2, rx2) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 3, template(), tx2)).unwrap();
        assert_eq!(rx2.recv().unwrap(), Ok(()));
    }

    #[test]
    fn owner_of_command_on_an_unplaced_name_is_none() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (tx, rx) = mpsc::channel();
        commands.send(Command::OwnerOf("web-0".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), None);
    }

    #[test]
    fn owner_of_command_on_a_service_replica_names_that_service() {
        let commands = spawn(Registry::new(test_cluster_cidr()), Placements::new(), Services::new(), UsedAddresses::new()).1;

        let (apply_tx, apply_rx) = mpsc::channel();
        commands.send(Command::ApplyService("web".to_string(), 1, template(), apply_tx)).unwrap();
        apply_rx.recv().unwrap().unwrap();
        let (rec_tx, rec_rx) = mpsc::channel();
        commands.send(Command::RecordPlacement("web-0".to_string(), "node-1".to_string(), rec_tx)).unwrap();
        rec_rx.recv().unwrap();

        let (tx, rx) = mpsc::channel();
        commands.send(Command::OwnerOf("web-0".to_string(), tx)).unwrap();
        assert_eq!(rx.recv().unwrap(), Some(Owner::Service("web".to_string())));
    }
}
