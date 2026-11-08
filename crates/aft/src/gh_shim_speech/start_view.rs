//! Cooperative routing hint only. The daemon still verifies the ticket's own row.

use super::model::PolicySnapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
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

/// Owns one launch's view, including a shared OS lock that keeps crash cleanup
/// from removing it while a long-running command can still read it.
#[derive(Debug)]
pub(crate) struct StartView {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for StartView {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
            drop(file);
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "could not remove speech start view {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

pub(crate) fn inject(
    ticket: Option<&str>,
    env: &mut HashMap<String, String>,
    fallback: &Path,
) -> Option<StartView> {
    env.remove(START_VIEW_ENV);
    env.remove(PROVISIONED_ENV);
    env.remove(WORKER_POLICY_ENV);
    let ticket = ticket?;
    if crate::gh_shim_ticket::redeem(ticket)
        .and_then(|ticket| ticket.lifecycle_scope)
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Worker)
    {
        env.insert(WORKER_POLICY_ENV.into(), "1".into());
        env.insert(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV.into(), "1".into());
        return None;
    }
    let speech = crate::gh_shim_ticket::speech_for_ticket(ticket)?;
    if !speech.prepared.provisioned {
        return None;
    }
    env.insert(PROVISIONED_ENV.into(), "1".into());
    let digest = crate::gh_shim_ticket::speech_digest(ticket)?;
    let root = env
        .get("AFT_GH_SHIM_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.join("gh-shim"));
    // Tickets can be shared; a launch must never overwrite or unlink another
    // running launch's hint. The digest remains inside the authenticated view.
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).ok()?;
    let name: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(format!("speech-start-{name}.json"));
    let view = View {
        ticket_digest: digest.to_vec(),
        policy: speech.prepared.classification.clone(),
    };
    // Protect only the known directory; never enumerate its accumulated files
    // during a shell launch to repair permissions on unrelated subdirectories.
    crate::private_storage::open_dir(&root, &root).ok()?;
    let bytes = serde_json::to_vec(&view).ok()?;
    let file = crate::private_storage::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .ok()?;
    let mut owned = StartView {
        path,
        file: Some(file),
    };
    let file = owned.file.as_mut()?;
    file.write_all(&bytes).ok()?;
    // Shared locks allow the shim to read on Windows as well as Unix. The
    // sweeper probes an exclusive lock, without waiting for a live launch.
    file.try_lock_shared().ok()?;
    sweep::queue_root(root);
    env.insert(START_VIEW_ENV.into(), owned.path.display().to_string());
    Some(owned)
}

mod sweep;
pub(crate) use sweep::{queue_sweep, sweep_queued};

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
