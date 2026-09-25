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
/// Budget for one relayed request (assertion mint, plexus, GitHub). A request
/// that gets no reply within it is reported as an unknown outcome and never
/// resent, the same contract the earlier `gh.route` path had.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Retries of a transient refusal, each reusing the same request nonce.
const TRANSIENT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(10)];
const BINDINGS_CHECK_FILE: &str = "relay-bindings-check.json";

/// Everything the relay needs from the process, gathered once so tests can
/// supply a fake daemon and ticket without touching the environment.
pub(super) struct RelayContext {
    pub(super) connection_file: Option<PathBuf>,
    pub(super) ticket: Option<String>,
    pub(super) transient_delays: [Duration; 2],
}

impl RelayContext {
    pub(super) fn from_process() -> Self {
        Self {
            connection_file: super::configured_connection_file(),
            ticket: std::env::var(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV).ok(),
            transient_delays: TRANSIENT_RETRY_DELAYS,
        }
    }
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

/// How the shim reacts to a plexus refusal code.
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

/// The caller-facing text for a plexus refusal that ends the command.
pub(super) fn plexus_refusal_text(code: &str, class: CodeClass) -> String {
    match class {
        CodeClass::Terminal if code_prefix(code) == "agent_repository_conflict" => {
            let detail = code_detail(code);
            format!(
                "plexus refused the bot write: agent_repository_conflict - this command speaks as agent {} but the repository is bound to agent {} ({code})",
                detail_field(detail, "claimed_agent").unwrap_or_else(|| "(unnamed)".to_string()),
                detail_field(detail, "bound_agent").unwrap_or_else(|| "(unnamed)".to_string()),
            )
        }
        CodeClass::Terminal => format!("plexus refused the bot write: {code}"),
        CodeClass::PlexusSetup => format!(
            "plexus configuration problem: {code} - the bot write was not attempted; the operator must fix plexus's GitHub handle, grant or assertion setup"
        ),
        CodeClass::Transient => format!(
            "plexus refused the bot write: {code} (still refused after retrying with the same request nonce)"
        ),
        CodeClass::Remint => format!(
            "plexus refused the bot write: {code} (a freshly minted assertion was refused too)"
        ),
        CodeClass::OutcomeUnknown => outcome_undetermined_text(code),
        CodeClass::Unknown => format!(
            "plexus refused the bot write with a code this shim does not know: {code}; it was not retried"
        ),
    }
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
            let generation = reply.get("repo_binding_generation").and_then(Value::as_u64);
            let result = reply.get("result").cloned().unwrap_or(Value::Null);
            match result.get("status").and_then(Value::as_str) {
                Some("completed") => Reply::Completed {
                    generation,
                    result: result.get("result").cloned().unwrap_or(Value::Null),
                },
                Some("refused") => match facade_refusal_code(&reply) {
                    Some(code) => Reply::Refused {
                        code: code.to_string(),
                        stage: PLEXUS_REPLY_STAGE.to_string(),
                        message: String::new(),
                        generation,
                    },
                    None => Reply::Malformed("plexus refused without a refusal_code".to_string()),
                },
                _ => Reply::Malformed("plexus's reply had no known result.status".to_string()),
            }
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
            },
            None => Reply::Malformed("the relay refused without a refusal_code".to_string()),
        },
        _ => Reply::Malformed("the relay reply had no status".to_string()),
    }
}

