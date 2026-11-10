use super::CallGraphStoreError;

/// Result of requesting the persisted callgraph store for a store-backed op.
///
/// Cold builds run in the background. Checkout-view acquisitions also run
/// off the request thread, with a bounded wait before returning `Building`
/// so the agent can retry while materialization continues.
pub enum CallgraphStoreAccess {
    /// A build or checkout-view acquisition is in flight; retry shortly.
    Building,
    /// The durable build-death breaker refuses this domain until an explicit reset
    /// or a time-to-live check confirms the suspension has expired.
    Suspended(crate::build_breaker::BuildSuspension),
    /// Resolved configuration turns the callgraph index off. No build is
    /// started for a disabled index.
    Off,
    /// Not configured, or a read-only worktree whose store was never built.
    Unavailable,
    /// A store open/build check failed with a real error (DB/IO).
    Error(CallGraphStoreError),
}
