//! The access-policy environment of one tool call on a route.
//!
//! Every path check a tool makes (`AppContext::validate_read_path` and the
//! rest) reads per-call state that only the caller can install: the route's
//! ask route, the scope whose saved grants apply, forced root restriction
//! for an untrusted bind, and the root confinement fence of a confined
//! scoped worker. [`with_tool_call_policy`] installs all of it. The tool-call
//! path and `page.collect` both go through it, so a page can never be read
//! under looser rules than the session's own `read` tool.

use super::*;

/// Run `work` as tool `tool` with `arguments` on `identity`, under that
/// route's access policy, then pass its result to `finish` while the policy
/// is still installed. `work` does not run when the confinement admission
/// refuses the call; `finish` then receives that refusal.
///
/// `ask_route` is where a question the policy raises would go
/// (`access_grants::ask_route_for_call`); `trust` is the bind's trust.
#[allow(clippy::too_many_arguments)]
pub(super) fn with_tool_call_policy<T, R>(
    ctx: &AppContext,
    identity: &RouteIdentity,
    request_id: &str,
    ask_route: crate::access_policy::AskRoute,
    trust: BindTrust,
    tool: &str,
    arguments: Value,
    work: impl FnOnce(Value) -> T,
    finish: impl FnOnce(Result<T, Response>) -> R,
) -> R {
    if matches!(ask_route, crate::access_policy::AskRoute::Plugin { .. }) {
        ctx.mark_standalone_plugin_host();
    }
    let _ask_route = crate::access_policy::ask_route_guard(Some(ask_route));
    let _grants =
        crate::access_policy::scope_grants_guard(access_grants::scope_grants_denial(identity));
    crate::access_policy::with_scope(Some(access_grants::policy_scope_key(identity)), || {
        let run = || {
            finish(remote_policy::with_call_policy(
                ctx, identity, request_id, tool, arguments, work,
            ))
        };
        if matches!(trust, BindTrust::Untrusted) {
            ctx.with_force_restrict(request_id, run)
        } else {
            run()
        }
    })
}
