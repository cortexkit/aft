use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::bash_rewrite::catalog::ControlRole;
use crate::bash_rewrite::rules::{
    CatAppendRule, CatRule, FindRule, GrepRule, HeadRule, LsRule, ParsedRewriteRule, RgRule,
    SedRule, TailRule,
};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// The application registers the four handlers that can execute a rewrite.
pub struct RewriteDispatch {
    pub read: fn(&RawRequest, &AppContext) -> Response,
    pub grep: fn(&RawRequest, &AppContext) -> Response,
    pub glob: fn(&RawRequest, &AppContext) -> Response,
    pub edit_match: fn(&RawRequest, &AppContext) -> Response,
}

static HANDLERS: OnceLock<RewriteDispatch> = OnceLock::new();

pub fn register(handlers: RewriteDispatch) {
    let _ = HANDLERS.set(handlers);
}

fn run_with(
    handlers: Option<&RewriteDispatch>,
    request: &RawRequest,
    ctx: &AppContext,
) -> Response {
    let Some(handlers) = handlers else {
        // An accepted rewrite must never turn into native shell execution,
        // even when the application has not registered rewrite handlers.
        return Response::error(
            &request.id,
            "bash_rewrite_unavailable",
            "bash rewrite handlers are not registered; command was not started",
        );
    };
    let handler = match request.command.as_str() {
        "read" => handlers.read,
        "grep" => handlers.grep,
        "glob" => handlers.glob,
        "edit_match" => handlers.edit_match,
        _ => unreachable!("rewrite target is declared by a built-in rule"),
    };
    handler(request, ctx)
}

