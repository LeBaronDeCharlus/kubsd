use ipnet::Ipv4Net;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct PodCidrSlot(Arc<Mutex<Option<Ipv4Net>>>);

impl PodCidrSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, pod_cidr: Ipv4Net) {
        *self.0.lock().unwrap() = Some(pod_cidr);
    }

    pub fn get(&self) -> Option<Ipv4Net> {
        *self.0.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_slot_is_empty() {
        assert_eq!(PodCidrSlot::new().get(), None);
    }

    #[test]
    fn set_then_get_returns_the_value() {
        let slot = PodCidrSlot::new();
        slot.set("10.0.4.0/24".parse().unwrap());
        assert_eq!(slot.get(), Some("10.0.4.0/24".parse().unwrap()));
    }

    #[test]
    fn clones_share_the_same_underlying_slot() {
        let slot = PodCidrSlot::new();
        let clone = slot.clone();
        clone.set("10.0.5.0/24".parse().unwrap());
        assert_eq!(slot.get(), Some("10.0.5.0/24".parse().unwrap()));
    }
}
