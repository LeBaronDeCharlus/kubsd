use crate::placements::Placements;
use keel_spec::JailTemplate;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceRecord {
    pub desired_replicas: u32,
    pub template: JailTemplate,
}

#[derive(Debug, Default)]
pub struct Services {
    by_name: HashMap<String, ServiceRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Unmanaged,
    Service(String),
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Owner::Unmanaged => write!(f, "an unmanaged jail"),
            Owner::Service(name) => write!(f, "service '{name}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyServiceError {
    #[error("name '{name}' is already in use by {owner}")]
    NameConflict { name: String, owner: Owner },
    #[error("service '{0}' template is immutable once created; delete and re-apply instead")]
    TemplateChanged(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown service '{0}'")]
pub struct UnknownService(pub String);

impl Services {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&ServiceRecord> {
        self.by_name.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(|s| s.as_str())
    }

    pub fn list(&self) -> Vec<(&str, &ServiceRecord)> {
        let mut entries: Vec<(&str, &ServiceRecord)> =
            self.by_name.iter().map(|(k, v)| (k.as_str(), v)).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        entries
    }

    /// Creates the service if `name` is new, or updates `desired_replicas`
    /// if it already exists and `template` is unchanged. Rejects a template
    /// change on an existing service (only `replicas` may change).
    pub fn apply(&mut self, name: String, desired_replicas: u32, template: JailTemplate) -> Result<(), ApplyServiceError> {
        if let Some(existing) = self.by_name.get(&name) {
            if existing.template != template {
                return Err(ApplyServiceError::TemplateChanged(name));
            }
        }
        self.by_name.insert(name, ServiceRecord { desired_replicas, template });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Option<ServiceRecord> {
        self.by_name.remove(name)
    }
}

/// `"<service_name>-<index>"`.
pub fn replica_name(service_name: &str, index: u32) -> String {
    format!("{service_name}-{index}")
}

/// If `jail_name` is `"<service_name>-<index>"` for a plain non-negative
/// integer index, returns that index.
pub fn replica_index(service_name: &str, jail_name: &str) -> Option<u32> {
    jail_name.strip_prefix(service_name)?.strip_prefix('-')?.parse::<u32>().ok()
}

/// Returns the current owner of a name already present in `placements`, or
/// `None` if it has no existing placement at all. A name belongs to a
/// service if it matches that service's deterministic replica pattern;
/// otherwise, if it's placed at all, it's an unmanaged plain `kind: Jail`.
pub fn owner_of(name: &str, placements: &Placements, services: &Services) -> Option<Owner> {
    if placements.get(name).is_none() {
        return None;
    }
    for service_name in services.names() {
        if replica_index(service_name, name).is_some() {
            return Some(Owner::Service(service_name.to_string()));
        }
    }
    Some(Owner::Unmanaged)
}

/// Given a service's desired replica count and the set of indices currently
/// healthy (Alive node + running jail, per the caller), returns the indices
/// to schedule (lowest missing first) and the indices to tear down (highest
/// healthy first), matching `diff_replicas(3, {}) == ([0,1,2], [])` and
/// `diff_replicas(1, {0,1,2}) == ([], [2,1])`.
pub fn diff_replicas(desired: u32, healthy_indices: &BTreeSet<u32>) -> (Vec<u32>, Vec<u32>) {
    let healthy_count = healthy_indices.len() as u32;
    if healthy_count < desired {
        let missing = desired - healthy_count;
        let to_add = (0u32..).filter(|i| !healthy_indices.contains(i)).take(missing as usize).collect();
        (to_add, Vec::new())
    } else if healthy_count > desired {
        let excess = healthy_count - desired;
        let to_remove = healthy_indices.iter().rev().take(excess as usize).copied().collect();
        (Vec::new(), to_remove)
    } else {
        (Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_spec::{ResourcesSpec, RestartPolicy, TemplateNetworkSpec};

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
    fn apply_creates_a_new_service() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 3);
    }

    #[test]
    fn apply_again_with_the_same_template_scales_up_or_down() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        services.apply("web".to_string(), 5, template()).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 5);
        services.apply("web".to_string(), 0, template()).unwrap();
        assert_eq!(services.get("web").unwrap().desired_replicas, 0);
    }

    #[test]
    fn apply_with_a_changed_template_is_rejected() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        let mut changed = template();
        changed.image = "base/different-image".to_string();
        assert_eq!(
            services.apply("web".to_string(), 3, changed),
            Err(ApplyServiceError::TemplateChanged("web".to_string()))
        );
    }

    #[test]
    fn remove_deletes_the_service() {
        let mut services = Services::new();
        services.apply("web".to_string(), 3, template()).unwrap();
        assert!(services.remove("web").is_some());
        assert!(services.get("web").is_none());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let mut services = Services::new();
        services.apply("web".to_string(), 1, template()).unwrap();
        services.apply("api".to_string(), 1, template()).unwrap();
        let names: Vec<&str> = services.list().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["api", "web"]);
    }

    #[test]
    fn replica_name_and_index_round_trip() {
        assert_eq!(replica_name("web", 2), "web-2");
        assert_eq!(replica_index("web", "web-2"), Some(2));
        assert_eq!(replica_index("web", "other-2"), None);
        assert_eq!(replica_index("web", "web-not-a-number"), None);
    }

    #[test]
    fn owner_of_an_unplaced_name_is_none() {
        let placements = Placements::new();
        let services = Services::new();
        assert_eq!(owner_of("web-0", &placements, &services), None);
    }

    #[test]
    fn owner_of_a_placed_name_matching_a_known_service_is_that_service() {
        let mut placements = Placements::new();
        placements.set("web-0".to_string(), "node-1".to_string());
        let mut services = Services::new();
        services.apply("web".to_string(), 1, template()).unwrap();
        assert_eq!(owner_of("web-0", &placements, &services), Some(Owner::Service("web".to_string())));
    }

    #[test]
    fn owner_of_a_placed_name_matching_no_service_is_unmanaged() {
        let mut placements = Placements::new();
        placements.set("web-1".to_string(), "node-1".to_string());
        let services = Services::new();
        assert_eq!(owner_of("web-1", &placements, &services), Some(Owner::Unmanaged));
    }

    #[test]
    fn diff_replicas_with_nothing_placed_adds_from_zero() {
        assert_eq!(diff_replicas(3, &BTreeSet::new()), (vec![0, 1, 2], vec![]));
    }

    #[test]
    fn diff_replicas_reuses_only_the_missing_index() {
        let healthy = BTreeSet::from([0, 1]);
        assert_eq!(diff_replicas(3, &healthy), (vec![2], vec![]));
    }

    #[test]
    fn diff_replicas_skips_an_index_that_is_unhealthy_and_reschedules_it() {
        let healthy = BTreeSet::from([1]);
        assert_eq!(diff_replicas(2, &healthy), (vec![0], vec![]));
    }

    #[test]
    fn diff_replicas_scales_down_from_the_highest_index() {
        let healthy = BTreeSet::from([0, 1, 2]);
        assert_eq!(diff_replicas(1, &healthy), (vec![], vec![2, 1]));
    }

    #[test]
    fn diff_replicas_scaled_to_zero_tears_down_everything() {
        let healthy = BTreeSet::from([0, 1, 2]);
        assert_eq!(diff_replicas(0, &healthy), (vec![], vec![2, 1, 0]));
    }

    #[test]
    fn diff_replicas_at_the_desired_count_does_nothing() {
        let healthy = BTreeSet::from([0, 1]);
        assert_eq!(diff_replicas(2, &healthy), (vec![], vec![]));
    }
}
