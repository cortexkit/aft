//! Cooperative routing hint only. The daemon still verifies the ticket's own row.

use super::model::PolicySnapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) const START_VIEW_ENV: &str = "AFT_GH_SPEECH_START_VIEW";
pub(crate) const PROVISIONED_ENV: &str = "AFT_GH_SPEECH_PROVISIONED";
pub(crate) const WORKER_POLICY_ENV: &str = "AFT_GH_WORKER_WRITE_POLICY";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct View {
    ticket_digest: Vec<u8>,
    policy: Option<PolicySnapshot>,
}

pub(crate) fn inject(ticket: Option<&str>, env: &mut HashMap<String, String>, fallback: &Path) {
    env.remove(START_VIEW_ENV);
    env.remove(PROVISIONED_ENV);
    env.remove(WORKER_POLICY_ENV);
    let Some(ticket) = ticket else { return };
    if crate::gh_shim_ticket::redeem(ticket)
        .and_then(|ticket| ticket.lifecycle_scope)
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Worker)
    {
        env.insert(WORKER_POLICY_ENV.into(), "1".into());
        env.insert(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV.into(), "1".into());
        return;
    }
    let Some(speech) = crate::gh_shim_ticket::speech_for_ticket(ticket) else {
        return;
    };
    if !speech.prepared.provisioned {
        return;
    }
    env.insert(PROVISIONED_ENV.into(), "1".into());
    let Some(digest) = crate::gh_shim_ticket::speech_digest(ticket) else {
        return;
    };
    let root = env
        .get("AFT_GH_SHIM_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.join("gh-shim"));
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(format!("speech-start-{name}.json"));
    let view = View {
        ticket_digest: digest.to_vec(),
        policy: speech.prepared.classification.clone(),
    };
    if crate::private_storage::open_root(&root).is_ok() {
        if let Ok(bytes) = serde_json::to_vec(&view) {
            if crate::private_storage::write(&path, &bytes).is_ok() {
                env.insert(START_VIEW_ENV.into(), path.display().to_string());
            }
        }
    }
}

pub(crate) fn protected(args: &[std::ffi::OsString]) -> bool {
    if crate::environment::non_empty_var(PROVISIONED_ENV).as_deref() != Some("1") {
        return false;
    }
    let view = crate::environment::non_empty_var(START_VIEW_ENV)
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<View>(&bytes).ok());
    let ticket = crate::environment::non_empty_var(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV)
        .and_then(|ticket| crate::gh_shim_ticket::speech_digest(&ticket));
    if let (Some(view), Some(ticket)) = (view, ticket) {
        if view.ticket_digest.as_slice() == ticket.as_slice() {
            if let Some(policy) = view.policy {
                if let Ok(protected) = crate::gh_shim::speech_start_classification(&policy, args) {
                    return protected;
                }
            }
        }
    }
    crate::gh_shim::speech_protected_without_view(args)
}
