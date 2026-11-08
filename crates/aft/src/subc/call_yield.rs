//! Handing back a waiting call: the tool-provider contract's `call-yield/v1`.
//!
//! A session runner sends `tool.yield` when a person sends a message while a
//! tool call is still running, so the agent can read the message instead of
//! waiting for the call. AFT hands back two kinds of call, and never cancels
//! the work either one waits for:
//!
//! - a `bash` call waiting for its command in the foreground: its command
//!   becomes an ordinary background task, the call answers at once with the
//!   task id, and the task's completion later reaches the launching session as
//!   a notice, like any background task's;
//! - a `bash_watch` call: the watch ends with what it has seen so far, as when
//!   its own time limit passes, and the task is left untouched.
//!
//! Every other call answers `not_yieldable`. The contract is §4.6 of
//! `CONTRACT.md` in `cortexkit-role-tool-provider`; its strings, request
//! parsing and answer shapes come from that crate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use cortexkit_role_tool_provider::call_yield::YieldAnswer as WireAnswer;
use cortexkit_role_tool_provider::errors;
use serde_json::Value;
use subc_protocol::ErrorBody;

use super::RouteChannel;

/// The provider capability (manifest and `role.describe`) promising
/// `tool.yield`, and the route-bind capability a consumer declares to opt in.
pub(crate) use cortexkit_role_tool_provider::CALL_YIELD_V1 as CALL_YIELD;

/// The catalog tag on each tool whose waiting calls can be handed back.
pub(crate) use cortexkit_role_tool_provider::CALL_YIELD_TOOL_TAG;

/// The role operation asking a waiting call to hand back.
pub(crate) use cortexkit_role_tool_provider::ops::TOOL_YIELD;

/// The yield reason a runner sends when the person sent a message. It is the
/// only reason AFT accepts: the reply a handed-back `bash` call gets says a
/// user message arrived, which would be false for any other reason.
pub(crate) use cortexkit_role_tool_provider::call_yield::PERSON_MESSAGE;

/// Whether a route's consumer opted in to call yielding. The contract asks
/// for all three of `tool-result/v1`, `held-reply/v1` and `call-yield/v1`.
pub(crate) use cortexkit_role_tool_provider::consumer_reads_call_yield_v1 as consumer_reads_call_yield;

/// The tools whose waiting calls AFT can hand back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum YieldableTool {
    Bash,
    BashWatch,
}

impl YieldableTool {
    pub(super) fn of(name: &str) -> Option<Self> {
        match name {
            "bash" => Some(Self::Bash),
            "bash_watch" => Some(Self::BashWatch),
            _ => None,
        }
    }
}

/// A decoded `tool.yield` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct YieldRequest {
    pub(super) call_key: String,
    pub(super) reason: String,
}

/// Decode `{name: "tool.yield", arguments: {call_key, reason}}` with the
/// role crate's checks. A malformed request is refused as `invalid_request`
/// naming the field.
pub(super) fn parse_request(body: &Value) -> Result<YieldRequest, ErrorBody> {
    let request: cortexkit_role_tool_provider::call::ToolCallRequest =
        serde_json::from_value(body.clone())
            .map_err(|error| errors::invalid_request("arguments", error.to_string()))?;
    let arguments = cortexkit_role_tool_provider::call_yield::parse_yield_request(&request)?;
    Ok(YieldRequest {
        call_key: arguments.call_key,
        reason: arguments.reason,
    })
}

/// Why an open call cannot hand back now. Each is sent as the
/// `not_yieldable` answer's `reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotYieldable {
    /// The route's consumer did not declare `call-yield/v1` (with
    /// `tool-result/v1` and `held-reply/v1`) when it opened the route.
    NotNegotiated,
    /// The call is not a `bash` or `bash_watch` call.
    Tool,
    /// The call is not waiting: a `bash` call still starting its command or
    /// already sending its result, or a watch that already ended.
    NotWaiting,
    /// The call belongs to a delegated worker or a codemode run, which keep
    /// holding their calls.
    Scope,
    /// The route never receives a task's completion notice, so a handed-back
    /// command's result would not reach the session.
    NoNotices,
    /// A reason other than [`PERSON_MESSAGE`].
    Reason,
}

impl NotYieldable {
    fn code(self) -> &'static str {
        match self {
            Self::NotNegotiated => "not_negotiated",
            Self::Tool => "tool_not_yieldable",
            Self::NotWaiting => "not_waiting",
            Self::Scope => "scope_not_yieldable",
            Self::NoNotices => "no_completion_notices",
            Self::Reason => "reason_not_supported",
        }
    }
}

/// Why no call on this route can hand back, or `None` when its calls may.
///
/// A route qualifies when its consumer opted in, it receives task completion
/// notices (so a handed-back command's result still reaches the session), and
/// it is a head agent's route. A delegated worker's or a codemode run's route
/// keeps holding its calls as before: the contract leaves scopes to the
/// provider, and those callers have no session that a notice could wake.
pub(super) fn route_refusal(identity: &super::RouteIdentity) -> Option<NotYieldable> {
    if !identity.call_yield {
        return Some(NotYieldable::NotNegotiated);
    }
    if !identity.trust.allows_bash_observation() {
        return Some(NotYieldable::NoNotices);
    }
    let head = identity
        .scope
        .as_ref()
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Head);
    if !head || identity.run_owner.is_some() || identity.run_scope.get().is_some() {
        return Some(NotYieldable::Scope);
    }
    None
}

