//! Configure-owned operations registered above the runtime, with runtime-owned
//! continuation state so idle draining does not depend on command handlers.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::context::{AppContext, ConfigureMaintenanceJob};

pub(crate) struct Hooks {
    pub drain: fn(&AppContext),
    pub drain_yielding: fn(&AppContext) -> bool,
    pub pending: fn(&AppContext, &mut ConfigureMaintenanceState) -> bool,
    pub prefix: fn(&AppContext, &mut ConfigureMaintenanceState) -> bool,
    pub unit: fn(&Arc<AppContext>, &mut ConfigureMaintenanceState) -> bool,
    pub database_runtime: fn(&AppContext, &Path, &Path, crate::db::OpenMode),
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();
// Runtime-only embedders may never load command handlers. Without a configure
// producer there is no command-owned maintenance to drain.
static IDLE_HOOKS: Hooks = Hooks {
    drain: |_| {},
    drain_yielding: |_| false,
    pending: |_, _| false,
    prefix: |_, _| false,
    unit: |_, _| false,
    database_runtime: |_, _, _, _| {},
};

pub(crate) fn register(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

pub(crate) fn hooks() -> &'static Hooks {
    HOOKS.get().unwrap_or(&IDLE_HOOKS)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ConfigureMaintenanceStage {
    #[default]
    Admission,
    DatabaseRuntime,
    SessionReplay,
    BashRuntime,
    ProjectRuntime,
    Watcher,
    ProcessFlags,
    Status,
}

impl ConfigureMaintenanceStage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::DatabaseRuntime => "database_runtime",
            Self::SessionReplay => "session_replay",
            Self::BashRuntime => "bash_runtime",
            Self::ProjectRuntime => "project_runtime",
            Self::Watcher => "watcher",
            Self::ProcessFlags => "process_flags",
            Self::Status => "status",
        }
    }

    /// Initial work that runs before the first request after configure: bash
    /// replay restores the DB tasks and one bounded disk batch, so an immediate
    /// `bash_drain_completions` sees those results. Disk-only tasks beyond that
    /// batch arrive through later maintenance and completion delivery rather
    /// than delaying requests for a full disk scan. The prefix also prepares the
    /// project runtime (a queued `outline`/`glob` must see the rebuilt gitignore
    /// matcher), and the watcher start. The watcher only observes changes made
    /// after it subscribes, so a file written between the configure ack and the
    /// first request is lost for good if the start waits for a later yield: on a
    /// small tree the index is ready after one request, an ignore-rule change
    /// written right after it was never seen, and the ignored file stayed in
    /// grep results. Everything after these is housekeeping and yields to
    /// requests.
    pub(crate) fn is_non_yielding_prefix(self) -> bool {
        matches!(
            self,
            Self::Admission
                | Self::DatabaseRuntime
                | Self::SessionReplay
                | Self::BashRuntime
                | Self::ProjectRuntime
                | Self::Watcher
        )
    }
}

#[derive(Debug)]
pub(crate) struct ConfigureMaintenanceContinuation {
    pub(crate) job: ConfigureMaintenanceJob,
    pub(crate) stage: ConfigureMaintenanceStage,
    pub(crate) replay_retry_at: Option<Instant>,
    pub(crate) lexical_bind_retry_at: Option<Instant>,
    pub(crate) callgraph_upgrade_retry_at: Option<Instant>,
    pub(crate) replay_error_logged: bool,
}

impl ConfigureMaintenanceContinuation {
    fn new(job: ConfigureMaintenanceJob) -> Self {
        Self {
            job,
            stage: ConfigureMaintenanceStage::Admission,
            replay_retry_at: None,
            lexical_bind_retry_at: None,
            callgraph_upgrade_retry_at: None,
            replay_error_logged: false,
        }
    }

    pub(crate) fn retry_at(&self) -> Option<Instant> {
        self.callgraph_upgrade_retry_at
            .or(self.lexical_bind_retry_at)
            .or(self.replay_retry_at)
    }
}

#[derive(Debug, Default)]
pub(crate) struct ConfigureMaintenanceState {
    pub(crate) jobs: VecDeque<ConfigureMaintenanceContinuation>,
    // Freeze retry eligibility across one full drain so a deferred unit cannot
    // become due again while other units run and monopolize the same drain.
    pub(crate) replay_retry_cutoff: Option<Instant>,
}

impl ConfigureMaintenanceState {
    pub(crate) fn jobs_is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub(crate) fn has_ready_job(&self) -> bool {
        let now = self.replay_retry_cutoff.unwrap_or_else(Instant::now);
        self.jobs.iter().any(|continuation| {
            continuation
                .retry_at()
                .is_none_or(|deadline| deadline <= now)
        })
    }

    pub(crate) fn select_ready_job(&mut self, ctx: &AppContext) -> bool {
        let now = self.replay_retry_cutoff.unwrap_or_else(Instant::now);
        let Some(index) = self.jobs.iter().position(|continuation| {
            ctx.subc_unbound_quiesced()
                || continuation
                    .retry_at()
                    .is_none_or(|deadline| deadline <= now)
        }) else {
            return false;
        };
        self.jobs.rotate_left(index);
        true
    }

    pub(crate) fn absorb_enqueued(&mut self, ctx: &AppContext) {
        self.jobs.extend(
            ctx.drain_configure_maintenance()
                .into_iter()
                .map(ConfigureMaintenanceContinuation::new),
        );
    }

    /// True when the next unit to run starts a job or continues one past its
    /// request-critical prefix, so stopping here leaves no prefix half done.
    ///
    /// Why the remaining stops are safe: stopping before a job's Admission
    /// leaves the root exactly as it is after acknowledging a configure request
    /// but before submitting its deferred maintenance. Every bind already
    /// passes through that state. Past the
    /// prefix, the standalone loop already lets requests (configures and edits
    /// included) run between units. A writer that runs meanwhile either keeps
    /// the configure generation (an equivalent rebind, an LSP-path update or
    /// an edit, none of which touch what the remaining stages read) or
    /// advances it, in which case `drop_superseded_parked_continuations`
    /// discards the rest and the newer configure's own tail repeats every
    /// root-scoped stage.
    pub(crate) fn can_step_aside(&self) -> bool {
        self.jobs.front().is_none_or(|continuation| {
            continuation.stage == ConfigureMaintenanceStage::Admission
                || !continuation.stage.is_non_yielding_prefix()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_registration_has_no_configure_work_to_drain() {
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ));
        let mut state = ConfigureMaintenanceState::default();
        (IDLE_HOOKS.drain)(&ctx);
        assert!(!(IDLE_HOOKS.drain_yielding)(&ctx));
        assert!(!(IDLE_HOOKS.pending)(&ctx, &mut state));
        assert!(!(IDLE_HOOKS.prefix)(&ctx, &mut state));
        assert!(!(IDLE_HOOKS.unit)(&ctx, &mut state));
        assert!(state.jobs_is_empty());
    }
}
