pub mod error;
pub mod fake;
pub mod process;

pub use error::NetError;
pub use fake::FakeNetManager;
pub use process::ProcessNetManager;

pub trait NetManager {
    /// Idempotent: creates the bridge if it doesn't already exist and
    /// brings it up. Never destroys a bridge (there is no corresponding
    /// `destroy_bridge` method), since other jails or host config may
    /// depend on it.
    fn ensure_bridge_exists(&self, bridge: &str) -> Result<(), NetError>;

    /// Requires the bridge to already exist (call `ensure_bridge_exists`
    /// first), fails otherwise. Wires the jail's networking as one
    /// coherent operation (epair creation, bridge attachment, VNET
    /// migration, address configuration).
    ///
    /// Tolerates being retried after a partial failure (e.g. the epair
    /// pair already existing from an interrupted prior attempt), but this
    /// method is not atomic: a failed call may still have created some
    /// resources (epair, bridge membership) that a subsequent
    /// `detach_jail` call would need to clean up.
    fn attach_jail(&self, jail_name: &str, bridge: &str, epair_base: &str, address: &str) -> Result<(), NetError>;

    /// Tears down the epair pair; treats an already-absent pair as success
    /// rather than an error (idempotent), mirroring `keel-jail`'s
    /// `remove_resource_limits`.
    ///
    /// Safe to call while the jail is still running, and should be called
    /// before jail destroy, not after: jail destroy does not clean up an
    /// epair moved into it, it orphans it back to the host.
    fn detach_jail(&self, epair_base: &str) -> Result<(), NetError>;
}
