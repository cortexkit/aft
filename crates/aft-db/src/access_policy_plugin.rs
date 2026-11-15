//! Short-lived, route-bound capabilities for standalone host permission loops.

use super::{AccessGrant, AccessOperation};
use std::any::Any;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(crate) struct Scope {
    route: Weak<dyn Any + Send + Sync>,
    session: String,
    tool: String,
    /// When the earliest capability claimed for this dispatch was first
    /// approved. Renewed continuations never outlive [`MAX_CHAIN`] from it.
    first_approved: Option<Instant>,
    /// Capabilities this dispatch presented whose tokens had expired. A new
    /// ask for one of them says so, so the host knows the user answered too
    /// late rather than the approval being refused.
    expired: Vec<AccessGrant>,
    /// Grants presented on a different route with the same session and tool.
    /// These are diagnostic only: a replacement route needs a fresh approval.
    replaced: Vec<AccessGrant>,
    presented_outside_token: bool,
}

impl Scope {
    pub(crate) fn session(&self) -> &str {
        &self.session
    }
    pub(crate) fn for_tool(mut self, tool: &str, arguments: &serde_json::Value) -> Self {
        self.tool = tool_family(tool, arguments);
        self
    }
    pub(crate) fn new<T: Any + Send + Sync>(route: &Arc<T>, session: &str) -> Self {
        let route: Arc<dyn Any + Send + Sync> = route.clone();
        Self {
            route: Arc::downgrade(&route),
            session: session.into(),
            tool: String::new(),
            first_approved: None,
            expired: Vec::new(),
            replaced: Vec::new(),
            presented_outside_token: false,
        }
    }

    /// Whether this dispatch presented an expired token for `grant`'s
    /// operation on the same path.
    pub(crate) fn presented_expired(&self, grant: &AccessGrant) -> bool {
        self.expired
            .iter()
            .any(|expired| expired.operation == grant.operation && expired.path == grant.path)
    }

    pub(crate) fn retry_refusal_reason(
        &self,
        grant: &AccessGrant,
        claimed_any: bool,
    ) -> Option<&'static str> {
        if self.presented_expired(grant) {
            Some("approval_expired")
        } else if self
            .replaced
            .iter()
            .any(|replaced| replaced.covers_kind(grant.operation, &grant.path, grant.kind))
        {
            Some("approval_route_replaced")
        } else if self.presented_outside_token {
            Some(if claimed_any {
                "approval_not_covered"
            } else {
                "approval_not_recognized"
            })
        } else {
            None
        }
    }

    pub(crate) fn log_retry_refusal(&self, operation: AccessOperation, path: &Path, reason: &str) {
        if !self.presented_outside_token {
            return;
        }
        let operation = match operation {
            AccessOperation::Read => "read",
            AccessOperation::Write => "write",
        };
        let path_hash = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
        crate::slog_info!("outside-access retry refused reason={reason} operation={operation} path_hash={path_hash}");
    }
}

impl PartialEq for Scope {
    fn eq(&self, other: &Self) -> bool {
        self.route.ptr_eq(&other.route) && self.session == other.session && self.tool == other.tool
    }
}

/// Only the preflight and apply names of one logical operation share authority.
pub(crate) fn tool_family(tool: &str, arguments: &serde_json::Value) -> String {
    match tool {
        "hashline_preflight" | "hashline_edit" | "edit" | "edit_symbol" => "edit".into(),
        "undo_preview" | "undo" => "safety:undo".into(),
        "checkpoint_paths" | "restore_checkpoint" => "safety:restore".into(),
        "checkpoint" => "safety:checkpoint".into(),
        "safety" => format!(
            "safety:{}",
            arguments
                .get("op")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        ),
        "read_file" => "read".into(),
        "write_file" => "write".into(),
        "delete_file" => "delete".into(),
        other => other.to_owned(),
    }
}
impl Eq for Scope {}

struct Issued {
    scope: Scope,
    grant: AccessGrant,
    expires: Instant,
    /// The first approval this token continues, or `None` for a fresh ask
    /// whose approval happens when it is first claimed.
    origin: Option<Instant>,
}

static TOKENS: LazyLock<Mutex<HashMap<String, Issued>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const TTL: Duration = Duration::from_secs(5 * 60);
/// How long an expired token is still recognised as expired (and not as an
/// unknown token) when it is presented. It never authorizes anything.
const EXPIRED_GRACE: Duration = Duration::from_secs(60 * 60);

fn retained(token: &Issued, now: Instant) -> bool {
    // Keep dead-route records for the same bounded diagnostic window as
    // expired tokens. Their weak route identity can never authorize a new route.
    token.expires + EXPIRED_GRACE > now
}
/// How long one approval can be carried forward by preview-to-apply
/// continuations. Past this, the host must ask the user again.
pub(crate) const MAX_CHAIN: Duration = Duration::from_secs(30 * 60);

