use crate::NetError;
use crate::NetManager;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeNetManager {
    bridges: Mutex<HashSet<String>>,
    attachments: Mutex<HashMap<String, (String, String, String)>>,
    routes: Mutex<HashMap<String, String>>,
}

impl FakeNetManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_route(&self, subnet: &str) -> Option<String> {
        self.routes.lock().unwrap().get(subnet).cloned()
    }
}

impl NetManager for FakeNetManager {
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError> {
        self.bridges.lock().unwrap().insert(bridge.to_string());
        Ok(())
    }

    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError> {
        if !self.bridges.lock().unwrap().contains(bridge) {
            return Err(NetError::NotFound(bridge.to_string()));
        }
        self.attachments.lock().unwrap().insert(
            epair_base.to_string(),
            (jail_name.to_string(), bridge.to_string(), address.to_string()),
        );
        Ok(())
    }

    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError> {
        self.attachments.lock().unwrap().remove(epair_base);
        Ok(())
    }

    fn add_route(&self, subnet: &str, gateway_addr: &str) -> Result<(), NetError> {
        self.routes.lock().unwrap().insert(subnet.to_string(), gateway_addr.to_string());
        Ok(())
    }

    fn remove_route(&self, subnet: &str) -> Result<(), NetError> {
        self.routes.lock().unwrap().remove(subnet);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_bridge_exists_is_idempotent() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.ensure_bridge_exists("keel0").unwrap();
    }

    #[test]
    fn attach_jail_requires_bridge_to_exist_first() {
        let net = FakeNetManager::new();
        assert!(matches!(
            net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24"),
            Err(NetError::NotFound(_))
        ));
    }

    #[test]
    fn attach_jail_succeeds_after_ensure_bridge_exists() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
    }

    #[test]
    fn detach_jail_on_unknown_epair_is_a_no_op_success() {
        let net = FakeNetManager::new();
        net.detach_jail("epair-never-attached").unwrap();
    }

    #[test]
    fn detach_then_reattach_works() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
        net.detach_jail("epair7").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
    }

    #[test]
    fn attach_jail_is_idempotent_when_called_twice_without_detaching() {
        let net = FakeNetManager::new();
        net.ensure_bridge_exists("keel0").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
        net.attach_jail("web-1", "keel0", "epair7", "10.0.0.5/24").unwrap();
    }

    #[test]
    fn add_route_then_has_route_reflects_it() {
        let net = FakeNetManager::new();
        assert_eq!(net.has_route("10.0.4.0/24"), None);
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        assert_eq!(net.has_route("10.0.4.0/24"), Some("192.168.64.5".to_string()));
    }

    #[test]
    fn add_route_is_idempotent() {
        let net = FakeNetManager::new();
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        assert_eq!(net.has_route("10.0.4.0/24"), Some("192.168.64.5".to_string()));
    }

    #[test]
    fn remove_route_on_a_never_added_subnet_is_a_no_op_success() {
        let net = FakeNetManager::new();
        net.remove_route("10.0.9.0/24").unwrap();
    }

    #[test]
    fn add_then_remove_route_clears_it() {
        let net = FakeNetManager::new();
        net.add_route("10.0.4.0/24", "192.168.64.5").unwrap();
        net.remove_route("10.0.4.0/24").unwrap();
        assert_eq!(net.has_route("10.0.4.0/24"), None);
    }
}
