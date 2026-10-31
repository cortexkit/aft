//! The shim's side of the governed-write relay.
//!
//! A governed `gh` write no longer goes to prefrontal's `gh.route`. The shim
//! opens a management route to the AFT daemon and calls the relay operation
//! with the per-command ticket the daemon put in its environment. The daemon
//! redeems the ticket to the agent session that ran the command, mints an
//! assertion for it, and returns plexus's reply unchanged; this module turns
//! that reply into output, a retry, or a named refusal.
//!
//! Every path fails closed: nothing here can reach upstream `gh`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_client_rs::{CallError, CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};

use super::{
    gh_session_id, governed_seam_state, governed_wire_request, project_root_for,
    render_applied_state, render_governed_response, seam_state, write_last_probe_silently,
    write_seam_state, AgentBinding, GovernedRequest, LastProbeReport, LastSeamRefusal, Manifest,
    ProbeStage, RouteOutcome, RungRecord, SeamState, StatePaths,
};
use crate::gh_shim_relay::{
    facade_refusal_code, facade_reply, BINDINGS_READ_OPERATION, BOT_REQUEST_OPERATION,
};

/// Refusal text when the running daemon does not serve the relay yet.
pub(super) const RELAY_UNSERVED_TEXT: &str = "the running AFT daemon does not serve the gh shim relay (gh_shim.bot_request); restart the AFT daemon on this aft version - this repository's actions are identity-governed, so the command was not run";
const RELAY_MODULE_ID: &str = "aft";
/// Budget for connecting, listing the catalog and opening the route.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for the whole governed exchange, including preflight and cleanup
/// around the daemon's 160 s post-send write wait. Retries share this ceiling.
const RELAY_BUDGET: Duration = Duration::from_secs(210);
// A management dispatch still has up to 9.75 s of daemon preflight before
// Plexus's 160 s reply wait, followed by at most 5 s of route cleanup.
const MIN_WRITE_DISPATCH_BUDGET: Duration = Duration::from_secs(175);
const READ_RELAY_BUDGET: Duration = Duration::from_secs(30);
/// Debug builds only: lets the binary-level tests shorten the relay budget
/// so a silent daemon does not cost them the full production wait.
#[cfg(debug_assertions)]
const REQUEST_TIMEOUT_TEST_ENV: &str = "AFT_GH_SHIM_RELAY_TIMEOUT_MS";
/// Retries of a transient refusal, each reusing the same request nonce.
const TRANSIENT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(10)];
const BINDINGS_CHECK_FILE: &str = "relay-bindings-check.json";

/// Everything the relay needs from the process, gathered once so tests can
/// supply a fake daemon and ticket without touching the environment.
pub(super) struct RelayContext {
    pub(super) worker_broker: Option<
        Result<crate::gh_shim_worker_broker::Client, crate::gh_shim_worker_broker::ClientError>,
    >,
    pub(super) connection_file: Option<PathBuf>,
    pub(super) ticket: Option<String>,
    pub(super) transient_delays: [Duration; 2],
    /// Budget for the whole relayed exchange, retries included.
    pub(super) relay_budget: Duration,
}

impl RelayContext {
    pub(super) fn from_process() -> Self {
        let worker = std::env::var_os(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV).is_some()
            || std::env::var_os(crate::gh_shim_worker_broker::SOCKET_ENV).is_some()
            || std::env::var_os(crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV).is_some();
        Self {
            worker_broker: worker.then(crate::gh_shim_worker_broker::Client::from_process),
            connection_file: if worker {
                None
            } else {
                super::configured_connection_file()
            },
            ticket: if worker {
                None
            } else {
                std::env::var(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV).ok()
            },
            transient_delays: TRANSIENT_RETRY_DELAYS,
            relay_budget: relay_budget_from_process(),
        }
    }
}

fn relay_budget_from_process() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(millis) = std::env::var(REQUEST_TIMEOUT_TEST_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
    {
        return Duration::from_millis(millis);
    }
    RELAY_BUDGET
}

/// `aft` when the catalog shows it serving the relay operation. No other
/// module may stand in for it: the relay carries a ticket and bot speech.
pub(super) fn relay_holder(entries: &[subc_client_rs::CatalogEntry]) -> Option<String> {
    entries
        .iter()
        .find(|entry| {
            entry.module_id == RELAY_MODULE_ID
                && entry.roles.iter().any(|role| {
                    matches!(
                        role,
                        ProviderRole::ManagementSurface { operations, .. }
                            if operations.iter().any(|operation| operation.name == BOT_REQUEST_OPERATION)
                    )
                })
        })
        .map(|entry| entry.module_id.clone())
}

/// Legacy retry policy for Plexus replies without retry metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodeClass {
    /// Resend with the same nonce after 5 s, then 10 s.
    Transient,
    /// Resend once; the daemon has dropped its cached assertion and mints anew.
    Remint,
    /// Plexus cannot tell whether the write happened. Never resend.
    OutcomeUnknown,
    /// The request itself is refused; name the code.
    Terminal,
    /// Plexus is not set up to accept this write; name it as configuration.
    PlexusSetup,
    /// A code this shim does not know; surface it, never retry.
    Unknown,
}

/// The code without any `{...}` detail plexus appended.
fn code_prefix(code: &str) -> &str {
    code.split('{').next().unwrap_or(code).trim()
}

fn code_detail(code: &str) -> &str {
    code.find('{').map(|start| &code[start..]).unwrap_or("")
}

pub(super) fn classify_code(code: &str) -> CodeClass {
    match code_prefix(code) {
        "nonce_in_flight"
        | "module_warming"
        | "assertion_key_unavailable"
        | "store_failure"
        | "pre_dispatch_read_incomplete" => CodeClass::Transient,
        // A credential that could not be resolved is worth a retry only when
        // plexus calls it transient or gives no class at all.
        "handle_credential_unresolvable" => {
            let detail = code_detail(code);
            if !detail.contains("class") || detail.contains("transient") {
                CodeClass::Transient
            } else {
                CodeClass::Unknown
            }
        }
        "assertion_expired" | "assertion_not_yet_valid" | "assertion_binding_generation_stale" => {
            CodeClass::Remint
        }
        "outcome_unknown" => CodeClass::OutcomeUnknown,
        "agent_repository_conflict"
        | "repository_unbound"
        | "request_kind_unmapped"
        | "request_field_forbidden"
        | "comment_author_mismatch"
        | "issue_author_mismatch"
        | "issue_edit_not_own"
        | "not_present"
        | "vendor_rejected"
        | "nonce_payload_mismatch"
        | "policy_blocked"
        | "agent_grant_absent"
        | "agent_grant_excludes_request" => CodeClass::Terminal,
        "module_grant_absent"
        | "agent_handle_unknown"
        | "agent_handle_mismatch"
        | "handle_disabled"
        | "probe_unproven"
        | "assertion_scope_absent"
        | "assertion_signature_invalid"
        | "assertion_malformed"
        | "assertion_wrong_surface" => CodeClass::PlexusSetup,
        _ => CodeClass::Unknown,
    }
}

