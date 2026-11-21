use std::io;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
use std::time::Instant;

use crate::lsp::registry::ServerKind;

#[cfg(unix)]
use crate::bash_background::process::ProcessIdentity;

#[cfg(unix)]
use crate::bash_background::process::TERMINATE_GRACE;

#[derive(Clone, Debug, Default)]
pub(in crate::lsp) struct TrackedProcess(Arc<Leader>);

#[derive(Debug, Default)]
struct Leader {
    pid: u32,
    #[cfg(unix)]
    kind: Option<ServerKind>,
    #[cfg(unix)]
    state: Mutex<LeaderState>,
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct LeaderState {
    identity: Option<ProcessIdentity>,
    reaped: bool,
    term_at: Option<Instant>,
    warned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::lsp) enum GroupSignal {
    Maintain(Instant),
    Kill,
    Observe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::lsp) enum GroupOutcome {
    Gone,
    Retain,
    Signalled,
    Unproven,
}

impl TrackedProcess {
    pub(in crate::lsp) fn new(pid: u32, kind: Option<&ServerKind>) -> Self {
        #[cfg(not(unix))]
        let _ = kind;
        Self(Arc::new(Leader {
            pid,
            #[cfg(unix)]
            kind: kind.cloned(),
            #[cfg(unix)]
            state: Mutex::new(LeaderState {
                identity: ProcessIdentity::of(pid),
                ..LeaderState::default()
            }),
        }))
    }

    pub(in crate::lsp) fn pid(&self) -> u32 {
        self.0.pid
    }

    pub(in crate::lsp) fn same_entry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(super) fn reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        let status = child.try_wait()?;
        #[cfg(unix)]
        if status.is_some() {
            state.reaped = true;
        }
        Ok(status)
    }

    #[cfg(unix)]
    pub(super) fn exit_observed(&self) -> io::Result<bool> {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.reaped {
            return Ok(true);
        }
        // Leave an exited leader waitable so the group number stays pinned
        // while the remaining processes are signalled.
        // SAFETY: waitid writes a siginfo_t and leaves it zeroed on no exit.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            loop {
                if libc::waitid(
                    libc::P_PID,
                    self.pid() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                ) == 0
                {
                    return Ok(info.si_pid() != 0);
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    state.reaped = true;
                }
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    #[cfg(unix)]
    pub(in crate::lsp) fn signal(&self, signal: GroupSignal) -> GroupOutcome {
        let Ok(pgid) = libc::pid_t::try_from(self.pid()) else {
            return GroupOutcome::Unproven;
        };
        if pgid <= 0 {
            return GroupOutcome::Unproven;
        }
        // The wait that can release the leader PID shares this lock. Even if
        // the leader exits between validation and killpg, its unreaped PID
        // still pins the group number until this signal attempt finishes.
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if signal_group(pgid, 0) == GroupOutcome::Gone {
            return GroupOutcome::Gone;
        }
        if state.reaped
            || !state
                .identity
                .is_some_and(|leader| leader.is_running_in_group(pgid))
        {
            if !state.warned {
                crate::slog_warn!(
                    "cannot verify language server {} process group {pgid}; leaving the group alone",
                    self.0.kind.as_ref().map_or("unknown", ServerKind::id_str),
                );
                state.warned = true;
            }
            return GroupOutcome::Unproven;
        }
        let sig = match signal {
            GroupSignal::Observe => return GroupOutcome::Retain,
            GroupSignal::Kill => libc::SIGKILL,
            GroupSignal::Maintain(now) => {
                let first = *state.term_at.get_or_insert(now);
                if now.saturating_duration_since(first) >= TERMINATE_GRACE {
                    libc::SIGKILL
                } else {
                    libc::SIGTERM
                }
            }
        };
        signal_group(pgid, sig)
    }

    #[cfg(not(unix))]
    pub(in crate::lsp) fn signal(&self, signal: GroupSignal) -> GroupOutcome {
        let pid = self.pid();
        if pid == 0 {
            return GroupOutcome::Unproven;
        }
        let taskkill = move || {
            std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        };
        match signal {
            GroupSignal::Observe => {
                if crate::process_identity::is_process_alive(pid) {
                    GroupOutcome::Retain
                } else {
                    GroupOutcome::Gone
                }
            }
            GroupSignal::Maintain(_now) => {
                if taskkill().is_ok_and(|status| status.success()) {
                    GroupOutcome::Gone
                } else {
                    GroupOutcome::Retain
                }
            }
            GroupSignal::Kill => {
                if std::thread::Builder::new()
                    .name("aft-lsp-force-kill".into())
                    .spawn(taskkill)
                    .is_ok()
                {
                    GroupOutcome::Signalled
                } else {
                    GroupOutcome::Retain
                }
            }
        }
    }

    #[cfg(all(test, unix))]
    pub(in crate::lsp) fn replace_start_time_for_test(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.identity.as_mut().unwrap().started_at_ms ^= 1;
    }
}

#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, sig: libc::c_int) -> GroupOutcome {
    // SAFETY: pgid is positive and killpg does not dereference pointers.
    if unsafe { libc::killpg(pgid, sig) } == 0 {
        GroupOutcome::Signalled
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        GroupOutcome::Gone
    } else {
        GroupOutcome::Retain
    }
}