/// Render a completed write the way upstream `gh` would where plexus says
/// enough: a speech verb prints its URL, a close or reopen prints the state
/// it applied (and the URL of the comment it posted, when there was one), and
/// other fields go through the governed renderer. Plexus omits any field
/// GitHub did not return, so every field is optional; a reply with no detail
/// prints a plain completion line.
///
/// Plexus posts a close or reopen comment before it changes state and only
/// changes state if the comment succeeded, so the partial "state applied,
/// comment failed" rendering can never arise on this path.
pub(super) fn render_completed(result: &Value) -> Result<String, RouteOutcome> {
    let Some(object) = result.as_object().filter(|object| !object.is_empty()) else {
        return Ok("completed\n".to_string());
    };
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

fn daemon_refusal(code: &str, stage: &str, message: &str) -> RouteOutcome {
    match code {
        "ticket_absent" => RouteOutcome::NoAgentSession,
        "ticket_unknown" => RouteOutcome::RelayRefusal {
            code: code.to_string(),
            text: "this command's agent session ticket is not live (its command ended, or this daemon never issued it); bot speech must come from an agent's own session".to_string(),
        },
        "untrusted_principal" => RouteOutcome::RelayRefusal {
            code: code.to_string(),
            text: "the AFT daemon relays bot writes only for first-party callers".to_string(),
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
            let class = classify_code(code);
            RouteOutcome::RelayRefusal {
                code: code.to_string(),
                text: plexus_refusal_text(code, class),
            }
        }
    }
}

/// What the shim last confirmed about plexus's repository bindings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct BindingsCheck {
    pub(super) manifest_version: u64,
    pub(super) repo_binding_generation: u64,
}

fn load_check(paths: &StatePaths) -> Option<BindingsCheck> {
    serde_json::from_slice(&std::fs::read(paths.root.join(BINDINGS_CHECK_FILE)).ok()?).ok()
}

fn store_check(paths: &StatePaths, check: &BindingsCheck) {
    let _ = std::fs::create_dir_all(&paths.root);
    if let Ok(bytes) = serde_json::to_vec(check) {
        let _ = std::fs::write(paths.root.join(BINDINGS_CHECK_FILE), bytes);
    }
}

fn clear_check(paths: &StatePaths) {
    let _ = std::fs::remove_file(paths.root.join(BINDINGS_CHECK_FILE));
}

