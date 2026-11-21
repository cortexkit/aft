//! Tracks, per session, access-grant paths confirmed to be absent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
use crate::db::remote_exec::PolicyKey;

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct UnrestoredGrant {
    pub(crate) path: PathBuf,
    pub(crate) mode: AccessOperation,
    pub(crate) retryable: bool,
}

impl UnrestoredGrant {
    fn new(grant: &AccessGrant, retryable: bool) -> Self {
        Self {
            path: grant.path.clone(),
            mode: grant.operation,
            retryable,
        }
    }

    pub(crate) fn covers(&self, operation: AccessOperation, path: &Path) -> bool {
        self.capability().covers(operation, path)
    }

    fn capability(&self) -> AccessGrant {
        AccessGrant {
            path: self.path.clone(),
            kind: GrantKind::Subtree,
            operation: self.mode,
        }
    }
}

#[derive(Default)]
struct State {
    absent: HashMap<PolicyKey, Vec<AccessGrant>>,
    reported: HashMap<PolicyKey, Vec<AccessGrant>>,
    #[cfg(test)]
    probes: HashMap<PathBuf, usize>,
}

#[derive(Default)]
pub(crate) struct LaunchGrantState(parking_lot::Mutex<State>);

fn session_key(key: &PolicyKey) -> PolicyKey {
    let mut key = key.clone();
    key.epoch = 0;
    key
}

impl LaunchGrantState {
    pub(crate) fn clear(&self) {
        *self.0.lock() = State::default();
    }

    pub(crate) fn is_absent(&self, key: &PolicyKey, grant: &AccessGrant) -> bool {
        self.0
            .lock()
            .absent
            .get(&session_key(key))
            .is_some_and(|grants| grants.contains(grant))
    }

    /// After a grant path is confirmed missing, recreating an object at that
    /// path must not restore the grant in this session. New explicit authority
    /// is required, rather than a route rebind or an epoch change.
    pub(crate) fn mark_absent(&self, key: &PolicyKey, grant: AccessGrant) {
        let mut state = self.0.lock();
        let grants = state.absent.entry(session_key(key)).or_default();
        if !grants.contains(&grant) {
            grants.push(grant);
        }
    }

    pub(crate) fn unavailable(
        &self,
        key: &PolicyKey,
        pending: Vec<AccessGrant>,
    ) -> Vec<UnrestoredGrant> {
        let state = self.0.lock();
        let absent = state
            .absent
            .get(&session_key(key))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut result: Vec<_> = pending
            .iter()
            .filter(|grant| !absent.contains(grant))
            .map(|grant| UnrestoredGrant::new(grant, true))
            .collect();
        result.extend(
            absent
                .iter()
                .map(|grant| UnrestoredGrant::new(grant, false)),
        );
        result
    }

    pub(crate) fn notices(
        &self,
        key: &PolicyKey,
        grants: &[UnrestoredGrant],
    ) -> Vec<UnrestoredGrant> {
        // Route epochs may change while the worker's session stays the same.
        let session = session_key(key);
        let mut state = self.0.lock();
        let reported = state.reported.entry(session).or_default();
        grants
            .iter()
            .filter(|grant| {
                if grant.retryable {
                    return true;
                }
                let capability = grant.capability();
                if reported.contains(&capability) {
                    return false;
                }
                reported.push(capability);
                true
            })
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn note_probe(&self, path: &Path) {
        *self.0.lock().probes.entry(path.to_path_buf()).or_default() += 1;
    }

    // Only the Unix-only restore tests read the count.
    #[cfg(all(test, unix))]
    pub(crate) fn probe_count(&self, path: &Path) -> usize {
        self.0.lock().probes.get(path).copied().unwrap_or_default()
    }
}

/// Include unrestored-grant information in rendered output and structured data.
/// This runs only for the completion being delivered, not intermediate polls.
pub(crate) fn annotate_shell_reply(
    text: &mut String,
    response: &mut crate::protocol::Response,
    grants: &[UnrestoredGrant],
) {
    if grants.is_empty() {
        return;
    }
    let entries = grants
        .iter()
        .map(|grant| {
            let mode = match grant.mode {
                AccessOperation::Read => "read",
                AccessOperation::Write => "write",
            };
            let status = if grant.retryable {
                "temporarily unavailable; may be retried; retryable=true"
            } else {
                "no longer exists; retryable=false"
            };
            format!(
                "{} ({mode}: {status})",
                grant.path.to_string_lossy().escape_debug()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let line = format!("Unrestored launch grants: {entries}. Reads or writes there may fail with a permission error until restored.\n");
    text.insert_str(0, &line);
    response.data["unrestored_grants"] =
        serde_json::to_value(grants).expect("grant notices serialize");
}
