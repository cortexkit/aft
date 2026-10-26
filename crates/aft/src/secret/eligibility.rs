//! Which stamped call may ask the vault for a label, and under which two
//! scopes.
//!
//! A call is eligible in exactly three forms, and every other call is
//! ineligible and never reaches `resolve`:
//!
//! 1. **Direct Head**: a Classic call whose route carries the head agent's own
//! stamp. Authorization and lifetime scopes are both that stamp.
//! 2. **Direct Worker**: the same for a worker's stamp. Both scopes are the
//!    worker's own `(owner, Worker, ref, epoch)`, so a worker's grant never
//!    covers its head or a sibling.
//! 3. **Bound codemode**: a Codemode call that `codemode::admit`/`resolve_owner`
//! already bound to a live head route. The lifetime scope is the run's
//! Ephemeral stamp, and the authorization scope is the head route's, so the
//! program cannot outlive its own lifetime.
//!
//! Ineligible: no stamp, a standalone call, a plugin-only route, another
//! owner, a flow-read scope, and any Ephemeral stamp on a non-codemode identity
//! (parented or not).
//!
//! Both scopes here are rebuilt from the daemon's stamp on every call. Nothing
//! is read from tool arguments or from a session id. A background command
//! resolves its scopes once at launch and keeps them, which is why
//! [`CallScopes`] owns plain data instead of borrowing the route.

use crate::secret::SecretScope;
use subc_protocol::scope::ScopeKind;

/// The two scopes one eligible call asks the vault under.
#[derive(Clone, PartialEq, Eq)]
pub struct CallScopes {
    /// The authorization scope: passed to `resolve` and `material`, and the
    /// identity a consent ask and an audit record are filed under.
    pub authorization: SecretScope,
    /// The lifetime scope: the call's own stamp. Its end is what stops a proxy
    /// and its matchers.
    pub lifetime: SecretScope,
}

impl std::fmt::Debug for CallScopes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallScopes")
            .field("authorization", &self.authorization)
            .field("lifetime", &self.lifetime)
            .finish()
    }
}

/// Which of the three eligible forms a call took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EligibilityForm {
    /// Form 1: a head agent's own Classic call.
    DirectHead,
    /// Form 2: a worker agent's own Classic call.
    DirectWorker,
    /// Form 3: a codemode run bound to a live head route.
    BoundCodemode,
}

/// Why a call may not ask the vault. Every variant maps to
/// `secret_refused`, and to no `resolve` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligibility {
    /// The route carries no daemon stamp.
    NoScope,
    /// The route bound for one session only (standalone, not subc).
    Standalone,
    /// The route serves a plugin's own calls, which have no Alfonso stamp.
    PluginOnlyRoute,
    /// The stamp's owner is not prefrontal-core.
    OtherOwner,
    /// The call reads a scheduled flow, which is not a command lifetime.
    FlowRead,
    /// The stamp is an Ephemeral one outside a codemode run.
    EphemeralOutsideCodemode,
    /// An Ephemeral run stamp is parented, which marks a worker's run.
    WorkerRunSession,
    /// The stamp is not a kind an Alfonso command runs under.
    NotACommandScope,
    /// A codemode call the daemon did not bind to a live head route.
    RunNotBound,
    /// The bound head route's stamp is not a prefrontal-core head.
    HeadScopeNotAlfonso,
}

impl Ineligibility {
    /// The refusal code a launch ends with for this reason.
    pub fn code(&self) -> &'static str {
        "secret_refused"
    }
}

/// Whether `stamp` is a prefrontal-core stamp of one of `kinds`.
///
/// This is `access_grants::alfonso`, the daemon-facing predicate the rest of the
/// route policy uses, narrowed to the kinds a command runs under. It is
/// repeated here because that function is private to its module, and the
/// eligibility answer is the vault seam's own.
pub fn alfonso_of_kind(stamp: &subc_protocol::scope::ScopeStamp, kinds: &[ScopeKind]) -> bool {
    stamp.owner_authorized
        && matches!(&stamp.owner, subc_protocol::Principal::Reserved { module_id } if module_id == "prefrontal-core")
        && kinds.contains(&stamp.kind)
}

/// The kinds a stamped command runs under; an Ephemeral stamp only qualifies
/// inside a bound codemode run.
pub const COMMAND_SCOPE_KINDS: [ScopeKind; 3] =
    [ScopeKind::Head, ScopeKind::Worker, ScopeKind::Ephemeral];
