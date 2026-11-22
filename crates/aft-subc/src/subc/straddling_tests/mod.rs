//! Checks that exercise a tool handler together with daemon-owned state.

mod a02_tests;
mod capture_reads;
mod external_hook;
mod registry;
#[cfg(unix)]
pub(super) mod remote;
mod secret_audit_regressions;
mod secret_bash;
mod secret_fetch;
