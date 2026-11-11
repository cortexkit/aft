//! One daemon-stamped scope, as the vault and the audit record name it.
//!
//! A [`SecretScope`] is stamped by the daemon onto a route. It is never built
//! from a tool argument or a session id, and a background command keeps the one
//! it resolved at launch for its whole life. The stamp itself travels to the
//! vault unchanged: `secret.use` trusts the sender to relay it, and takes the
//! asking scope from nowhere else.

use std::sync::Arc;

use secret_rules::audit::ScopeRef;
use subc_protocol::scope::ScopeStamp;

use super::wire::Coverage;

/// The `(owner, kind, ref)` identity of one stamped scope, plus the stamp the
/// vault is given.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretScope {
    stamp: Arc<ScopeStamp>,
    owner: String,
    kind: String,
    reference: String,
}

impl SecretScope {
    /// Read a scope off the stamp the daemon put on a route.
    pub fn from_stamp(stamp: &ScopeStamp) -> Self {
        Self {
            stamp: Arc::new(stamp.clone()),
            // The contract renders an owner as `reserved:<module_id>`, `direct`
            // or `unverified`; that spelling is what a consent or audit reader
            // compares, and it is also what the statements use.
            owner: render_owner(&stamp.owner),
            kind: match stamp.kind {
                subc_protocol::scope::ScopeKind::Head => "head",
                subc_protocol::scope::ScopeKind::Worker => "worker",
                subc_protocol::scope::ScopeKind::Ephemeral => "ephemeral",
            }
            .to_string(),
            reference: stamp.scope_ref.clone(),
        }
    }

    /// The daemon's stamp, relayed to the vault unchanged.
    pub fn stamp(&self) -> &ScopeStamp {
        &self.stamp
    }

    /// The scope's owner, rendered as the contract spells it.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The scope's kind: `head`, `worker` or `ephemeral`.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The scope's own reference, stable across a new epoch of the same work.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// The authorization scope of an audit record.
    pub fn to_scope_ref(&self) -> ScopeRef {
        ScopeRef {
            owner: self.owner.clone(),
            kind: self.kind.clone(),
            reference: self.reference.clone(),
        }
    }

    /// The coverage a call asks the vault for. A head or worker call asks for
    /// its own scope; a bound codemode run's authorization scope is its head's,
    /// so it asks as that head.
    pub fn coverage(&self) -> Coverage {
        Coverage::Scope
    }
}

/// The contract's owner rendering: `reserved:<module_id>`, `direct` or
/// `unverified`.
fn render_owner(owner: &subc_protocol::Principal) -> String {
    match owner {
        subc_protocol::Principal::Reserved { module_id } => format!("reserved:{module_id}"),
        subc_protocol::Principal::Direct => "direct".to_string(),
        subc_protocol::Principal::Unverified => "unverified".to_string(),
    }
}

impl std::hash::Hash for SecretScope {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // The identity fields are what make two scopes the same, so the relayed
        // stamp takes no part in hashing and equality stays cheap.
        self.owner.hash(state);
        self.kind.hash(state);
        self.reference.hash(state);
    }
}

impl std::fmt::Debug for SecretScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretScope")
            .field("owner", &self.owner)
            .field("kind", &self.kind)
            .field("ref", &self.reference)
            .finish()
    }
}

impl std::fmt::Display for SecretScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.owner, self.kind, self.reference)
    }
}