/// Read one `key: value` (or `"key": "value"`) pair out of a code's detail.
fn detail_field(detail: &str, key: &str) -> Option<String> {
    let start = detail.find(key)? + key.len();
    let rest = detail[start..].trim_start_matches(['"', ' ']);
    let rest = rest.strip_prefix([':', '='])?;
    let value: String = rest
        .trim_start_matches([' ', '"'])
        .chars()
        .take_while(|c| !matches!(c, ',' | '}' | '"'))
        .collect();
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[derive(Clone, Copy)]
enum GithubReach {
    Never,
    ReadsOnly,
    Maybe,
    Yes,
}

#[derive(Clone, Copy)]
enum RefusalActor {
    Agent,
    Operator,
    Nobody,
}

// More specific detail selectors precede the fallback for the same code.
// Transmission includes reads, not just writes. A later gate can follow sent
// steps, so phase-dependent codes must not promise that nothing reached GitHub.
// Read-only gates stop before writes; sent-step evidence still takes precedence.
const REFUSAL_EXPLANATIONS: &[(&str, &str, &str, GithubReach, RefusalActor)] = &[
    ("agent_grant_absent", "", "The agent has no grant for this write.", GithubReach::Maybe, RefusalActor::Operator),
    ("agent_grant_excludes_request", "", "The agent's grant does not cover this request.", GithubReach::Never, RefusalActor::Operator),
    ("agent_handle_unknown", "", "The agent's GitHub bot handle is unknown.", GithubReach::Never, RefusalActor::Operator),
    ("agent_repository_conflict", "claimed_agent=*", "This command speaks as agent {claimed_agent}, but the repository is bound to agent {bound_agent}.", GithubReach::Never, RefusalActor::Operator),
    ("agent_repository_conflict", "", "The GitHub target does not match the bound repository.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("argument_undeclared", "", "The request contains an unsupported identity field.", GithubReach::Never, RefusalActor::Agent),
    ("approval_agent_mismatch", "", "The approval belongs to another agent.", GithubReach::Never, RefusalActor::Agent),
    ("approval_denied", "", "Approval for this write was denied.", GithubReach::Never, RefusalActor::Operator),
    ("approval_expired", "", "The approval has expired.", GithubReach::Never, RefusalActor::Agent),
    ("call_mode_unsupported", "", "This call mode cannot make bot writes.", GithubReach::Never, RefusalActor::Agent),
    ("comment_author_mismatch", "", "The comment was not written by the expected bot.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("connector_home_not_held", "", "This machine does not hold permission to send GitHub requests.", GithubReach::Never, RefusalActor::Operator),
    ("consent_unavailable", "", "This flow cannot answer the required consent prompt.", GithubReach::Never, RefusalActor::Operator),
    ("delegating_scope_required", "", "The route has no scope authorized to act as an agent.", GithubReach::Never, RefusalActor::Agent),
    ("flow_audience_conflict", "", "The flow's approved repository sets conflict.", GithubReach::Never, RefusalActor::Operator),
    ("flow_not_approved", "", "The flow is not approved.", GithubReach::Never, RefusalActor::Operator),
    ("flow_unknown", "", "The flow is unknown.", GithubReach::Never, RefusalActor::Operator),
    ("flow_audience_excludes_repository", "", "The repository is outside the flow's approved set.", GithubReach::Never, RefusalActor::Agent),
    ("flow_audience_mismatch", "", "The flow requires a matching agent-scoped route.", GithubReach::Never, RefusalActor::Agent),
    ("flow_audience_unavailable", "", "The flow's approved repository set could not be read.", GithubReach::Never, RefusalActor::Operator),
    ("flow_carrier_not_admitted", "", "This caller is not allowed to carry flow requests.", GithubReach::Never, RefusalActor::Agent),
    ("flow_scope_required", "", "The route has no required flow scope.", GithubReach::Never, RefusalActor::Agent),
    ("handle_credential_rejected", "", "GitHub rejected the bot's credential.", GithubReach::Yes, RefusalActor::Operator),
    ("handle_credential_unresolvable", "class=transient", "The bot's credential is temporarily unavailable.", GithubReach::Maybe, RefusalActor::Nobody),
    ("handle_credential_unresolvable", "", "The bot's credential could not be loaded.", GithubReach::Maybe, RefusalActor::Operator),
    ("handle_disabled", "", "The bot handle is disabled.", GithubReach::Never, RefusalActor::Operator),
    ("invalid_request", "", "The request has an invalid field or shape.", GithubReach::Never, RefusalActor::Agent),
    ("module_grant_absent", "", "The calling module has no grant for this write.", GithubReach::Maybe, RefusalActor::Operator),
    ("module_warming", "", "The module is still starting.", GithubReach::Never, RefusalActor::Nobody),
    ("nonce_in_flight", "", "Another worker is still handling this request nonce.", GithubReach::Maybe, RefusalActor::Nobody),
    ("nonce_payload_mismatch", "", "This nonce already belongs to a different request.", GithubReach::Maybe, RefusalActor::Agent),
    ("not_present", "", "The GitHub target was not found.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("outcome_unknown", "", "The GitHub read outcome is unknown.", GithubReach::Maybe, RefusalActor::Operator),
    ("pipeline_denied", "denial=RecipientArgumentInvalid", "A permission or service check stopped the current step.", GithubReach::Maybe, RefusalActor::Agent),
    ("pipeline_denied", "denial=ArgumentUndeclared", "A permission or service check stopped the current step.", GithubReach::Maybe, RefusalActor::Agent),
    ("pipeline_denied", "denial=ResourceFilter(MissingRequiredFilter)", "A permission or service check stopped the current step.", GithubReach::Maybe, RefusalActor::Agent),
    ("pipeline_denied", "denial=ResourceFilter(ValueNotAllowed)", "A permission or service check stopped the current step.", GithubReach::Maybe, RefusalActor::Agent),
    ("pipeline_denied", "denial=ResourceFilter(WriteFiltersAbsent)", "A permission or service check stopped the current step.", GithubReach::Maybe, RefusalActor::Agent),
    ("pipeline_denied", "", "A permission or service check stopped the current step.", GithubReach::Maybe, RefusalActor::Operator),
    ("policy_blocked", "", "Policy blocks this write.", GithubReach::Never, RefusalActor::Operator),
    ("pre_dispatch_read_incomplete", "reason=page_bound", "The GitHub read exceeded its page limit.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("pre_dispatch_read_incomplete", "reason=invalid_response", "The GitHub read response could not be used.", GithubReach::Maybe, RefusalActor::Nobody),
    ("pre_dispatch_read_incomplete", "", "The GitHub read did not finish.", GithubReach::Maybe, RefusalActor::Nobody),
    ("pr_edit_not_own", "", "The pull request was not opened by this bot.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("review_thread_not_own", "", "The pull request was not opened by this bot.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("issue_edit_not_own", "", "The issue was not opened by this bot.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("pr_head_cross_repository", "", "The pull request head is outside the bound repository.", GithubReach::Never, RefusalActor::Agent),
    ("pull_request_head_moved", "", "The pull request's head changed after the commit given with --match-head-commit; read its current head and enqueue again with that commit.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("pull_request_not_open", "", "The pull request is closed or merged, so it can't join or leave the merge queue.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("probe_unproven", "", "The bot handle has not passed its proof check.", GithubReach::Never, RefusalActor::Operator),
    ("repository_unbound", "", "The repository has no bot binding.", GithubReach::Maybe, RefusalActor::Operator),
    ("request_deadline_exceeded", "", "The request deadline expired before dispatch.", GithubReach::Never, RefusalActor::Nobody),
    ("request_field_forbidden", "", "The request contains a forbidden field.", GithubReach::Never, RefusalActor::Agent),
    ("request_kind_unmapped", "", "This request action or shape is not supported.", GithubReach::Never, RefusalActor::Agent),
    ("route_not_bound", "", "The route is not bound to GitHub.", GithubReach::Never, RefusalActor::Operator),
    ("run_scope_not_child", "", "The run scope is not a live child of the route scope.", GithubReach::Maybe, RefusalActor::Agent),
    ("scope_ended", "", "The agent's route scope has ended.", GithubReach::Maybe, RefusalActor::Operator),
    ("delegation_withdrawn", "", "Permission to act as this agent was withdrawn.", GithubReach::Maybe, RefusalActor::Operator),
    ("scope_not_synced", "", "The agent's scope has not synced yet.", GithubReach::Maybe, RefusalActor::Operator),
    ("scope_unverifiable", "", "The agent's scope could not be verified.", GithubReach::Maybe, RefusalActor::Operator),
    ("secret_input_rejected", "", "The request contains credential material that is not allowed here.", GithubReach::Never, RefusalActor::Agent),
    ("stale_authority", "dimension=expected_handle", "The expected bot handle has changed.", GithubReach::Never, RefusalActor::Agent),
    ("stale_authority", "dimension=expected_binding", "The expected repository binding has changed.", GithubReach::Never, RefusalActor::Agent),
    ("stale_authority", "", "The write's authorization changed or expired.", GithubReach::Maybe, RefusalActor::Operator),
    ("stack_pr_not_own", "", "A stacked pull request was not opened by this bot.", GithubReach::ReadsOnly, RefusalActor::Agent),
    ("store_failure", "", "Request storage failed.", GithubReach::Maybe, RefusalActor::Operator),
    ("transport_not_sent", "", "The current step was not sent.", GithubReach::Maybe, RefusalActor::Operator),
    ("vendor_rejected", "status=429", "GitHub rejected the request.", GithubReach::Yes, RefusalActor::Nobody),
    ("vendor_rejected", "status=503", "GitHub rejected the request.", GithubReach::Yes, RefusalActor::Nobody),
    ("vendor_rejected", "", "GitHub rejected the request.", GithubReach::Yes, RefusalActor::Agent),
    ("agent_identity_conflict", "", "The repository is bound to another agent's bot.", GithubReach::Never, RefusalActor::Agent),
    ("bot_write_requires_scope", "", "This session has no agent scope for bot writes.", GithubReach::Never, RefusalActor::Operator),
    ("ticket_unknown", "", "This command's agent session ticket is no longer live.", GithubReach::Never, RefusalActor::Agent),
    ("untrusted_principal", "", "Only first-party callers may relay bot writes.", GithubReach::Never, RefusalActor::Operator),
];

#[derive(Debug, Default, PartialEq)]
pub(super) struct RefusalFacts {
    retry_same_nonce: Option<bool>,
    legs_sent: Vec<Value>,
    not_sent: bool,
    detail_message: Option<String>,
}

impl RefusalFacts {
    fn from_reply(refusal: &Value) -> Self {
        Self {
            retry_same_nonce: refusal.get("retry_same_nonce").and_then(Value::as_bool),
            legs_sent: refusal
                .get("legs_sent")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            not_sent: refusal.get("transmission").and_then(Value::as_str) == Some("not_sent"),
            detail_message: refusal
                .pointer("/detail/message")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }
}

fn refusal_text(origin: &str, code: &str, facts: &RefusalFacts, budget_ms: Option<u64>) -> String {
    let explanation = REFUSAL_EXPLANATIONS.iter().find(|(name, selector, ..)| {
        *name == code_prefix(code)
            && (selector.is_empty()
                || selector.split_once('=').is_some_and(|(key, value)| {
                    detail_field(code_detail(code), key)
                        .is_some_and(|actual| value == "*" || actual == value)
                }))
    });
    let mut text = format!("{origin} refused the bot write: {code}. ");
    if let Some(message) = facts
        .detail_message
        .as_deref()
        .filter(|_| origin == "Plexus")
    {
        text.push_str(message);
    } else if let Some((_, _, sentence, _, _)) = explanation {
        let mut sentence = sentence.to_string();
        for key in ["claimed_agent", "bound_agent"] {
            sentence = sentence.replace(
                &format!("{{{key}}}"),
                &detail_field(code_detail(code), key).unwrap_or_else(|| "(unnamed)".to_string()),
            );
        }
        text.push_str(&sentence);
    } else {
        text.push_str("This shim does not recognize the refusal code.");
    }
    let reach = if !facts.legs_sent.is_empty() {
        Some(GithubReach::Yes)
    } else if facts.not_sent {
        Some(GithubReach::Never)
    } else {
        explanation.map(|(_, _, _, reach, _)| *reach)
    };
    text.push_str(match reach {
        Some(GithubReach::Never) => " Nothing reached GitHub.",
        Some(GithubReach::ReadsOnly) => " GitHub received only reads; nothing was written.",
        Some(GithubReach::Maybe) => " Earlier reads or writes may have reached GitHub.",
        Some(GithubReach::Yes) => " GitHub received a request.",
        None => " Whether anything reached GitHub is unknown.",
    });
    if !facts.legs_sent.is_empty() {
        let steps: Vec<_> = facts
            .legs_sent
            .iter()
            .map(|step| {
                step.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| step.to_string())
            })
            .collect();
        text.push_str(&format!(" Already-sent write steps: {}.", steps.join(", ")));
    }
    let actor = explanation.map(|(_, _, _, _, actor)| *actor);
    if let Some(actor) = actor {
        text.push_str(match actor {
            RefusalActor::Agent => " The agent must change the request.",
            RefusalActor::Operator => " An operator must fix the setup, grants, or policy.",
            RefusalActor::Nobody => "",
        });
    }
    if let Some(budget_ms) = budget_ms {
        text.push_str(&format!(
            " The relay's {budget_ms} ms budget cannot fit another retry."
        ));
    }
    text.push_str(match (reach, actor) {
        (Some(GithubReach::ReadsOnly), _) => "",
        (Some(GithubReach::Never), Some(RefusalActor::Nobody)) => " Rerunning the command is safe.",
        (Some(GithubReach::Never), Some(RefusalActor::Agent | RefusalActor::Operator)) => "",
        (Some(GithubReach::Maybe | GithubReach::Yes), _) => {
            " Check GitHub before rerunning; a rerun may repeat the steps that were sent."
        }
        _ => " Check GitHub before rerunning.",
    });
    text
}

fn plexus_refusal_text(code: &str, facts: &RefusalFacts) -> String {
    refusal_text("Plexus", code, facts, None)
}

fn outcome_undetermined_text(code: &str) -> String {
    format!(
        "the outcome could not be determined ({code}): the bot write may have executed; check before retrying (for comments: gh api repos/<owner>/<repo>/issues/<n>/comments --jq '.[-1]')"
    )
}

/// One decoded relay reply.
#[derive(Debug, PartialEq)]
pub(super) enum Reply {
    Completed {
        generation: Option<u64>,
        result: Value,
    },
    /// Refused by the daemon (ticket, mint, transport) or by plexus. `stage`
    /// is `plexus_reply` for plexus's own refusals.
    Refused {
        code: String,
        stage: String,
        message: String,
        generation: Option<u64>,
        facts: RefusalFacts,
    },
    /// If retries do not complete, module refusals keep their historical
    /// unknown-outcome exit while explaining the refusal itself.
    ModuleRefused {
        code: String,
        facts: RefusalFacts,
    },
    /// The daemon refused before forwarding to a module that is still starting.
    Warming {
        origin: &'static str,
    },
    Malformed(String),
}

pub(super) const PLEXUS_REPLY_STAGE: &str = "plexus_reply";

/// Decode the daemon's management reply `{op, status, data}`.
pub(super) fn parse_reply(bytes: &[u8]) -> Reply {
    let Ok(envelope) = serde_json::from_slice::<Value>(bytes) else {
        return Reply::Malformed("the relay reply was not JSON".to_string());
    };
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    match envelope.get("status").and_then(Value::as_str) {
        Some("ok") => {
            let reply = facade_reply(&data);
            if let Some(code) = reply
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
            {
                return Reply::ModuleRefused {
                    code: code.to_string(),
                    facts: RefusalFacts::from_reply(&reply["error"]),
                };
            }
            let generation = reply.get("repo_binding_generation").and_then(Value::as_u64);
            let result = reply.get("result").cloned().unwrap_or(Value::Null);
            if result["status"] == "refused"
                && result
                    .get("legs_sent")
                    .and_then(Value::as_array)
                    .is_some_and(|legs| {
                        legs.iter()
                            .any(|leg| leg.as_str().is_none_or(str::is_empty))
                    })
            {
                return Reply::Malformed("plexus reported posted legs without valid action names; check GitHub before retrying".into());
            }
            match result.get("status").and_then(Value::as_str) {
                Some("completed") => Reply::Completed {
                    generation,
                    result: result.get("result").cloned().unwrap_or(Value::Null),
                },
                Some("refused")
                    if result
                        .get("legs_sent")
                        .and_then(Value::as_array)
                        .is_some_and(|legs| !legs.is_empty())
                        && result.get("retry_same_nonce").and_then(Value::as_bool)
                            != Some(true) =>
                {
                    let legs = result["legs_sent"].as_array().expect("checked above");
                    // Only an explicit same-nonce retry guarantee can safely
                    // resume a write without repeating its completed steps.
                    Reply::Completed {
                        generation,
                        result: json!({
                            "status":"partial_posted",
                            "posted_legs":legs,
                            "refusal_code":result.get("refusal_code").or_else(||result.get("code")).cloned().unwrap_or(Value::Null),
                            "scope_role":result.get("scope_role").cloned().unwrap_or(Value::Null),
                        }),
                    }
                }
                Some("refused")
                    if crate::gh_shim_speech::partner::map_refusal(
                        &result,
                        crate::gh_shim_speech::ScopeRole::Authority,
                    )
                    .is_some() =>
                {
                    let refusal = crate::gh_shim_speech::partner::map_refusal(
                        &result,
                        crate::gh_shim_speech::ScopeRole::Authority,
                    )
                    .expect("checked above");
                    Reply::Refused {
                        code: refusal.code.as_str().into(),
                        stage: crate::gh_shim_speech::REFUSAL_STAGE.into(),
                        message: refusal.text(),
                        generation,
                        facts: RefusalFacts::from_reply(&result),
                    }
                }
                Some("refused") => match facade_refusal_code(&reply) {
                    Some(code) => Reply::Refused {
                        code: code.to_string(),
                        stage: PLEXUS_REPLY_STAGE.to_string(),
                        message: String::new(),
                        generation,
                        facts: RefusalFacts::from_reply(&result),
                    },
                    None => Reply::Malformed("plexus refused without a refusal_code".to_string()),
                },
                // New statuses need an explicit contract before they can authorize
                // success, refusal, or retry; never infer one from other fields.
                _ => {
                    let status = result.get("status").unwrap_or(&Value::Null);
                    log::warn!("gh-shim: unrecognised plexus result.status: {status}");
                    Reply::Malformed(format!(
                        "plexus's reply had no known result.status: {status}"
                    ))
                }
            }
        }
        Some("error") if data["refusal_code"] == "module_warming" && data["stage"] == "plexus" => {
            Reply::Warming { origin: "Plexus" }
        }
        Some("error") => match data.get("refusal_code").and_then(Value::as_str) {
            Some(code) => Reply::Refused {
                code: code.to_string(),
                stage: data
                    .get("stage")
                    .and_then(Value::as_str)
                    .unwrap_or("relay")
                    .to_string(),
                message: data
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                generation: None,
                facts: RefusalFacts::from_reply(&data),
            },
            None => Reply::Malformed("the relay refused without a refusal_code".to_string()),
        },
        _ => Reply::Malformed("the relay reply had no status".to_string()),
    }
}

/// Render a completed write the way upstream `gh` would where plexus says
/// enough: a speech verb prints its URL, and other fields go through the
/// governed renderer. Native close/reopen confirmations are rendered with
/// their request context by `render_thread_state_confirmation`. Plexus omits any field
/// GitHub did not return, so every field is optional; a reply with no detail
/// prints a plain completion line.
///
/// A multi-leg write can post a comment before its state change is stopped.
/// Such a result reports posted actions and must never look safe to retry.
pub(super) fn render_completed(result: &Value) -> Result<String, RouteOutcome> {
    let Some(object) = result.as_object().filter(|object| !object.is_empty()) else {
        return Ok("completed\n".to_string());
    };
    if object.get("status").and_then(Value::as_str) == Some("partial_posted") {
        let actions = object.get("posted_legs").cloned().unwrap_or(Value::Null);
        let code = object
            .get("refusal_code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let code: String = code.chars().flat_map(char::escape_default).collect();
        return Err(RouteOutcome::PartiallyApplied(format!(
            "Posted as the bot: {actions}.\nThe rest of this GitHub write did not finish ({code}). Check GitHub before trying again.\n"
        )));
    }
    if let Some(url) = object.get("url").and_then(Value::as_str) {
        return Ok(format!("{url}\n"));
    }
    if object.contains_key("state") {
        let mut output = render_applied_state(object)?;
        if let Some(url) = object
            .get("comment")
            .and_then(|comment| comment.get("url"))
            .and_then(Value::as_str)
        {
            output.push_str(url);
            output.push('\n');
        }
        return Ok(output);
    }
    let field_order: Vec<Value> = object.keys().map(|key| json!(key)).collect();
    render_governed_response(result, &field_order)
}

/// gh v2.86.0's issue/pr close/reopen commands print these confirmations on
/// stderr (pkg/cmd/{issue,pr}/{close,reopen}). The relay may return only state,
/// so use the already-canonical request for the repository and number. Include
/// a title only when GitHub returned one; do not invent it or make another read.
fn render_thread_state_confirmation(result: &Value, request: &Value) -> Option<String> {
    let (past, noun, segment) = match request.get("verb")?.as_str()? {
        "issue close" => ("Closed", "issue", "issues"),
        "issue reopen" => ("Reopened", "issue", "issues"),
        "pr close" => ("Closed", "pull request", "pull"),
        "pr reopen" => ("Reopened", "pull request", "pull"),
        _ => return None,
    };
    let repository = request.get("repository")?.as_str()?;
    let number = request.get("number")?;
    let number = if let Some(number) = number.as_str() {
        super::parse_thread_target(number, segment)?.1
    } else {
        number.as_u64()?
    };
    let title = result
        .get("title")
        .and_then(Value::as_str)
        .map(|title| format!(" ({title})"))
        .unwrap_or_default();
    Some(format!("✓ {past} {noun} {repository}#{number}{title}\n"))
}

fn daemon_refusal(code: &str, stage: &str, message: &str, facts: &RefusalFacts) -> RouteOutcome {
    if stage == crate::gh_shim_speech::REFUSAL_STAGE
        && crate::gh_shim_speech::Code::parse(code).is_some()
    {
        return RouteOutcome::RelayRefusal {
            code: code.to_string(),
            text: format!("{code}: {message}"),
        };
    }
    match code {
        "gh_shim_worker_write_refused"=>RouteOutcome::WorkerWriteRefused,
        "ticket_absent" => RouteOutcome::NoAgentSession,
        // These are AFT's own pre-dispatch gates, not Plexus receipt replies.
        "ticket_unknown" | "untrusted_principal" | "bot_write_requires_scope" | "agent_identity_conflict" => RouteOutcome::RelayRefusal {
            code: code.to_string(),
            text: refusal_text("AFT", code, facts, None),
        },
        "relay_unavailable" => RouteOutcome::Unavailable(format!(
            "the AFT daemon could not reach {} to relay the bot write, so nothing was sent: {message}",
            if stage == "mint" { "prefrontal" } else { "plexus" }
        )),
        "outcome_unknown" => RouteOutcome::OutcomeUndetermined(outcome_undetermined_text(code)),
        "request_malformed" | "mint_reply_malformed" => {
            RouteOutcome::SchemaMismatch(format!("{code}: {message}"))
        }
        _ if stage == "mint" => RouteOutcome::RelayRefusal {
            code: code.to_string(),
            text: format!(
                "prefrontal refused to mint an agent assertion for this command's session: {code}{}",
                if message.is_empty() { String::new() } else { format!(" ({message})") }
            ),
        },
        _ => {
            RouteOutcome::RelayRefusal {
                code: code.to_string(),
                text: refusal_text(if stage == "plexus" { "Plexus" } else { "AFT" }, code, facts, None),
            }
        }
    }
}

/// What the shim last confirmed about plexus's repository bindings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct BindingsCheck {
    pub(super) manifest_version: u64,
    pub(super) repo_binding_generation: u64,
    /// Handles already returned by the normal bindings check; never fetched
    /// just to improve a refusal. Older cached checks have no handle detail.
    #[serde(default)]
    pub(super) handles: BTreeMap<String, String>,
}

fn load_check(paths: &StatePaths) -> Option<BindingsCheck> {
    serde_json::from_slice(&std::fs::read(paths.root.join(BINDINGS_CHECK_FILE)).ok()?).ok()
}

fn store_check(paths: &StatePaths, check: &BindingsCheck) {
    let _ = crate::private_storage::open_root(&paths.root);
    if let Ok(bytes) = serde_json::to_vec(check) {
        let _ = crate::private_storage::write(paths.root.join(BINDINGS_CHECK_FILE), bytes);
    }
}

fn clear_check(paths: &StatePaths) {
    let _ = std::fs::remove_file(paths.root.join(BINDINGS_CHECK_FILE));
}

fn render_bindings(bindings: &BTreeMap<String, String>) -> String {
    if bindings.is_empty() {
        return "(none)".to_string();
    }
    bindings
        .iter()
        .map(|(repository, agent)| format!("{repository}->{agent}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compare plexus's bindings read with the manifest's governed repositories.
/// They must agree exactly: a repository plexus binds but the manifest does
/// not would otherwise pass through to upstream `gh` unguarded.
pub(super) fn compare_bindings(
    manifest: &BTreeMap<String, String>,
    reply: &Value,
) -> Result<u64, String> {
    // plexus answers bindings.read as `{"result": {...}}` or `{"error": {"code"}}`;
    // accept the bare body too, so a reply without the wrapper still parses.
    if let Some(code) = reply
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
    {
        return Err(format!("plexus refused the bindings read: {code}"));
    }
    let reply = reply
        .get("result")
        .filter(|result| result.get("repo_binding_generation").is_some())
        .unwrap_or(reply);
    let generation = reply
        .get("repo_binding_generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| "plexus's bindings read had no repo_binding_generation".to_string())?;
    let rows = reply
        .get("bindings")
        .and_then(Value::as_array)
        .ok_or_else(|| "plexus's bindings read had no bindings list".to_string())?;
    let mut plexus = BTreeMap::new();
    for row in rows {
        let (Some(repository), Some(agent)) = (
            row.get("repository").and_then(Value::as_str),
            row.get("agent_id").and_then(Value::as_str),
        ) else {
            return Err("plexus's bindings read had a malformed row".to_string());
        };
        plexus.insert(repository.to_ascii_lowercase(), agent.to_string());
    }
    let manifest_view: BTreeMap<String, String> = manifest
        .iter()
        .map(|(repository, agent)| (repository.to_ascii_lowercase(), agent.clone()))
        .collect();
    // Every repository plexus binds must be governed by the manifest, bound to
    // the same agent: a repository plexus binds but the manifest does not would
    // otherwise reach upstream `gh` on operator credentials, and a disagreement
    // on the agent means the two views describe different bots. The manifest
    // may govern repositories plexus does not bind (plexus binds one repository
    // per agent, the manifest can bind several); a write to one of those is
    // refused by plexus as `repository_unbound`, which fails closed.
    let disagreements = plexus
        .iter()
        .filter(|(repository, agent)| manifest_view.get(*repository) != Some(*agent))
        .map(|(repository, agent)| (repository.clone(), agent.clone()))
        .collect::<BTreeMap<_, _>>();
    if disagreements.is_empty() {
        return Ok(generation);
    }
    Err(format!(
        "plexus's repository bindings (generation {generation}) disagree with this shim's manifest for {}; manifest: {}; plexus: {}; governed writes stay refused until they agree",
        render_bindings(&disagreements),
        render_bindings(&manifest_view),
        render_bindings(&plexus),
    ))
}

fn new_nonce() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // A clock-derived fallback is still unique per invocation, which is all
        // the nonce needs; it is not a secret.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        bytes = (nanos ^ u128::from(std::process::id())).to_be_bytes();
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("gh-shim-{hex}")
}

#[cfg(test)]
thread_local! {
    static ATTEMPT_DEADLINES: std::cell::RefCell<Option<Vec<Instant>>> = const { std::cell::RefCell::new(None) };
}

/// The exchange uses one monotonic clock for requests and retry sleeps. Tests
/// can advance that clock without depending on daemon or runner scheduling.
trait ExchangeIo {
    fn now(&self) -> Instant;
    async fn sleep(&self, delay: Duration);
    async fn request(&self, bytes: Vec<u8>, options: CallOptions) -> Result<Vec<u8>, CallError>;
}

struct SubcExchangeIo<'a> {
    consumer: &'a SubcConsumer,
    route: &'a subc_client_rs::RouteHandle,
    #[cfg(test)]
    clock: Option<TestExchangeClock>,
}

impl ExchangeIo for SubcExchangeIo<'_> {
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return clock.now();
        }
        Instant::now()
    }

    async fn sleep(&self, delay: Duration) {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            clock.advance(delay);
            return;
        }
        tokio::time::sleep(delay).await;
    }

    async fn request(&self, bytes: Vec<u8>, options: CallOptions) -> Result<Vec<u8>, CallError> {
        #[cfg(test)]
        let sent = Instant::now();
        let result = self.consumer.request(self.route, bytes, options).await;
        #[cfg(test)]
        if let (Some(clock), Err(_)) = (&self.clock, &result) {
            clock.advance(sent.elapsed());
        }
        #[cfg(test)]
        if result.is_ok() {
            TEST_EXCHANGE_REPLY_DELAY.with(|delay| {
                let delay = delay.replace(Duration::ZERO);
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            });
        }
        result
    }
}

#[cfg(test)]
thread_local! {
    /// Set by a test to run [`route`]'s exchange on a [`TestExchangeClock`]
    /// on this thread, still against the real subc consumer.
    static USE_TEST_EXCHANGE_CLOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Pause after a successful reply to model a loaded runner between exchange steps.
    static TEST_EXCHANGE_REPLY_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

/// An exchange clock for tests that drive the real subc consumer, so how
/// long the runner takes to deliver an answered reply cannot decide whether
/// a retry fits in the budget. A reply that arrives takes no time on it; a
/// retry sleep moves it by the sleep without waiting; a request that fails,
/// such as one the consumer gave up on at its timeout, moves it by the real
/// time the consumer waited. It starts at the first reading, which is after
/// the exchange's start.
#[cfg(test)]
struct TestExchangeClock {
    now: std::cell::Cell<Option<Instant>>,
}

#[cfg(test)]
impl TestExchangeClock {
    fn for_this_thread() -> Option<Self> {
        USE_TEST_EXCHANGE_CLOCK
            .with(std::cell::Cell::get)
            .then(|| Self {
                now: std::cell::Cell::new(None),
            })
    }

    fn now(&self) -> Instant {
        let now = self.now.get().unwrap_or_else(Instant::now);
        self.now.set(Some(now));
        now
    }

    fn advance(&self, by: Duration) {
        self.now.set(Some(self.now() + by));
    }
}

fn lock_probe_stage(stage: &Mutex<ProbeStage>) -> std::sync::MutexGuard<'_, ProbeStage> {
    // ProbeStage records diagnostic progress only; it never authorizes or settles a call.
    // Its stored variant remains valid after a panic and can be read or replaced safely.
    stage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Exchange<'a, Io> {
    io: &'a Io,
    ticket: &'a str,
    stage: &'a Mutex<ProbeStage>,
    /// When the exchange began; `budget` after it is the exchange's deadline.
    started: Instant,
    budget: Duration,
}

#[derive(Debug)]
enum SendFailure {
    Outcome(RouteOutcome),
    Module { code: String, message: String },
}

impl SendFailure {
    fn into_outcome(self) -> RouteOutcome {
        match self {
            Self::Outcome(outcome) => outcome,
            Self::Module { code, .. } if code == "module_warming" => RouteOutcome::RelayRefusal {
                text: refusal_text(
                    "AFT",
                    &code,
                    &RefusalFacts {
                        not_sent: true,
                        ..RefusalFacts::default()
                    },
                    None,
                ),
                code,
            },
            Self::Module { code, message } => RouteOutcome::RelayRefusal {
                text: format!("the AFT daemon refused the relay request: {code}: {message}"),
                code,
            },
        }
    }
}

impl<Io: ExchangeIo> Exchange<'_, Io> {
    async fn retry_warming(&self, delays: &[Duration; 2], retries: &mut usize) -> bool {
        let Some(delay) = delays.get(*retries) else {
            return false;
        };
        if self.io.now() + *delay >= self.deadline() {
            return false;
        }
        self.io.sleep(*delay).await;
        *retries += 1;
        true
    }

    async fn check_bindings_with_warming_retry(
        &self,
        agent_id: &str,
        manifest: &Manifest,
        delays: &[Duration; 2],
        retries: &mut usize,
    ) -> Result<BindingsCheck, RouteOutcome> {
        loop {
            match self.check_bindings(agent_id, manifest).await {
                Err(RouteOutcome::RelayRefusal { code, .. })
                    if code == "module_warming" && self.retry_warming(delays, retries).await => {}
                result => return result,
            }
        }
    }

    fn deadline(&self) -> Instant {
        self.started + self.budget
    }

    fn budget_ms(&self) -> u64 {
        self.budget.as_millis() as u64
    }

    /// Each attempt gets only what is left of the exchange's budget. With
    /// nothing left the request is not sent at all, which is a known
    /// not-sent outcome rather than an unknown one.
    async fn send(&self, body: &Value) -> Result<Vec<u8>, SendFailure> {
        *lock_probe_stage(self.stage) = ProbeStage::Request;
        let bytes = serde_json::to_vec(body).map_err(|error| {
            SendFailure::Outcome(RouteOutcome::SchemaMismatch(error.to_string()))
        })?;
        let now = self.io.now();
        let remaining = self.deadline().saturating_duration_since(now);
        // Explicit debug/test budget overrides deliberately allow shorter waits.
        // Production must not enqueue a write that cannot get its full reply window.
        if body["op"] == BOT_REQUEST_OPERATION
            && self.budget == RELAY_BUDGET
            && remaining < MIN_WRITE_DISPATCH_BUDGET
        {
            return Err(SendFailure::Outcome(
                RouteOutcome::GovernanceUnavailableTimedOut {
                    stage: ProbeStage::Request,
                    elapsed_ms: now.saturating_duration_since(self.started).as_millis() as u64,
                },
            ));
        }
        if remaining.is_zero() {
            return Err(SendFailure::Outcome(RouteOutcome::Unavailable(format!(
                "the gh relay's {} ms budget ran out before the request could be sent; nothing was sent",
                self.budget_ms()
            ))));
        }
        let options = CallOptions {
            timeout: if body["op"] == BOT_REQUEST_OPERATION {
                remaining
            } else {
                remaining.min(READ_RELAY_BUDGET)
            },
            ..CallOptions::default()
        };
        #[cfg(test)]
        if body["op"] == BOT_REQUEST_OPERATION {
            ATTEMPT_DEADLINES.with(|slot| {
                if let Some(deadlines) = slot.borrow_mut().as_mut() {
                    deadlines.push(now + options.timeout);
                }
            });
        }
        match self.io.request(bytes, options).await {
            Ok(bytes) => Ok(bytes),
            Err(CallError::NotSent(_)) => {
                Err(SendFailure::Outcome(RouteOutcome::GovernanceUnavailable))
            }
            Err(CallError::Module(body)) => Err(SendFailure::Module {
                code: body.code.clone(),
                message: body.message.clone(),
            }),
            // Reported as the time the whole exchange waited, which is what
            // the caller experienced; it never exceeds the budget.
            Err(_) => Err(SendFailure::Outcome(RouteOutcome::OutcomeUnknown {
                elapsed_ms: self
                    .io
                    .now()
                    .saturating_duration_since(self.started)
                    .min(self.budget)
                    .as_millis() as u64,
            })),
        }
    }

    /// Read plexus's bindings through the relay and compare them with the
    /// manifest. Any failure to read them refuses the write.
    async fn check_bindings(
        &self,
        agent_id: &str,
        manifest: &Manifest,
    ) -> Result<BindingsCheck, RouteOutcome> {
        let body = json!({
            "op": BINDINGS_READ_OPERATION,
            // The daemon ignores agent_id: plexus answers the binding read
            // with every repository's binding, whoever asks. It is still sent
            // so a daemon from before this change keeps answering.
            "params": {"ticket": self.ticket, "agent_id": agent_id,"purpose":"speech"},
        });
        let bytes = self
            .send(&body)
            .await
            .map_err(|error| match error.into_outcome() {
                // A read cannot have written anything, so an unanswered read is
                // simply unavailable rather than an unknown write outcome.
                RouteOutcome::OutcomeUnknown { .. } => RouteOutcome::Unavailable(
                    "plexus's repository bindings could not be read for the version check"
                        .to_string(),
                ),
                other => other,
            })?;
        let envelope: Value = serde_json::from_slice(&bytes).map_err(|_| {
            RouteOutcome::SchemaMismatch("the bindings read reply was not JSON".to_string())
        })?;
        let data = envelope.get("data").cloned().unwrap_or(Value::Null);
        if envelope.get("status").and_then(Value::as_str) != Some("ok") {
            let code = data
                .get("refusal_code")
                .and_then(Value::as_str)
                .unwrap_or("relay_refused");
            let stage = data.get("stage").and_then(Value::as_str).unwrap_or("relay");
            let message = data.get("message").and_then(Value::as_str).unwrap_or("");
            return Err(daemon_refusal(
                code,
                stage,
                message,
                &RefusalFacts::from_reply(&data),
            ));
        }
        let reply = facade_reply(&data);
        if let Some(code) = facade_refusal_code(&reply) {
            return Err(RouteOutcome::RelayRefusal {
                code: code.to_string(),
                text: format!(
                    "plexus refused the repository bindings read for the version check: {}",
                    plexus_refusal_text(code, &RefusalFacts::from_reply(&reply["result"]))
                ),
            });
        }
        let generation = compare_bindings(&manifest.bindings, &reply).map_err(|text| {
            RouteOutcome::RelayRefusal {
                code: "binding_view_mismatch".to_string(),
                text,
            }
        })?;
        Ok(BindingsCheck {
            manifest_version: manifest.manifest_version,
            repo_binding_generation: generation,
            handles: reply
                .get("result")
                .unwrap_or(&reply)
                .get("bindings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|row| {
                    Some((
                        row.get("repository")?.as_str()?.to_ascii_lowercase(),
                        row.get("app_handle_id")?.as_str()?.to_string(),
                    ))
                })
                .collect(),
        })
    }
}

/// Send one governed request through the relay with one nonce. Explicit retry
/// metadata controls resends; older replies use the legacy refusal classes.
/// Attempts and retry sleeps share one budget, so a retry sleep that would run
/// past the deadline is not taken.
#[allow(clippy::too_many_arguments)]
async fn exchange(
    exchange: &Exchange<'_, impl ExchangeIo>,
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    manifest: &Manifest,
    body: &Value,
    transient_delays: [Duration; 2],
) -> RouteOutcome {
    let mut transient_retries = 0_usize;
    let mut confirmed =
        load_check(paths).filter(|check| check.manifest_version == manifest.manifest_version);
    if confirmed.is_none() {
        match exchange
            .check_bindings_with_warming_retry(
                &agent_binding.agent_id,
                manifest,
                &transient_delays,
                &mut transient_retries,
            )
            .await
        {
            Ok(check) => {
                store_check(paths, &check);
                confirmed = Some(check);
            }
            Err(outcome) => return outcome,
        }
    }
    let mut reminted = false;
    let mut pre_read_retried = false;
    let mut sent_steps = Vec::new();
    loop {
        let mut reply = match exchange.send(body).await {
            Ok(bytes) => parse_reply(&bytes),
            Err(SendFailure::Module { code, .. }) if code == "module_warming" => {
                Reply::Warming { origin: "AFT" }
            }
            Err(error) => return error.into_outcome(),
        };
        if let Reply::Refused { facts, .. } | Reply::ModuleRefused { facts, .. } = &mut reply {
            // An early gate on a later retry may not know which steps an
            // earlier attempt sent. Keep those steps in the final warning.
            for step in &facts.legs_sent {
                if !sent_steps.contains(step) {
                    sent_steps.push(step.clone());
                }
            }
            facts.legs_sent.clone_from(&sent_steps);
        }
        let reply_generation = match &reply {
            Reply::Completed { generation, .. } | Reply::Refused { generation, .. } => *generation,
            Reply::Malformed(_) | Reply::ModuleRefused { .. } | Reply::Warming { .. } => None,
        };
        // A new binding generation means plexus's bindings changed since the
        // last check; compare again before trusting the next write.
        let mut mismatch = None;
        if let Some(generation) = reply_generation {
            if confirmed
                .as_ref()
                .is_none_or(|check| check.repo_binding_generation != generation)
            {
                match exchange
                    .check_bindings_with_warming_retry(
                        &agent_binding.agent_id,
                        manifest,
                        &transient_delays,
                        &mut transient_retries,
                    )
                    .await
                {
                    Ok(check) => {
                        store_check(paths, &check);
                        confirmed = Some(check);
                    }
                    Err(outcome) => {
                        clear_check(paths);
                        mismatch = Some(outcome);
                    }
                }
            }
        }
        let retry_same_nonce = match &reply {
            Reply::Refused { code, facts, .. } | Reply::ModuleRefused { code, facts } => {
                if facts.not_sent {
                    // Safe to resend does not mean useful to resend: deterministic
                    // read refusals (including 404/422) will just refuse again.
                    Some(
                        facts.retry_same_nonce == Some(true)
                            && code_prefix(code) == "pre_dispatch_read_incomplete"
                            && matches!(
                                detail_field(code_detail(code), "reason").as_deref(),
                                Some("transport_unknown" | "lease_expired")
                            )
                            && !pre_read_retried,
                    )
                } else {
                    facts.retry_same_nonce
                }
            }
            _ => None,
        };
        let mut retry_budget_ms = None;
        if retry_same_nonce == Some(true) {
            if let Some(outcome) = mismatch {
                return outcome;
            }
            // Pre-read retries have their own one-retry cap. Other explicit
            // retries keep the final backoff and stop at the shared deadline.
            let delay = transient_delays[transient_retries.min(transient_delays.len() - 1)];
            if exchange.io.now() + delay < exchange.deadline() {
                exchange.io.sleep(delay).await;
                if matches!(&reply, Reply::Refused { facts, .. } | Reply::ModuleRefused { facts, .. } if facts.not_sent)
                {
                    pre_read_retried = true;
                }
                transient_retries += 1;
                continue;
            }
            retry_budget_ms = Some(exchange.budget_ms());
        }
        match reply {
            Reply::Warming { origin } => {
                if exchange
                    .retry_warming(&transient_delays, &mut transient_retries)
                    .await
                {
                    continue;
                }
                // module_warming is refused before forwarding. Do not infer
                // the same guarantee for other daemon or transport errors.
                let facts = RefusalFacts {
                    not_sent: true,
                    legs_sent: sent_steps,
                    ..RefusalFacts::default()
                };
                return RouteOutcome::RelayRefusal {
                    code: "module_warming".to_string(),
                    text: refusal_text(origin, "module_warming", &facts, None),
                };
            }
            Reply::Completed { result, .. } => {
                let outcome = if let Some(output) =
                    render_thread_state_confirmation(&result, &body["params"]["request"])
                {
                    RouteOutcome::ResultStderr(output)
                } else {
                    match render_completed(&result) {
                        Ok(output) => RouteOutcome::Result(output),
                        Err(outcome) => return outcome,
                    }
                };
                if let Some(RouteOutcome::RelayRefusal { text, .. }) = &mismatch {
                    // The write already happened; say so, and leave the check
                    // cleared so the next governed write refuses up front.
                    eprintln!("gh-shim: warning: {text}");
                }
                return outcome;
            }
            Reply::Malformed(message) => {
                // The request was sent; an unreadable reply cannot prove the
                // write did not happen, so returning a refusal risks a duplicate.
                return RouteOutcome::OutcomeUndetermined(outcome_undetermined_text(&format!(
                    "unreadable plexus reply: {message}"
                )));
            }
            Reply::ModuleRefused { code, facts } => {
                if facts.not_sent && facts.legs_sent.is_empty() {
                    return RouteOutcome::RelayRefusal {
                        text: refusal_text("Plexus", &code, &facts, retry_budget_ms),
                        code,
                    };
                }
                return RouteOutcome::OutcomeUndetermined(refusal_text(
                    "Plexus",
                    &code,
                    &facts,
                    retry_budget_ms,
                ));
            }
            Reply::Refused {
                code,
                stage,
                message,
                facts,
                ..
            } => {
                if let Some(outcome) = mismatch {
                    return outcome;
                }
                if stage != PLEXUS_REPLY_STAGE {
                    return daemon_refusal(&code, &stage, &message, &facts);
                }
                let class = classify_code(&code);
                match (retry_same_nonce, class) {
                    (None, CodeClass::Transient) if transient_retries < transient_delays.len() => {
                        let delay = transient_delays[transient_retries];
                        if exchange.io.now() + delay >= exchange.deadline() {
                            return RouteOutcome::RelayRefusal {
                                text: refusal_text(
                                    "Plexus",
                                    &code,
                                    &facts,
                                    Some(exchange.budget_ms()),
                                ),
                                code,
                            };
                        }
                        exchange.io.sleep(delay).await;
                        transient_retries += 1;
                    }
                    (None, CodeClass::Remint) if !reminted => reminted = true,
                    (_, CodeClass::OutcomeUnknown) if !facts.not_sent => {
                        return RouteOutcome::OutcomeUndetermined(refusal_text(
                            "Plexus",
                            &code,
                            &facts,
                            retry_budget_ms,
                        ))
                    }
                    _ => {
                        return RouteOutcome::RelayRefusal {
                            text: refusal_text("Plexus", &code, &facts, retry_budget_ms),
                            code,
                        }
                    }
                }
            }
        }
    }
}

/// Carry one governed request through the AFT daemon's relay.
#[allow(clippy::too_many_arguments)]
pub(super) fn route(
    paths: &StatePaths,
    determination: &RungRecord,
    agent_binding: &AgentBinding,
    request: GovernedRequest,
    now: u64,
    manifest: &Manifest,
    relay: &RelayContext,
) -> RouteOutcome {
    if let Err(error) = write_seam_state(paths, governed_seam_state(paths, None, agent_binding)) {
        return RouteOutcome::Unavailable(format!("governed self-report update failed: {error}"));
    }
    // Without a ticket the command has no agent session to speak for, so it
    // is refused before the daemon is contacted at all.
    let Some(ticket) = relay
        .ticket
        .clone()
        .filter(|ticket| !ticket.trim().is_empty())
    else {
        return RouteOutcome::NoAgentSession;
    };
    let Some(connection_file) = relay.connection_file.clone() else {
        return RouteOutcome::GovernanceUnavailable;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_root = project_root_for(&cwd);
    // One nonce per invocation: every resend of this request reuses it, so
    // plexus can tell a retry from a second write.
    let invocation = request.invocation.clone();
    let body = json!({
        "op": BOT_REQUEST_OPERATION,
        "params": {
            "ticket": ticket,
            "request_nonce": new_nonce(),
            "request": governed_wire_request(determination, &agent_binding.agent_id, request),
            "invocation":invocation,
        },
    });
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return RouteOutcome::Unavailable(error.to_string()),
    };
    let stage = Arc::new(Mutex::new(ProbeStage::Connect));
    let started = Instant::now();
    let outcome = runtime.block_on(async {
        let setup_stage = Arc::clone(&stage);
        let setup = tokio::time::timeout(SETUP_TIMEOUT, async move {
            let options = ConsumerOptions {
                call_timeout: SETUP_TIMEOUT,
                handshake_timeout: SETUP_TIMEOUT,
                ..ConsumerOptions::default()
            };
            let consumer = SubcConsumer::connect(&connection_file, options)
                .await
                .map_err(|_| RouteOutcome::GovernanceUnavailable)?;
            *lock_probe_stage(&setup_stage) = ProbeStage::CatalogList;
            let catalog = consumer
                .catalog_list()
                .await
                .map_err(|_| RouteOutcome::GovernanceUnavailable)?;
            let module_id = relay_holder(&catalog.modules)
                .ok_or_else(|| RouteOutcome::Unavailable(RELAY_UNSERVED_TEXT.to_string()))?;
            *lock_probe_stage(&setup_stage) = ProbeStage::OpenRoute;
            let route = consumer
                .open_route(
                    RouteTarget::ManagementSurface { module_id },
                    BindIdentity::new("aft-gh-shim", gh_session_id(&agent_binding.agent_id)),
                    CallOptions::default(),
                )
                .await
                .map_err(|_| RouteOutcome::UnboundIdentity)?;
            Ok::<_, RouteOutcome>((consumer, route))
        })
        .await;
        let (consumer, route) = match setup {
            Ok(Ok(connected)) => connected,
            // Running out of setup budget while the daemon looked unreachable
            // is a timeout on a busy host, not a missing daemon.
            Ok(Err(RouteOutcome::GovernanceUnavailable)) if started.elapsed() >= SETUP_TIMEOUT => {
                return RouteOutcome::GovernanceUnavailableTimedOut {
                    stage: *lock_probe_stage(&stage),
                    elapsed_ms: SETUP_TIMEOUT.as_millis() as u64,
                }
            }
            Ok(Err(outcome)) => return outcome,
            Err(_) => {
                return RouteOutcome::GovernanceUnavailableTimedOut {
                    stage: *lock_probe_stage(&stage),
                    elapsed_ms: SETUP_TIMEOUT.as_millis() as u64,
                }
            }
        };
        if let Err(error) = write_seam_state(
            paths,
            governed_seam_state(paths, Some(RELAY_MODULE_ID.to_string()), agent_binding),
        ) {
            let _ = consumer
                .close_handle(&route, CloseRouteOptions::default())
                .await;
            return RouteOutcome::Unavailable(format!(
                "governed self-report update failed: {error}"
            ));
        }
        let outcome = exchange(
            &Exchange {
                io: &SubcExchangeIo {
                    consumer: &consumer,
                    route: &route,
                    #[cfg(test)]
                    clock: TestExchangeClock::for_this_thread(),
                },
                ticket: &ticket,
                stage: &stage,
                started: {
                    #[cfg(test)]
                    ATTEMPT_DEADLINES.with(|slot| {
                        if slot.borrow().is_some() {
                            std::thread::sleep(Duration::from_millis(400));
                        }
                    });
                    Instant::now()
                },
                budget: relay.relay_budget,
            },
            paths,
            agent_binding,
            manifest,
            &body,
            relay.transient_delays,
        )
        .await;
        let _ = consumer
            .close_handle(&route, CloseRouteOptions::default())
            .await;
        outcome
    });
    // `gh --status` shows where the last governed call ran out of time.
    let timed_out = match &outcome {
        RouteOutcome::GovernanceUnavailableTimedOut { stage, elapsed_ms } => {
            Some((*stage, *elapsed_ms))
        }
        RouteOutcome::OutcomeUnknown { elapsed_ms } => Some((ProbeStage::Request, *elapsed_ms)),
        _ => None,
    };
    if let Some((stage, elapsed_ms)) = timed_out {
        write_last_probe_silently(
            paths,
            &LastProbeReport {
                stage: stage.as_str().to_string(),
                elapsed_ms,
                outcome: "timed_out".to_string(),
            },
        );
    }
    if let RouteOutcome::RelayRefusal { code, .. } = &outcome {
        if let Err(error) = write_seam_state(
            paths,
            SeamState {
                bound_holder: seam_state(paths).bound_holder,
                agent_binding: Some(agent_binding.clone()),
                last_seam_refusal: Some(LastSeamRefusal {
                    code: code.clone(),
                    at_unix_secs: now,
                }),
            },
        ) {
            return RouteOutcome::Unavailable(format!(
                "governed self-report update failed: {error}"
            ));
        }
    }
    describe_session_agent_mismatch(outcome, agent_binding, manifest, paths)
}

/// Preserve the mint refusal verbatim after a caller-facing identity summary.
/// The repository's identity comes from the active manifest, not the ids in an
/// upstream error. The session id is only reported when prefrontal names it.
fn describe_session_agent_mismatch(
    outcome: RouteOutcome,
    binding: &AgentBinding,
    manifest: &Manifest,
    paths: &StatePaths,
) -> RouteOutcome {
    let RouteOutcome::RelayRefusal { code, text } = &outcome else {
        return outcome;
    };
    if code != "assertion_session_agent_mismatch" {
        return outcome;
    }
    let Some(agent) = manifest.bindings.get(&binding.repo) else {
        return outcome;
    };
    let session_agent = text
        .split_once("belongs to agent ")
        .and_then(|(_, tail)| tail.split_once(", not the requested agent "))
        .map(|(agent, _)| agent)
        .filter(|agent| {
            !agent.is_empty()
                && agent
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '[' | ']'))
        })
        .unwrap_or("unknown (prefrontal did not name the session's agent)");
    let handle = load_check(paths)
        .filter(|check| check.manifest_version == manifest.manifest_version)
        .and_then(|check| check.handles.get(&binding.repo).cloned())
        .filter(|handle| super::valid_github_login(handle));
    let identity = handle
        .map(|handle| format!("{handle} ({agent})"))
        .unwrap_or_else(|| agent.clone());
    RouteOutcome::RelayRefusal {
        code: code.clone(),
        text: format!(
            "{} is bound to {identity}; this session is {session_agent}; {text}",
            binding.repo
        ),
    }
}

#[cfg(test)]
#[path = "gh_shim_relay_client_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "gh_shim_speech/partner_client_tests.rs"]
mod partner_contract_tests;

/// Resolve session kind from the daemon's ticket record, never from a body claim.
pub(super) fn worker_session(relay: &RelayContext) -> Result<Option<bool>, String> {
    session_context(relay).map(|context| {
        context.and_then(|context| {
            if context.worker {
                Some(true)
            } else if context.head {
                Some(false)
            } else {
                None
            }
        })
    })
}

pub(super) struct SessionContext {
    pub(super) worker: bool,
    pub(super) head: bool,
    pub(super) repository: Option<String>,
}

pub(super) fn worker_repository(relay: &RelayContext) -> Result<String, String> {
    let context = session_context(relay)?.ok_or("no authenticated session context")?;
    if !context.worker {
        return Err("the session is not a worker".into());
    }
    context.repository.ok_or_else(|| {
        "the ticket has no immutable repository snapshot; restart the command".into()
    })
}

fn session_context(relay: &RelayContext) -> Result<Option<SessionContext>, String> {
    if let Some(client) = &relay.worker_broker {
        let client = client.as_ref().map_err(ToString::to_string)?;
        let data = crate::gh_shim_worker_broker::request(
            client,
            crate::gh_shim_worker_broker::Operation::SessionContext,
            relay.relay_budget.min(READ_RELAY_BUDGET),
        )?;
        let worker = data["worker"]
            .as_bool()
            .ok_or("broker omitted worker role")?;
        let head = data["head"].as_bool().ok_or("broker omitted head role")?;
        if !worker || head {
            return Err("broker did not authenticate a worker".into());
        }
        let repository = data["repository"]
            .as_str()
            .filter(|r| super::canonical_repository_key(r).as_deref() == Some(*r))
            .ok_or("broker returned an invalid repository")?
            .to_string();
        return Ok(Some(SessionContext {
            worker,
            head,
            repository: Some(repository),
        }));
    }
    let Some(ticket) = relay.ticket.as_deref().filter(|ticket| !ticket.is_empty()) else {
        return Ok(None);
    };
    let connection = relay
        .connection_file
        .as_ref()
        .ok_or("worker read session lookup requires the AFT daemon connection")?;
    let budget = SETUP_TIMEOUT.min(relay.relay_budget);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        tokio::time::timeout(budget, async {
            let consumer = SubcConsumer::connect(connection, ConsumerOptions {
                call_timeout: budget, handshake_timeout: budget, ..ConsumerOptions::default()
            }).await.map_err(|error| format!("worker read session lookup failed: {error}"))?;
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            let route = consumer.open_route(
                RouteTarget::ManagementSurface { module_id: RELAY_MODULE_ID.to_string() },
                BindIdentity::new("aft-gh-shim", "gh-read-session"),
                CallOptions::default(),
            ).await.map_err(|error| format!("worker read session lookup is unavailable; restart the AFT daemon: {error}"))?;
            let request = json!({"op": crate::gh_shim_relay::SESSION_CONTEXT_OPERATION, "params": {"ticket": ticket}});
            let reply = consumer.request(&route, serde_json::to_vec(&request).map_err(|error| error.to_string())?, CallOptions::default()).await;
            let _ = consumer.close_handle(&route, CloseRouteOptions::default()).await;
            let bytes = reply.map_err(|error| format!("worker read session lookup failed: {error}"))?;
            let envelope: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if envelope["status"] != "ok" {
                if envelope["data"]["refusal_code"] == "session_unscoped" { return Ok(None); }
                return Err(format!("worker read session lookup refused: {}", envelope["data"]["message"].as_str().unwrap_or("no authenticated session context")));
            }
            let worker = envelope["data"]["worker"].as_bool().ok_or("the daemon returned no authenticated worker context")?;
            let head = envelope["data"].get("head").and_then(Value::as_bool).unwrap_or(false);
            if worker && head { return Err("the daemon returned conflicting caller roles".into()); }
            let repository = match envelope["data"].get("repository") {
                None | Some(Value::Null) => None,
                Some(Value::String(repository)) if super::canonical_repository_key(repository).as_deref() == Some(repository.as_str()) => Some(repository.clone()),
                _ => return Err("the daemon returned an invalid repository snapshot".into()),
            };
            Ok(Some(SessionContext {worker, head, repository}))
        }).await.map_err(|_| "worker read session lookup timed out; caller identity was not established".to_string())?
    })
}