/// The plexus connection that holds an agent's GitHub handle.
pub(super) fn connection_id(agent_id: &str) -> String {
    let slug: String = agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("github-handle-{slug}")
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
    if plexus == manifest_view {
        return Ok(generation);
    }
    Err(format!(
        "plexus's repository bindings (generation {generation}) do not match this shim's manifest; manifest: {}; plexus: {}; governed writes stay refused until they agree",
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

struct Exchange<'a> {
    consumer: &'a SubcConsumer,
    route: &'a subc_client_rs::RouteHandle,
    ticket: &'a str,
    stage: &'a Mutex<ProbeStage>,
}

impl Exchange<'_> {
    async fn send(&self, body: &Value) -> Result<Vec<u8>, RouteOutcome> {
        *self.stage.lock().unwrap() = ProbeStage::Request;
        let bytes = serde_json::to_vec(body)
            .map_err(|error| RouteOutcome::SchemaMismatch(error.to_string()))?;
        let options = CallOptions {
            timeout: REQUEST_TIMEOUT,
            ..CallOptions::default()
        };
        let started = Instant::now();
        match self.consumer.request(self.route, bytes, options).await {
            Ok(bytes) => Ok(bytes),
            Err(CallError::NotSent(_)) => Err(RouteOutcome::GovernanceUnavailable),
            Err(CallError::Module(body)) => Err(RouteOutcome::RelayRefusal {
                code: body.code.clone(),
                text: format!(
                    "the AFT daemon refused the relay request: {}: {}",
                    body.code, body.message
                ),
            }),
            Err(_) => Err(RouteOutcome::OutcomeUnknown {
                elapsed_ms: started.elapsed().min(REQUEST_TIMEOUT).as_millis() as u64,
            }),
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
            "params": {"ticket": self.ticket, "connection_id": connection_id(agent_id)},
        });
        let bytes = self.send(&body).await.map_err(|outcome| match outcome {
            // A read cannot have written anything, so an unanswered read is
            // simply unavailable rather than an unknown write outcome.
            RouteOutcome::OutcomeUnknown { .. } => RouteOutcome::Unavailable(
                "plexus's repository bindings could not be read for the version check".to_string(),
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
            return Err(daemon_refusal(code, stage, message));
        }
        let reply = facade_reply(&data);
        if let Some(code) = facade_refusal_code(&reply) {
            return Err(RouteOutcome::RelayRefusal {
                code: code.to_string(),
                text: format!(
                    "plexus refused the repository bindings read for the version check: {}",
                    plexus_refusal_text(code, classify_code(code))
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
        })
    }
}

/// Send one governed request through the relay, retrying only as the plexus
/// refusal class allows and always with this invocation's single nonce.
#[allow(clippy::too_many_arguments)]
async fn exchange(
    exchange: &Exchange<'_>,
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    manifest: &Manifest,
    body: &Value,
    transient_delays: [Duration; 2],
) -> RouteOutcome {
    let mut confirmed =
        load_check(paths).filter(|check| check.manifest_version == manifest.manifest_version);
    if confirmed.is_none() {
        match exchange
            .check_bindings(&agent_binding.agent_id, manifest)
            .await
        {
            Ok(check) => {
                store_check(paths, &check);
                confirmed = Some(check);
            }
            Err(outcome) => return outcome,
        }
    }
    let mut transient_retries = 0_usize;
    let mut reminted = false;
    loop {
        let bytes = match exchange.send(body).await {
            Ok(bytes) => bytes,
            Err(outcome) => return outcome,
        };
        let reply = parse_reply(&bytes);
        let reply_generation = match &reply {
            Reply::Completed { generation, .. } | Reply::Refused { generation, .. } => *generation,
            Reply::Malformed(_) => None,
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
                    .check_bindings(&agent_binding.agent_id, manifest)
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
        match reply {
            Reply::Completed { result, .. } => {
                let output = match render_completed(&result) {
                    Ok(output) => output,
                    Err(outcome) => return outcome,
                };
                if let Some(RouteOutcome::RelayRefusal { text, .. }) = &mismatch {
                    // The write already happened; say so, and leave the check
                    // cleared so the next governed write refuses up front.
                    eprintln!("gh-shim: warning: {text}");
                }
                return RouteOutcome::Result(output);
            }
            Reply::Malformed(message) => return RouteOutcome::SchemaMismatch(message),
            Reply::Refused {
                code,
                stage,
                message,
                ..
            } => {
                if let Some(outcome) = mismatch {
                    return outcome;
                }
                if stage != PLEXUS_REPLY_STAGE {
                    return daemon_refusal(&code, &stage, &message);
                }
                let class = classify_code(&code);
                match class {
                    CodeClass::Transient if transient_retries < transient_delays.len() => {
                        tokio::time::sleep(transient_delays[transient_retries]).await;
                        transient_retries += 1;
                    }
                    CodeClass::Remint if !reminted => reminted = true,
                    CodeClass::OutcomeUnknown => {
                        return RouteOutcome::OutcomeUndetermined(outcome_undetermined_text(&code))
                    }
                    _ => {
                        return RouteOutcome::RelayRefusal {
                            text: plexus_refusal_text(&code, class),
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
    let body = json!({
        "op": BOT_REQUEST_OPERATION,
        "params": {
            "ticket": ticket,
            "request_nonce": new_nonce(),
            "request": governed_wire_request(determination, &agent_binding.agent_id, request),
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
            *setup_stage.lock().unwrap() = ProbeStage::CatalogList;
            let catalog = consumer
                .catalog_list()
                .await
                .map_err(|_| RouteOutcome::GovernanceUnavailable)?;
            let module_id = relay_holder(&catalog.modules)
                .ok_or_else(|| RouteOutcome::Unavailable(RELAY_UNSERVED_TEXT.to_string()))?;
            *setup_stage.lock().unwrap() = ProbeStage::OpenRoute;
            let route = consumer
                .open_route(
                    RouteTarget::ManagementSurface { module_id },
                    BindIdentity::new(
                        project_root.to_string_lossy().into_owned(),
                        "aft-gh-shim",
                        gh_session_id(&agent_binding.agent_id),
                    ),
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
                    stage: *stage.lock().unwrap(),
                    elapsed_ms: SETUP_TIMEOUT.as_millis() as u64,
                }
            }
            Ok(Err(outcome)) => return outcome,
            Err(_) => {
                return RouteOutcome::GovernanceUnavailableTimedOut {
                    stage: *stage.lock().unwrap(),
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
                consumer: &consumer,
                route: &route,
                ticket: &ticket,
                stage: &stage,
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
    outcome
}

#[cfg(test)]
#[path = "gh_shim_relay_client_tests.rs"]
mod tests;