/// The bare answer to `tool.yield`; never a route error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum YieldAnswer {
    /// The call's final frame follows within five seconds, the contract's
    /// deadline for an accepted yield.
    Accepted,
    NotYieldable(NotYieldable),
    /// No open call under that key for this carrier on this route: never
    /// admitted, already answered, or already handed back.
    UnknownCall,
}

impl YieldAnswer {
    pub(super) fn body(self) -> Value {
        let answer = match self {
            Self::Accepted => WireAnswer::Accepted,
            Self::NotYieldable(reason) => WireAnswer::NotYieldable {
                reason: Some(reason.code().to_owned()),
            },
            Self::UnknownCall => WireAnswer::UnknownCall,
        };
        serde_json::to_value(answer).expect("yield answers are JSON")
    }
}

/// The yield signals of the `bash_watch` calls waiting right now, by request.
///
/// The watch's own thread holds the only strong reference, so an entry stops
/// upgrading once that watch has ended; dead entries are pruned whenever a
/// new watch registers.
#[derive(Default)]
pub(super) struct WatchYields {
    watches: Mutex<HashMap<(RouteChannel, u64), Weak<crate::commands::bash_watch::YieldSignal>>>,
}

impl WatchYields {
    /// A new signal for the watch answering request `corr` on `route`.
    pub(super) fn register(
        &self,
        route: RouteChannel,
        corr: u64,
    ) -> Arc<crate::commands::bash_watch::YieldSignal> {
        let signal = Arc::new(crate::commands::bash_watch::YieldSignal::default());
        let mut watches = self.lock();
        watches.retain(|_, watch| watch.strong_count() > 0);
        watches.insert((route, corr), Arc::downgrade(&signal));
        signal
    }

    /// Ask the watch answering `corr` on `route` to end now. False when no
    /// such watch is still waiting, or it was already asked.
    pub(super) fn request(&self, route: RouteChannel, corr: u64) -> bool {
        let signal = self.lock().get(&(route, corr)).and_then(Weak::upgrade);
        signal.is_some_and(|signal| signal.request())
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        HashMap<(RouteChannel, u64), Weak<crate::commands::bash_watch::YieldSignal>>,
    > {
        self.watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The role crate's shared `call-yield.json` vectors: every request the
    /// contract lists is accepted or refused naming the same field.
    #[test]
    fn requests_follow_the_shared_vectors() {
        let vectors: Value =
            serde_json::from_str(cortexkit_role_tool_provider::vectors::CALL_YIELD_JSON).unwrap();
        let requests = vectors["requests"].as_array().unwrap();
        assert!(!requests.is_empty());
        for case in requests {
            let parsed = parse_request(&case["request"]);
            if case["valid"] == true {
                let parsed = parsed.unwrap_or_else(|error| panic!("{case}: {error:?}"));
                assert_eq!(parsed.call_key, case["request"]["arguments"]["call_key"]);
                assert_eq!(parsed.reason, case["request"]["arguments"]["reason"]);
            } else {
                let error = parsed.unwrap_err();
                assert_eq!(error.code, "invalid_request", "{case}");
                assert_eq!(error.detail.unwrap()["field"], case["field"], "{case}");
            }
        }
        // A body that is not a request at all is refused naming `arguments`.
        let error = parse_request(&json!({"name": "tool.yield"})).unwrap_err();
        assert_eq!(error.detail.unwrap()["field"], "arguments");
    }

    #[test]
    fn answers_have_the_contract_shapes() {
        assert_eq!(YieldAnswer::Accepted.body(), json!({"answer": "accepted"}));
        assert_eq!(
            YieldAnswer::UnknownCall.body(),
            json!({"answer": "unknown_call"})
        );
        assert_eq!(
            YieldAnswer::NotYieldable(NotYieldable::Tool).body(),
            json!({"answer": "not_yieldable", "reason": "tool_not_yieldable"})
        );
    }

    #[test]
    fn opting_in_needs_all_three_consumer_capabilities() {
        let caps = |list: &[&str]| list.iter().map(|cap| cap.to_string()).collect::<Vec<_>>();
        assert!(consumer_reads_call_yield(Some(&caps(&[
            "tool-result/v1",
            "held-reply/v1",
            "call-yield/v1"
        ]))));
        for missing in [
            caps(&["held-reply/v1", "call-yield/v1"]),
            caps(&["tool-result/v1", "call-yield/v1"]),
            caps(&["tool-result/v1", "held-reply/v1"]),
        ] {
            assert!(!consumer_reads_call_yield(Some(&missing)), "{missing:?}");
        }
        assert!(!consumer_reads_call_yield(None));
    }

    #[test]
    fn a_watch_is_asked_once_and_only_while_it_waits() {
        let yields = WatchYields::default();
        let route = super::super::route_key(1, 1);
        let signal = yields.register(route, 7);
        assert!(!yields.request(route, 8), "no watch answers corr 8");
        assert!(yields.request(route, 7));
        assert!(signal.requested());
        assert!(!yields.request(route, 7), "a watch is asked only once");
        drop(signal);
        let _other = yields.register(route, 9);
        assert!(!yields.request(route, 7), "an ended watch is not waiting");
    }
}