pub(crate) fn issue(scope: &Scope, grant: &AccessGrant) -> String {
    issue_until(scope, grant, Instant::now() + TTL, None)
}

fn issue_until(
    scope: &Scope,
    grant: &AccessGrant,
    expires: Instant,
    origin: Option<Instant>,
) -> String {
    let mut tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    tokens.retain(|_, token| retained(token, now));
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).expect("operating system random source for plugin approval tokens");
    let id = format!(
        "outside:{}",
        nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    tokens.insert(
        id.clone(),
        Issued {
            scope: scope.clone(),
            grant: grant.clone(),
            expires,
            origin,
        },
    );
    id
}

/// Claim at admission, once per dispatch. Repeated path checks within that
/// dispatch share the claimed capability, but replaying the token cannot.
/// The scope records the earliest first-approval time among the claimed
/// tokens, which bounds any continuation issued for this dispatch. An
/// expired token authorizes nothing; it is consumed and its capability is
/// remembered on the scope so the next ask for it can say it expired.
pub(crate) fn claim(scope: &mut Scope, ids: &[String]) -> Vec<AccessGrant> {
    let mut tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    tokens.retain(|_, token| retained(token, now));
    let mut grants = Vec::new();
    scope.presented_outside_token |= ids.iter().any(|id| id.starts_with("outside:"));
    for id in ids {
        if let Some(token) = tokens.get(id) {
            if token.scope.session == scope.session
                && token.scope.tool == scope.tool
                && !token.scope.route.ptr_eq(&scope.route)
            {
                // Do not consume another route's token or add its authority.
                // The marker only lets the host offer a new approval prompt.
                scope.replaced.push(token.grant.clone());
            }
        }
        if tokens.get(id).is_some_and(|token| token.scope == *scope) {
            if let Some(token) = tokens.remove(id) {
                if token.expires <= now {
                    scope.expired.push(token.grant);
                    continue;
                }
                let origin = token.origin.unwrap_or(now);
                scope.first_approved = Some(
                    scope
                        .first_approved
                        .map_or(origin, |earlier| earlier.min(origin)),
                );
                grants.push(token.grant);
            }
        }
    }
    grants
}

/// Re-issue claimed capabilities for the apply half of a preview. Each
/// renewal gets a fresh short TTL, but never past [`MAX_CHAIN`] from the
/// first approval; once that passes nothing is renewed and the host asks again.
pub(crate) fn continuation(scope: &Scope, grants: &[AccessGrant]) -> Vec<String> {
    continuation_at(scope, grants, Instant::now())
}

fn continuation_at(scope: &Scope, grants: &[AccessGrant], now: Instant) -> Vec<String> {
    let origin = scope.first_approved.unwrap_or(now);
    let deadline = origin + MAX_CHAIN;
    if now >= deadline {
        return Vec::new();
    }
    let expires = (now + TTL).min(deadline);
    grants
        .iter()
        .map(|grant| issue_until(scope, grant, expires, Some(origin)))
        .collect()
}

