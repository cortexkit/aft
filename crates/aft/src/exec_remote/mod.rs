//! Caller-side client for the daemon's `exec-remote/v1` capability.
//! This module does not choose execution policy or launch local commands.

pub use cortexkit_exec_remote_types as types;
use types::{Outcome, TerminalRecord};

mod stream;
pub use stream::{OutputSink, ResumePoint, StreamConsumer};

#[derive(Debug)]
pub enum Error {
    Protocol(String),
    Sink(std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    RunLocally { reason: types::RefusalReason },
    Exited { code: i32 },
    Signalled { signal: i32 },
    Cancelled,
    DeadlineKilled,
    CancelKilled,
    OutcomeUnknown,
    HistoryExpired,
}

pub fn grade(_terminal: &TerminalRecord) -> Verdict {
    unimplemented!("terminal grading")
}

#[cfg(test)]
mod tests;
