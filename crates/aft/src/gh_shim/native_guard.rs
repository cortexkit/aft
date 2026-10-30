//! Native gh is an operator-only exit from the shim, never a worker fallback.

use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ShellSignals {
    pub(super) sandboxed: bool,
    pub(super) worker_marked: bool,
}

impl ShellSignals {
    pub(super) fn from_process() -> Self {
        Self {
            sandboxed: native_sandbox_active(),
            worker_marked: std::env::var_os(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV).is_some(),
        }
    }
}

/// Whether this shell runs inside AFT's own worker jail, from kernel state only.
///
/// A generic sandbox signal is not enough. Linux `no_new_privs` and macOS
/// `sandbox_check(pid, NULL, 0)` are also set by containers, Kubernetes pods
/// with `allowPrivilegeEscalation: false`, systemd `NoNewPrivileges=`,
/// bubblewrap, CI job sandboxes and other tools' Seatbelt profiles. A standalone
/// user there had working gh passthrough and must keep it.
///
/// Only the worker-jail profile denies reading a canary file at a fixed path in
/// the account's default AFT storage (see `crate::worker_jail_canary`). The
/// deny is enforced by Landlock or Seatbelt, inherited across exec and
/// irremovable, so clearing the environment or unsetting the ticket cannot
/// make a fenced worker look like a head. Off workers, relaxed Linux and Windows
/// have no proven canary fence and report it as unenforced. This gates the shim,
/// not direct upstream execution or credential-service access.
pub(super) fn native_sandbox_active() -> bool {
    let jailed = crate::worker_jail_canary::inside_worker_jail();
    // Support aid: show what the canary probe saw. It never changes the
    // decision, so a worker setting or clearing it gains nothing.
    if std::env::var_os(CANARY_DIAGNOSTICS_ENV).is_some() {
        eprintln!(
            "gh-shim: worker-jail canary jailed={jailed}: {}",
            crate::worker_jail_canary::diagnostics()
        );
    }
    jailed
}

/// Set to any value to print the canary probe's raw results on stderr.
pub(super) const CANARY_DIAGNOSTICS_ENV: &str = "AFT_GH_SHIM_CANARY_DIAGNOSTICS";

pub(super) fn permission_with(
    relay: &RelayContext,
    signals: ShellSignals,
    lookup: impl FnOnce(&RelayContext) -> Result<Option<bool>, String>,
) -> Result<(), String> {
    // Denied access to the worker-jail canary proves confinement even without
    // a ticket. Reject it regardless of the daemon's session lookup result.
    if signals.sandboxed {
        if relay
            .ticket
            .as_deref()
            .is_none_or(|ticket| ticket.is_empty())
        {
            // A worker shell gets the read broker, never a ticket, so a
            // command that is not a repository read always lands here.
            return Err(
                "in a worker shell, gh runs only reads of a repository; this command is not one"
                    .into(),
            );
        }
        return Err(
            "operator gh is not available inside the worker jail; use governed reads or bot speech"
                .into(),
        );
    }
    // AFT also marks worker shells on platforms without an enforced canary
    // fence. Keep those shells off operator gh even when scope lookup is down.
    if signals.worker_marked {
        return Err(
            "operator gh is not available to worker-marked shells; use governed reads or bot speech"
                .into(),
        );
    }
    if relay
        .ticket
        .as_deref()
        .is_some_and(|ticket| !ticket.is_empty())
    {
        if matches!(lookup(relay), Ok(Some(true))) {
            return Err(
                "operator gh is not available to worker sessions; use governed reads or bot speech"
                    .into(),
            );
        }
    }
    // The main agent can have a ticket without an authenticated session scope
    // when prefrontal-core is offline. In an unconfined, unmarked shell, a failed
    // or empty lookup does not establish worker status.
    Ok(())
}

pub(super) fn permission_from_process() -> Result<(), String> {
    permission_with(
        &RelayContext::from_process(),
        ShellSignals::from_process(),
        relay_client::worker_session,
    )
}

pub(super) fn dispatch_with(
    args: &[OsString],
    relay: &RelayContext,
    signals: ShellSignals,
    lookup: impl FnOnce(&RelayContext) -> Result<Option<bool>, String>,
    native: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    match permission_with(relay, signals, lookup) {
        Ok(()) => native(args),
        Err(text) => refuse(RefusalCode::WorkerNativeGh, &text),
    }
}

pub(super) fn dispatch(
    args: &[OsString],
    relay: &RelayContext,
    signals: ShellSignals,
    native: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    dispatch_with(args, relay, signals, relay_client::worker_session, native)
}