/// Make an issued token expire now, to test what a late approval does.
#[cfg(test)]
pub(crate) fn expire_for_test(id: &str) {
    if let Some(token) = TOKENS.lock().unwrap().get_mut(id) {
        token.expires = Instant::now() - Duration::from_secs(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_policy::{AccessOperation, GrantKind};

    /// An expired token authorizes nothing and is used up, but the dispatch
    /// that presented it knows it expired. An unknown or foreign token says
    /// nothing about expiry.
    #[test]
    fn plugin_expired_tokens_are_remembered_not_honoured() {
        let owner = Arc::new(());
        let fresh = || Scope::new(&owner, "session").for_tool("read", &serde_json::Value::Null);
        let grant = AccessGrant {
            path: "/outside/late".into(),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let token = issue(&fresh(), &grant);
        expire_for_test(&token);
        // Other dispatches issue and claim meanwhile; the expired token stays
        // recognisable within the grace period.
        let _ = issue(&fresh(), &grant);
        let mut late = fresh();
        assert!(claim(&mut late, std::slice::from_ref(&token)).is_empty());
        assert!(late.presented_expired(&grant));
        let mut replayed = fresh();
        assert!(claim(&mut replayed, &[token]).is_empty());
        assert!(
            !replayed.presented_expired(&grant),
            "the expired token is used up"
        );
        let mut unknown = fresh();
        assert!(claim(&mut unknown, &["outside:unknown".to_string()]).is_empty());
        assert!(!unknown.presented_expired(&grant));
    }

    #[test]
    fn plugin_tokens_bind_route_session_and_expiry_and_are_single_use() {
        let owner = Arc::new(());
        let mut scope = Scope::new(&owner, "session").for_tool("read", &serde_json::Value::Null);
        let grant = AccessGrant {
            path: "/outside/a".into(),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let token = issue(&scope, &grant);
        assert!(claim(
            &mut Scope::new(&Arc::new(()), "session").for_tool("read", &serde_json::Value::Null),
            &[token.clone()]
        )
        .is_empty());
        assert!(claim(
            &mut Scope::new(&owner, "other").for_tool("read", &serde_json::Value::Null),
            &[token.clone()]
        )
        .is_empty());
        assert!(claim(
            &mut Scope::new(&owner, "session").for_tool("write", &serde_json::Value::Null),
            &[token.clone()]
        )
        .is_empty());
        assert_eq!(claim(&mut scope, &[token.clone()]), vec![grant.clone()]);
        assert!(claim(&mut scope, &[token]).is_empty());
        assert!(claim(
            &mut scope,
            &[format!(
                "outside:{}",
                serde_json::to_string(&grant).unwrap()
            )]
        )
        .is_empty());
        let expired = issue(&scope, &grant);
        TOKENS.lock().unwrap().get_mut(&expired).unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        assert!(claim(&mut scope, &[expired]).is_empty());
    }

    #[test]
    fn plugin_route_replacement_marker_does_not_consume_or_widen_grants() {
        let owner = Arc::new(());
        let replacement = Arc::new(());
        let mut original = Scope::new(&owner, "session").for_tool("grep", &serde_json::Value::Null);
        let grant = AccessGrant {
            path: "/outside/packages".into(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let token = issue(&original, &grant);
        for (session, tool) in [("other", "grep"), ("session", "read")] {
            let mut foreign =
                Scope::new(&replacement, session).for_tool(tool, &serde_json::Value::Null);
            assert!(claim(&mut foreign, std::slice::from_ref(&token)).is_empty());
            assert_eq!(
                foreign.retry_refusal_reason(&grant, false),
                Some("approval_not_recognized")
            );
        }
        let mut replaced =
            Scope::new(&replacement, "session").for_tool("grep", &serde_json::Value::Null);
        assert!(claim(&mut replaced, std::slice::from_ref(&token)).is_empty());
        assert_eq!(
            replaced.retry_refusal_reason(&grant, false),
            Some("approval_route_replaced")
        );
        let sibling = AccessGrant {
            path: "/outside/packages-sibling".into(),
            ..grant.clone()
        };
        assert_eq!(
            replaced.retry_refusal_reason(&sibling, false),
            Some("approval_not_recognized")
        );
        // A foreign route's diagnostic probe must not spend the original route's approval.
        assert_eq!(claim(&mut original, &[token]), vec![grant]);
    }

    #[test]
    fn plugin_dead_route_diagnostics_have_a_bounded_lifetime() {
        let owner = Arc::new(());
        let now = Instant::now();
        let issued = Issued {
            scope: Scope::new(&owner, "session"),
            grant: AccessGrant {
                path: "/outside/packages".into(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Read,
            },
            expires: now + TTL,
            origin: None,
        };
        drop(owner);
        assert!(retained(&issued, now));
        assert!(!retained(&issued, now + TTL + EXPIRED_GRACE));
    }

    /// Each preview-to-apply renewal gets a fresh short TTL, but a chain of
    /// renewals cannot keep one approval alive past the absolute cap.
    #[test]
    fn plugin_continuations_stop_renewing_after_the_absolute_cap() {
        let owner = Arc::new(());
        let fresh = || Scope::new(&owner, "session").for_tool("edit", &serde_json::Value::Null);
        let grant = AccessGrant {
            path: "/outside/b".into(),
            kind: GrantKind::File,
            operation: AccessOperation::Write,
        };
        let mut scope = fresh();
        let token = issue(&scope, &grant);
        assert_eq!(claim(&mut scope, &[token]), vec![grant.clone()]);
        let approved = scope.first_approved.unwrap();

        // A renewal carries the original approval time, so claiming it in a
        // later dispatch keeps the chain anchored to the first approval.
        let renewed = continuation(&scope, std::slice::from_ref(&grant));
        assert_eq!(renewed.len(), 1);
        assert_eq!(
            TOKENS.lock().unwrap().get(&renewed[0]).unwrap().origin,
            Some(approved)
        );
        let mut next = fresh();
        assert_eq!(claim(&mut next, &renewed), vec![grant.clone()]);
        assert_eq!(next.first_approved, Some(approved));

        // Just inside the cap a renewal is issued, but it expires at the cap
        // rather than a full TTL later.
        let late = approved + MAX_CHAIN - Duration::from_secs(60);
        let last = continuation_at(&next, std::slice::from_ref(&grant), late);
        assert_eq!(last.len(), 1);
        assert_eq!(
            TOKENS.lock().unwrap().get(&last[0]).unwrap().expires,
            approved + MAX_CHAIN
        );

        // At the cap nothing is renewed: the apply half must ask again.
        assert!(
            continuation_at(&next, std::slice::from_ref(&grant), approved + MAX_CHAIN).is_empty()
        );
    }
}
