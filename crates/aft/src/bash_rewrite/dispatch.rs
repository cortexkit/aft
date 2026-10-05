use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::bash_rewrite::catalog::ControlRole;
use crate::bash_rewrite::rules::{
    CatAppendRule, CatRule, FindRule, GrepRule, HeadRule, LsRule, RgRule, SedRule, TailRule,
};
use crate::bash_rewrite::RewriteRule;
use crate::context::AppContext;
use crate::protocol::Response;

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
    dispatch_for_request(command, "bash_rewrite", session_id, ctx)
}

pub fn dispatch_for_request(
    command: &str,
    request_id: &str,
    session_id: Option<&str>,
    ctx: &AppContext,
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

    let rules: [&dyn RewriteRule; 9] = [
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
        let decision = rule.decide(command, request_id, session_id, ctx);
        match decision {
            crate::bash_rewrite::RewriteDecision::Accept(request) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_rewrite::catalog::ControlRole;

    #[test]
    fn route_records_are_request_correlated_and_replaceable() {
        record_native(
            "route-test",
            ControlRole::Native,
            "dispatch.native.no_rule",
            "test",
        );
        assert_eq!(
            route_record("route-test").expect("route record").request_id,
            "route-test"
        );
        record_native(
            "route-test",
            ControlRole::Sandbox,
            "dispatch.native.sandbox",
            "test",
        );
        assert_eq!(
            route_record("route-test").expect("replacement").route,
            DispatchRoute::Native {
                role: "sandbox".to_string(),
                branch_id: "dispatch.native.sandbox".to_string(),
                reason: "test".to_string(),
            }
        );
        let _ = take_route_record("route-test");
    }

    #[test]
    fn audit_growth_route_records_are_bounded_without_losing_the_latest_pending_record() {
        const MAX_RECORDS: usize = 1024;
        let prefix = format!("route-growth-{}-", std::process::id());
        for index in 0..(MAX_RECORDS + 128) {
            record_native(
                &format!("{prefix}{index}"),
                ControlRole::Native,
                "dispatch.native.no_rule",
                "test",
            );
        }

        let retained = route_records()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .keys()
            .filter(|request_id| request_id.starts_with(&prefix))
            .count();
        assert!(retained <= MAX_RECORDS, "retained {retained} route records");
        let latest = format!("{prefix}{}", MAX_RECORDS + 127);
        assert!(
            route_record(&latest).is_some(),
            "latest record remains readable"
        );
        assert!(
            take_route_record(&latest).is_some(),
            "caller can consume its record"
        );
    }
}