/// Send one github_read request through AFT's ticket-authenticated management
/// relay. Tool refusals remain Response bodies; Error frames are transport errors.
pub(super) fn read_request(relay: &RelayContext, request: &Value) -> Result<Value, String> {
    let budget = relay.relay_budget.min(READ_RELAY_BUDGET);
    if let Some(client) = &relay.worker_broker {
        let client = client.as_ref().map_err(ToString::to_string)?;
        let request =
            serde_json::from_value(request.clone()).map_err(|_| "invalid worker read request")?;
        return crate::gh_shim_worker_broker::request(
            client,
            crate::gh_shim_worker_broker::Operation::ReadRequest { request },
            budget,
        );
    }
    let ticket = relay
        .ticket
        .as_deref()
        .filter(|ticket| !ticket.is_empty())
        .ok_or("no live command ticket is attached")?;
    let connection = relay
        .connection_file
        .as_ref()
        .ok_or("no AFT daemon connection is configured")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        tokio::time::timeout(budget, async {
            let setup = tokio::time::timeout(SETUP_TIMEOUT, async {
                let consumer = SubcConsumer::connect(connection, ConsumerOptions {
                    call_timeout: budget, handshake_timeout: SETUP_TIMEOUT, ..ConsumerOptions::default()
                }).await.map_err(|error| error.to_string())?;
                let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
                let route = consumer.open_route(
                    RouteTarget::ManagementSurface { module_id: RELAY_MODULE_ID.to_string() },
                    BindIdentity::new("aft-gh-shim", "gh-read-request"),
                    CallOptions::default(),
                ).await.map_err(|error| error.to_string())?;
                Ok::<_, String>((consumer, route))
            }).await.map_err(|_| "read relay setup timed out".to_string())??;
            let (consumer, route) = setup;
            let body = json!({"op": crate::gh_shim_relay::READ_REQUEST_OPERATION, "params": {"ticket": ticket, "request": request}});
            let reply = consumer.request(&route, serde_json::to_vec(&body).map_err(|error| error.to_string())?, CallOptions::default()).await;
            let _ = consumer.close_handle(&route, CloseRouteOptions::default()).await;
            let bytes = reply.map_err(|error| format!("read relay transport error: {error}"))?;
            let envelope: Value = serde_json::from_slice(&bytes).map_err(|error| format!("malformed read relay response: {error}"))?;
            match envelope["status"].as_str() {
                Some("ok") => envelope.get("data").cloned().ok_or_else(|| "read relay response omitted data".to_string()),
                Some("error") => Err(format!("{}: {}", envelope["data"]["refusal_code"].as_str().unwrap_or("relay_error"), envelope["data"]["message"].as_str().unwrap_or("read relay refused"))),
                _ => Err("read relay response has no known status".into()),
            }
        }).await.map_err(|_| "read relay timed out; the read was not retried".to_string())?
    })
}
