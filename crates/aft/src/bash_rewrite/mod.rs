//! Bash command rewriter for hoisted bash.
//!
//! Rewriting is an optimization of native bash, not a second command
//! language. The dispatch layer makes a pre-execution decision and records it
//! for differential tests. Once a request is accepted, its handler response is
//! returned directly; native bash is never run as a recovery path.

pub mod catalog;
pub mod differential;
pub mod dispatch;
pub mod footer;
pub mod observation;
pub use crate::bash_parse as parser;
pub mod rules;

use serde_json::Value;

use crate::context::AppContext;
use crate::protocol::Response;
use crate::sandbox_spawn::{native_sandbox_enforced, AuthenticatedPrincipal};

#[derive(Debug, Clone, PartialEq)]
pub struct RewriteRequest {
    pub request_id: String,
    pub command: String,
    pub session_id: Option<String>,
    pub rule_id: &'static str,
    pub branch_id: &'static str,
    pub decision_class_id: &'static str,
    pub params: Value,
    /// The caller's names for AFT's command and patch tools, which the footer
    /// of a rewritten command names (see [`crate::host_tool_names`]).
    pub host_tool_names: crate::host_tool_names::HostToolNames,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclineReason {
    pub rule_id: Option<&'static str>,
    pub branch_id: &'static str,
    pub decision_class_id: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RewriteDecision {
    Accept(RewriteRequest),
    Decline(DeclineReason),
}

/// A rewrite rule owns both the pre-execution shape decision and the typed
/// request it emits. Implementations keep parsing in the decision phase so an
/// accepted request has one stable request ID and one decision class.
pub trait RewriteRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> RewriteDecision;
    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response;
}

thread_local! {
    /// Set while a codemode program's shell call runs. The program parses
    /// what a real shell prints, so its commands are never answered by AFT's
    /// own tools (line-numbered reads, grep formatting, edit reports).
    static REWRITES_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct RewritesOffScope(bool);

impl Drop for RewritesOffScope {
    fn drop(&mut self) {
        REWRITES_OFF.with(|slot| slot.set(self.0));
    }
}

/// Run `run` with command rewriting turned off when `off` is true.
pub(crate) fn with_rewrites_off<R>(off: bool, run: impl FnOnce() -> R) -> R {
    let previous = REWRITES_OFF.with(|slot| slot.replace(off || slot.get()));
    let _scope = RewritesOffScope(previous);
    run()
}

/// Try to rewrite a synthetic request. Production bash calls
/// [`try_rewrite_for_request`] with the caller's request ID; this compatibility
/// wrapper is retained for direct library users and existing unit tests.
pub fn try_rewrite(
    command: &str,
    session_id: Option<&str>,
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> Option<Response> {
    try_rewrite_for_request(
        command,
        "bash_rewrite",
        session_id,
        &crate::host_tool_names::HostToolNames::default(),
        ctx,
        principal,
    )
}

/// Make the rewrite decision for one public bash request. A native route is
/// recorded when the sandbox owns execution or when no rule accepts the
/// command. The record is test-visible through `dispatch::route_record` and an
/// opt-in JSONL sidecar, without changing the agent-facing response.
pub fn try_rewrite_for_request(
    command: &str,
    request_id: &str,
    session_id: Option<&str>,
    host_tool_names: &crate::host_tool_names::HostToolNames,
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> Option<Response> {
    // A codemode program's shell call always runs in a real shell. This is a
    // caller decision, not a rewrite branch, so it records no branch.
    if REWRITES_OFF.with(std::cell::Cell::get) {
        return None;
    }
    // Windows relaxed shells must reach task capture to report that their
    // filesystem denies are unenforced, even when a rewrite would otherwise fit.
    let unenforced_relaxed = cfg!(windows)
        && crate::sandbox_spawn::effective_sandbox_mode(ctx, principal)
            == crate::config::SandboxMode::Relaxed;
    if native_sandbox_enforced(ctx, principal) || unenforced_relaxed {
        dispatch::record_native(
            request_id,
            catalog::ControlRole::Sandbox,
            "dispatch.native.sandbox",
            if unenforced_relaxed {
                "relaxed sandbox must report unenforced denies"
            } else {
                "native sandbox is enforced"
            },
        );
        return None;
    }
    dispatch::dispatch_for_request(command, request_id, session_id, host_tool_names, ctx)
}
