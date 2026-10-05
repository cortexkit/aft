//! Caller-side client for the daemon's `exec-remote/v1` capability.
//! This module does not choose execution policy or launch local commands.

pub use cortexkit_exec_remote_types as types;
use types::{Killed, Outcome, Ran, TerminalRecord};

mod client;
pub mod policy;
mod reply;
mod request;
mod stream;
pub use client::{ExecRemoteClient, RemoteStream, StreamProgress};
pub use reply::{decode_reply, grade_reply, Reply, ReplyVerdict};
pub use request::{build_request, PresetParams};
pub use stream::{OutputSink, ResumePoint, StreamConsumer};

#[derive(Debug)]
pub enum Error {
    Protocol(String),
    Sink(std::io::Error),
    Transport(subc_client_rs::CallError),
    Connection(subc_client_rs::ConsumerError),
    /// No terminal proof is available. Query status and reattach when a job ID
    /// is known; never turn a transport/decode failure into permission to rerun.
    RecoveryRequired {
        resume: Option<ResumePoint>,
        reason: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) => write!(f, "remote execution protocol: {message}"),
            Self::Sink(error) => write!(f, "remote execution output sink: {error}"),
            Self::Transport(error) => write!(f, "remote execution route: {error}"),
            Self::Connection(error) => write!(f, "remote execution connection: {error}"),
            Self::RecoveryRequired { reason, .. } => write!(
                f,
                "remote execution needs status/attach, not resubmission: {reason}"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Sink(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The executor proved no start. A later caller may run in AFT's local
    /// sandbox and must tell the user; neither this client nor ck-motor does it.
    RunLocally {
        reason: types::RefusalReason,
    },
    Exited {
        code: i32,
    },
    Signalled {
        signal: i32,
    },
    Cancelled,
    DeadlineKilled,
    CancelKilled,
    OutcomeUnknown,
    HistoryExpired,
}

/// Grade the terminal proof without inferring missing execution metadata.
/// Unknown `Ran` overrides even a familiar outcome: it cannot prove no start.
/// Unknown kill reasons are unknown outcomes, not ordinary exits or cancels.
/// The original terminal is delivered to the sink unchanged, including null
/// `ran`, `tree_hash`, and `workspace_changes`; changed files are never copied.
pub fn grade(terminal: &TerminalRecord) -> Verdict {
    match &terminal.ran {
        None | Some(Ran::Remote) | Some(Ran::None) => {}
        Some(_) => return Verdict::OutcomeUnknown,
    }
    // Refusals, lost jobs, and expired history are authoritative before kills.
    match &terminal.outcome {
        Outcome::RefusedBeforeStart { reason } => {
            return Verdict::RunLocally {
                reason: reason.clone(),
            }
        }
        Outcome::OutcomeUnknown | Outcome::Unknown { .. } => return Verdict::OutcomeUnknown,
        Outcome::HistoryExpired => return Verdict::HistoryExpired,
        Outcome::Exit { .. } | Outcome::Signal { .. } | Outcome::Cancelled => {}
        _ => return Verdict::OutcomeUnknown,
    }
    match &terminal.killed {
        Some(Killed::Deadline) => return Verdict::DeadlineKilled,
        Some(Killed::Cancel) => return Verdict::CancelKilled,
        Some(_) => return Verdict::OutcomeUnknown,
        None => {}
    }
    match terminal.outcome {
        Outcome::Exit { code } => Verdict::Exited { code },
        Outcome::Signal { signal } => Verdict::Signalled { signal },
        Outcome::Cancelled => Verdict::Cancelled,
        _ => Verdict::OutcomeUnknown,
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_tests;