pub(super) fn run(request: &RawRequest, ctx: &AppContext) -> Response {
    run_with(HANDLERS.get(), request, ctx)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DispatchRoute {
    Rewritten {
        rule_id: String,
        branch_id: String,
        decision_class_id: String,
    },
    Native {
        role: String,
        branch_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchRecord {
    pub request_id: String,
    pub route: DispatchRoute,
}

const MAX_ROUTE_RECORDS: usize = 1024;

#[derive(Default)]
struct RouteRecordStore {
    records: HashMap<String, DispatchRecord>,
    oldest_first: VecDeque<String>,
}

static ROUTE_RECORDS: OnceLock<Mutex<RouteRecordStore>> = OnceLock::new();

fn route_records() -> &'static Mutex<RouteRecordStore> {
    ROUTE_RECORDS.get_or_init(|| Mutex::new(RouteRecordStore::default()))
}

#[cfg(test)]
pub(crate) fn count_route_records_for_prefix(prefix: &str) -> usize {
    route_records()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .records
        .keys()
        .filter(|request_id| request_id.starts_with(prefix))
        .count()
}

fn store_record(record: DispatchRecord) {
    if let Ok(mut records) = route_records().lock() {
        if records.records.contains_key(&record.request_id) {
            records
                .oldest_first
                .retain(|request_id| request_id != &record.request_id);
        }
        records.oldest_first.push_back(record.request_id.clone());
        records
            .records
            .insert(record.request_id.clone(), record.clone());
        while records.oldest_first.len() > MAX_ROUTE_RECORDS {
            if let Some(expired) = records.oldest_first.pop_front() {
                records.records.remove(&expired);
            }
        }
    }

    // The sidecar is opt-in for the differential child process. Normal AFT
    // responses remain byte-for-byte free of test metadata.
    if let Some(path) = std::env::var_os("AFT_BASH_REWRITE_ROUTE_RECORD") {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            if let Ok(line) = serde_json::to_string(&record) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

pub fn route_record(request_id: &str) -> Option<DispatchRecord> {
    route_records()
        .lock()
        .ok()
        .and_then(|records| records.records.get(request_id).cloned())
}

pub fn take_route_record(request_id: &str) -> Option<DispatchRecord> {
    let mut records = route_records().lock().ok()?;
    records
        .oldest_first
        .retain(|record_id| record_id != request_id);
    records.records.remove(request_id)
}

pub fn record_native(request_id: &str, role: ControlRole, branch_id: &str, reason: &str) {
    store_record(DispatchRecord {
        request_id: request_id.to_string(),
        route: DispatchRoute::Native {
            role: role.id().to_string(),
            branch_id: branch_id.to_string(),
            reason: reason.to_string(),
        },
    });
}

pub fn dispatch(command: &str, session_id: Option<&str>, ctx: &AppContext) -> Option<Response> {
    dispatch_for_request(
        command,
        "bash_rewrite",
        session_id,
        &crate::host_tool_names::HostToolNames::default(),
        ctx,
    )
}

pub fn dispatch_for_request(
    command: &str,
    request_id: &str,
    session_id: Option<&str>,
    host_tool_names: &crate::host_tool_names::HostToolNames,
    ctx: &AppContext,
) -> Option<Response> {
    dispatch_for_request_with_handlers(
        command,
        request_id,
        session_id,
        host_tool_names,
        ctx,
        HANDLERS.get(),
    )
}

pub(crate) fn dispatch_for_request_with_handlers(
    command: &str,
    request_id: &str,
    session_id: Option<&str>,
    host_tool_names: &crate::host_tool_names::HostToolNames,
    ctx: &AppContext,
    handlers: Option<&RewriteDispatch>,
) -> Option<Response> {
    if !ctx.config().experimental_bash_rewrite {
        record_native(
            request_id,
            ControlRole::Native,
            "dispatch.native.no_rule",
            "experimental bash rewriting is disabled",
        );
        return None;
    }

    // Shape checks may themselves query a typed handler (for example find's
    // glob comparison). Refuse before those checks can mistake an unavailable
    // handler for an unfaithful rewrite and choose native bash instead.
    if handlers.is_none() {
        return Some(Response::error(
            request_id,
            "bash_rewrite_unavailable",
            "bash rewrite handlers are not registered; command was not started",
        ));
    }

    let parsed = crate::bash_rewrite::parser::parse(command);
    let rules: [&dyn ParsedRewriteRule; 9] = [
        &GrepRule,
        &RgRule,
        &FindRule,
        &CatRule,
        &HeadRule,
        &TailRule,
        &CatAppendRule,
        &SedRule,
        &LsRule,
    ];

    for rule in rules {
        let decision = rule.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx);
        match decision {
            crate::bash_rewrite::RewriteDecision::Accept(mut request) => {
                // The footer appended to a rewritten command names the caller's
                // own tools. Only the request carries those names and the rule
                // decided from the command alone, so they are copied on here.
                request.host_tool_names = host_tool_names.clone();
                store_record(DispatchRecord {
                    request_id: request_id.to_string(),
                    route: DispatchRoute::Rewritten {
                        rule_id: request.rule_id.to_string(),
                        branch_id: request.branch_id.to_string(),
                        decision_class_id: request.decision_class_id.to_string(),
                    },
                });
                // Do not turn a handler failure into a native execution. The
                // handler has already begun and a second execution could
                // duplicate a mutation such as cat_append.
                //
                // A rewrite runs the command in this process, so it passes the
                // same startup fence as process creation: after the bash reply
                // deadline has told the caller "not started", it must not run.
                if let Err(message) = crate::bash_background::commit_spawn_receipt_inline() {
                    return Some(Response::error(
                        request_id,
                        "bash_start_deadline",
                        format!("{message}; command was not started"),
                    ));
                }
                return Some(rule.execute(&request, ctx));
            }
            crate::bash_rewrite::RewriteDecision::Decline(reason) => {
                if reason.rule_id.is_some() {
                    crate::slog_debug!(
                        "bash rewrite rule {} declined before execution: {}",
                        reason.rule_id.unwrap_or("unknown"),
                        reason.reason
                    );
                }
            }
        }
    }

    record_native(
        request_id,
        ControlRole::Native,
        "dispatch.native.no_rule",
        "no rewrite rule accepted the command shape",
    );
    None
}
