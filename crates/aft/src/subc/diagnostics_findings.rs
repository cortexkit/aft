//! Read-only diagnoses from evidence the serving process already keeps.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_protocol::{
    scope::{ScopeKind, ScopeStamp},
    ErrorBody,
};

pub(super) const OPERATION: &str = "diagnostics.findings";

const DEFAULT_WINDOW_MS: u64 = 21_600_000;
const MAX_WINDOW_MS: u64 = 604_800_000;
const BODY_LIMIT: usize = 65_536;

#[derive(Debug, Default, Deserialize)]
struct Params {
    window_ms: Option<u64>,
}

fn window_ms(params: &Value) -> Result<u64, String> {
    let params: Params = serde_json::from_value(if params.is_null() {
        json!({})
    } else {
        params.clone()
    })
    .map_err(|_| {
        "diagnostics.findings params must be an object with an integer window_ms".to_owned()
    })?;
    let window = params.window_ms.unwrap_or(DEFAULT_WINDOW_MS);
    if !(1..=MAX_WINDOW_MS).contains(&window) {
        return Err("window_ms must be between 1 and 604800000 milliseconds".to_owned());
    }
    Ok(window)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Error,
    #[serde(alias = "warning")]
    Warn,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Finding {
    check: String,
    key: String,
    severity: Severity,
    count: u64,
    first_seen_ms: u64,
    last_seen_ms: u64,
    summary: String,
    samples: Vec<Value>,
    hint: String,
}

pub(super) fn is_request(body: &Value) -> bool {
    body.get("method")
        .or_else(|| body.get("op"))
        .and_then(Value::as_str)
        == Some(OPERATION)
}

fn admit(scope: Option<&ScopeStamp>, now_ms: u64) -> Result<(), ErrorBody> {
    let refused = || {
        ErrorBody::new(
            "permission_denied",
            "diagnostics.findings requires an authorized agent head or daemon-stamped flow route",
        )
    };
    let scope = scope.ok_or_else(refused)?;
    if !scope.owner_authorized
        || scope.scope_epoch == 0
        || scope.expires_at_ms.is_some_and(|at| at <= now_ms)
        || scope.kind == ScopeKind::Worker
        || scope.attributes.on_behalf_of_agent.is_some()
        || scope.attributes.run_id.is_some()
    {
        return Err(refused());
    }
    if scope
        .attributes
        .flow_id
        .as_ref()
        .is_some_and(|id| !id.is_empty())
        || (scope.kind == ScopeKind::Head
            && scope
                .attributes
                .agent_id
                .as_ref()
                .is_some_and(|id| !id.is_empty()))
    {
        Ok(())
    } else {
        Err(refused())
    }
}

pub(super) async fn dispatch(
    tx: &super::WriterSender,
    metrics: &super::DispatchPathMetrics,
    frame: &super::Frame,
    scope: Option<&ScopeStamp>,
    envelope: &Value,
    cache: &super::HealthRollupCache,
    executor: &crate::executor::Executor,
) -> Result<(), super::SubcError> {
    match reply(scope, envelope, cache, executor) {
        Err(error) => super::send_provider_error(tx, metrics, frame, error).await,
        Ok(body) => {
            let response = super::Frame::build_with_version(
                frame.header.ver,
                super::FrameType::Response,
                frame.header.flags,
                frame.header.channel,
                frame.header.epoch,
                frame.header.corr,
                serde_json::to_vec(&body).map_err(super::SubcError::Json)?,
            )
            .map_err(super::SubcError::FrameBuild)?;
            super::send_reliable_writer_frame(tx, metrics, response, "diagnostics findings").await
        }
    }
}

fn reply(
    scope: Option<&ScopeStamp>,
    envelope: &Value,
    cache: &super::HealthRollupCache,
    executor: &crate::executor::Executor,
) -> Result<Value, ErrorBody> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ErrorBody::new("clock_unavailable", "Unix-epoch clock unavailable"))?
        .as_millis() as u64;
    admit(scope, now)?;
    let window = window_ms(&envelope["params"])
        .map_err(|message| ErrorBody::new("invalid_params", message))?;
    let mut findings = cache.findings(now, window);
    collect_executor(
        &super::health::dispatch_liveness_metrics(executor),
        now,
        &mut findings,
    );
    collect_cold_waits(
        crate::cold_build_limiter::global_limiter().try_overdue_waits(now, LIVENESS_BOUND_MS),
        now,
        &mut findings,
    );
    collect_floor_refusals(
        crate::persisted_format::try_floor_refusal_counts(),
        now,
        &mut findings,
    );
    Ok(bounded_body(Findings {
        generated_at_ms: now,
        window_ms: window,
        findings,
    }))
}

fn collect_cold_waits(
    waits: Result<(u64, u64), &'static str>,
    now: u64,
    findings: &mut Vec<Finding>,
) {
    match waits {
        Ok((count, first)) if count > 0 => findings.push(condition("cold_build_slot_waits", "overdue", Severity::Warn, count, first, now,
            "Cold-build slot waiters exceed the 60-second diagnostic bound.", vec![json!({"bound_ms": LIVENESS_BOUND_MS})],
            "Read health cold_build_limiter and indexing for current slot holders; completed waits are not retained.")),
        Ok(_) => {},
        Err(reason) => findings.push(unknown("cold_build_slot_waits", reason, now)),
    }
}

fn collect_floor_refusals(
    counts: Option<std::collections::BTreeMap<String, u64>>,
    now: u64,
    findings: &mut Vec<Finding>,
) {
    match counts {
        Some(counts) => {
            for (key, count) in counts {
                findings.push(condition("storage_reader_floor", &key, Severity::Error, count, now, now,
                "Storage is refused by its reader floor.", vec![], "Read status storage refusal details; use a reader compatible with the recorded floor."));
            }
        }
        None => findings.push(unknown(
            "storage_reader_floor",
            "registry_busy_poisoned_or_scan_deadline",
            now,
        )),
    }
}

const LIVENESS_BOUND_MS: u64 = 60_000;
const SNAPSHOT_MAX_AGE_MS: u64 = 30_000;

#[allow(clippy::too_many_arguments)]
fn condition(
    check: &str,
    key: &str,
    severity: Severity,
    count: u64,
    first: u64,
    now: u64,
    summary: &str,
    samples: Vec<Value>,
    hint: &str,
) -> Finding {
    Finding {
        check: check.to_owned(),
        key: key.to_owned(),
        severity,
        count,
        first_seen_ms: first.min(now),
        last_seen_ms: now,
        summary: summary.to_owned(),
        samples,
        hint: hint.to_owned(),
    }
}

fn unknown(check: &str, reason: &str, now: u64) -> Finding {
    condition(
        check,
        &format!("unavailable/{reason}"),
        Severity::Error,
        1,
        now,
        now,
        "AFT could not determine this check from its recorded evidence.",
        vec![json!({"reason": reason})],
        "Read AFT health for evidence availability; an unavailable check is not a healthy result.",
    )
}

fn root_key(root: &Value) -> String {
    let label = root["project_root"].as_str().unwrap_or("unidentified-root");
    format!("{:x}", Sha256::digest(label.as_bytes()))
}

pub(super) fn from_health(
    metrics: &Value,
    age_ms: Option<u64>,
    stale_reason: Option<&str>,
    now: u64,
    window: u64,
) -> Vec<Finding> {
    let mut findings = vec![];
    if stale_reason.is_some() || age_ms.is_none_or(|age| age > SNAPSHOT_MAX_AGE_MS) {
        findings.push(unknown(
            "health_snapshot",
            stale_reason.unwrap_or("snapshot_older_than_30s_or_unmeasured"),
            now,
        ));
        return findings;
    }
    let observed = now.saturating_sub(age_ms.unwrap_or(0));
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    for field in ["root_details_omitted", "cache_unavailable_omitted"] {
        if metrics[field].as_u64().unwrap_or(0) > 0 {
            findings.push(unknown("health_snapshot", field, now));
        }
    }
    match metrics["embedding_backend"]["available"].as_bool() {
        Some(false) => findings.push(condition("semantic_backend", "unavailable", Severity::Error, 1,
            metrics["embedding_backend"]["since_ms"].as_u64().unwrap_or(observed), now,
            "The semantic embedding backend is unavailable or paused.", vec![],
            "Read health embedding_backend and semantic_index; inspect synapse availability and retry state.")),
        Some(true) => {},
        None => findings.push(unknown("semantic_backend", "backend_state_unmeasured", now)),
    }
    if let Some(roots) = metrics["roots"].as_array() {
        for root in roots {
            if std::time::Instant::now() >= deadline {
                findings.push(unknown("health_snapshot", "root_scan_deadline", now));
                break;
            }
            let key = root_key(root);
            let evidence = &root["diagnostics_evidence"];
            if root["state"] == "busy" || !evidence.is_object() {
                findings.push(unknown(
                    "root_health",
                    &format!("{key}/busy_or_unhosted"),
                    now,
                ));
                continue;
            }
            if root["cache_unavailable_reason"].as_str().is_some() {
                findings.push(condition(
                    "storage_open",
                    &key,
                    Severity::Error,
                    1,
                    observed,
                    now,
                    "A root's storage is unavailable, including possible reader-floor refusal.",
                    vec![],
                    "Read health cache_unavailable_reason and status storage refusal details.",
                ));
            }
            if let Some(reason) = evidence["build_breaker_error"].as_str() {
                findings.push(unknown("build_breaker", &format!("{key}/{reason}"), now));
            } else if let Some(domains) = root["suspended_domains"].as_array() {
                for domain in domains {
                    if std::time::Instant::now() >= deadline {
                        findings.push(unknown("build_breaker", "domain_scan_deadline", now));
                        return findings;
                    }
                    let domain_name = domain["domain"].as_str().unwrap_or("unknown");
                    // Hash labels rather than returning roots, custom server names or error text.
                    let domain_key = format!("{key}/{:x}", Sha256::digest(domain_name.as_bytes()));
                    findings.push(condition("build_breaker", &domain_key, Severity::Error, 1,
                        observed.saturating_sub(domain["age_s"].as_u64().unwrap_or(0).saturating_mul(1000)), now,
                        "A build domain is currently suspended by the death breaker.",
                        vec![json!({"death_count": domain["death_count"].as_u64().unwrap_or(0)})],
                        "Read health suspended_domains; inspect build_breaker before attempting a reset."));
                }
            }
            if evidence["database_open_failed"] == true {
                findings.push(condition(
                    "database_open",
                    &key,
                    Severity::Error,
                    1,
                    observed,
                    now,
                    "A hosted root's database open is failed or busy.",
                    vec![],
                    "Read status database runtime state and storage compatibility diagnostics.",
                ));
            } else if evidence["database_open_failed"].as_bool().is_none() {
                findings.push(unknown("database_open", &key, now));
            }
            if let Some(failures) = evidence["lsp_start_failures"].as_object() {
                for (server, count) in failures {
                    if std::time::Instant::now() >= deadline {
                        findings.push(unknown("lsp_start_failures", "server_scan_deadline", now));
                        return findings;
                    }
                    if let Some(count) = count.as_u64().filter(|n| *n > 0) {
                        let server_key = format!("{key}/{:x}", Sha256::digest(server.as_bytes()));
                        findings.push(condition("lsp_start_failures", &server_key, Severity::Error, count, observed, now,
                            "Language-server starts remain in cached failure or retry backoff.", vec![],
                            "Read LSP status and startup logs; the count is failed server/root entries, not crash attempts."));
                    }
                }
            } else {
                findings.push(unknown(
                    "lsp_start_failures",
                    &format!("{key}/manager_busy"),
                    now,
                ));
            }
        }
    } else {
        findings.push(unknown("root_health", "roots_unmeasured", now));
    }
    collect_cleanup(metrics, now, window, &mut findings);
    findings
}

fn collect_executor(metrics: &Value, now: u64, findings: &mut Vec<Finding>) {
    if metrics["scheduler_busy"] == true {
        findings.push(unknown("executor_liveness", "scheduler_busy", now));
        return;
    }
    for (key, age) in [
        (
            "interactive_queue",
            &metrics["interactive"]["oldest_age_ms"],
        ),
        (
            "maintenance_queue",
            &metrics["maintenance"]["oldest_age_ms"],
        ),
        (
            "maintenance_worker",
            &metrics["running"]["oldest_worker_owned_maintenance_age_ms"],
        ),
    ] {
        if let Some(age) = age.as_u64().filter(|age| *age > LIVENESS_BOUND_MS) {
            findings.push(condition("executor_liveness", key, Severity::Warn, 1, now.saturating_sub(age), now,
                "The oldest executor job exceeds the 60-second diagnostic liveness bound.",
                vec![json!({"age_ms": age, "bound_ms": LIVENESS_BOUND_MS})],
                "Read health dispatch_liveness and dispatch_path; queue ages are standing evidence, not a per-job deadline."));
        }
    }
    if let Some(count) = metrics["phantom_running_maintenance"]
        .as_u64()
        .filter(|n| *n > 0)
    {
        findings.push(condition("executor_liveness", "phantom_maintenance", Severity::Error, count, now, now,
            "Maintenance jobs have no worker or completion owner.", vec![], "Read health dispatch_liveness completion ownership before investigating executor progress."));
    }
}

fn collect_cleanup(metrics: &Value, now: u64, window: u64, findings: &mut Vec<Finding>) {
    let Some(reports) = metrics["storage_retention"].as_object() else {
        findings.push(unknown("cleanup_failures", "retention_unmeasured", now));
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
    for (storage, report) in reports {
        if std::time::Instant::now() >= deadline {
            findings.push(unknown("cleanup_failures", "report_scan_deadline", now));
            break;
        }
        let key = format!("{:x}", Sha256::digest(storage.as_bytes()));
        let Some(at) = report["at_ms"].as_u64() else {
            findings.push(unknown("cleanup_failures", "report_clock_unmeasured", now));
            continue;
        };
        if at >= now.saturating_sub(window) && at <= now {
            if let Some(errors) = report["errors"].as_array().filter(|e| !e.is_empty()) {
                findings.push(condition("cleanup_failures", &format!("{key}/retention"), Severity::Warn, errors.len() as u64, at, at,
                    "The latest retention pass recorded cleanup failures.", vec![],
                    "Read health storage_retention and retention logs; only the latest report per storage root is retained."));
            }
        }
        if let Some(reasons) = report["trash"]["kept_by_reason"].as_object() {
            for (reason, count) in reasons {
                if let Some(count) = count.as_u64().filter(|n| *n > 0) {
                    // Live/protected/recent trash is intentional, not a deletion failure.
                    if matches!(
                        reason.as_str(),
                        "unlink_guard"
                            | "unlink_status"
                            | "unlink_pending"
                            | "enumeration_error"
                            | "identity"
                            | "metadata"
                            | "open_parent"
                            | "open_child"
                            | "tree_guard"
                            | "mount_guard"
                            | "protections"
                            | "witness"
                            | "trash_container"
                            | "reap_lock"
                            | "reaper_pass"
                    ) {
                        findings.push(condition("cleanup_failures", &format!("{key}/trash/{reason}"), Severity::Warn, count,
                            report["trash"]["at_ms"].as_u64().unwrap_or(at), now,
                            "The last trash census recorded entries it could not clean or assess.", vec![],
                            "Read health storage_retention trash kept_by_reason and reaper logs; do not delete protected storage manually."));
                    }
                }
            }
        }
        if report["trash"]["incomplete"] == true {
            findings.push(unknown(
                "cleanup_failures",
                &format!("{key}/trash_census_incomplete"),
                now,
            ));
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Findings {
    generated_at_ms: u64,
    window_ms: u64,
    findings: Vec<Finding>,
}

fn bounded_body(mut result: Findings) -> Value {
    result.findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.check.cmp(&b.check))
            .then_with(|| a.key.cmp(&b.key))
    });
    for finding in &mut result.findings {
        finding.samples.truncate(5);
    }
    let mut size = serde_json::to_vec(&json!({"result": &result}))
        .expect("serializable findings")
        .len();
    // Match core's priority: discard samples before discarding diagnoses.
    // Subtract exact JSON lengths so a large evidence set is not re-encoded
    // in full after every removed sample.
    for finding in &mut result.findings {
        while size >= BODY_LIMIT && !finding.samples.is_empty() {
            let sample = finding.samples.pop().expect("nonempty samples");
            size -= serde_json::to_vec(&sample)
                .expect("serializable sample")
                .len()
                + usize::from(!finding.samples.is_empty());
        }
    }
    while size >= BODY_LIMIT {
        let finding = result
            .findings
            .pop()
            .expect("empty reply fits the body bound");
        size -= serde_json::to_vec(&finding)
            .expect("serializable finding")
            .len()
            + usize::from(!result.findings.is_empty());
    }
    json!({"result": result})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_metrics() -> Value {
        json!({"embedding_backend": {"available": true}, "root_details_omitted": 0,
            "cache_unavailable_omitted": 0,
            "roots": [{"project_root": "/private/user/project", "state": "ready",
                "diagnostics_evidence": {"build_breaker_error": null,
                    "database_open_failed": false, "lsp_start_failures": {}}}],
            "storage_retention": {}})
    }

    fn snapshot(metrics: &Value) -> Vec<Finding> {
        from_health(metrics, Some(0), None, 1_000_000, 60_000)
    }

    #[test]
    fn diagnostics_findings_healthy_snapshot_is_empty() {
        assert!(snapshot(&healthy_metrics()).is_empty());
        let mut findings = vec![];
        collect_executor(
            &json!({"interactive": {"queued": 0}, "maintenance": {"queued": 0}, "running": {}, "phantom_running_maintenance": 0}),
            1_000_000,
            &mut findings,
        );
        collect_cold_waits(Ok((0, 1_000_000)), 1_000_000, &mut findings);
        collect_floor_refusals(Some(Default::default()), 1_000_000, &mut findings);
        assert!(findings.is_empty());
    }

    #[test]
    fn diagnostics_findings_seeded_standing_conditions_and_redaction() {
        let mut metrics = healthy_metrics();
        metrics["roots"][0]["suspended_domains"] = json!([{"domain": "semantic", "death_count": 4, "age_s": 5000, "reason": "secret file contents"}]);
        metrics["roots"][0]["diagnostics_evidence"]["database_open_failed"] = json!(true);
        metrics["roots"][0]["diagnostics_evidence"]["lsp_start_failures"] =
            json!({"custom/private-server": 3});
        metrics["roots"][0]["cache_unavailable_reason"] = json!("storage_requires_newer_reader");
        metrics["embedding_backend"] = json!({"available": false, "since_ms": 10, "last_error": "https://user:secret@host/private"});
        let findings = snapshot(&metrics);
        for check in [
            "build_breaker",
            "database_open",
            "lsp_start_failures",
            "storage_open",
            "semantic_backend",
        ] {
            assert!(findings.iter().any(|f| f.check == check), "missing {check}");
        }
        assert_eq!(
            findings
                .iter()
                .find(|f| f.check == "lsp_start_failures")
                .unwrap()
                .count,
            3
        );
        let body = bounded_body(Findings {
            generated_at_ms: 1_000_000,
            window_ms: 60_000,
            findings,
        });
        let text = serde_json::to_string(&body).unwrap();
        for sensitive in [
            "/private/user/project",
            "custom/private-server",
            "secret",
            "https://",
        ] {
            assert!(!text.contains(sensitive), "leaked {sensitive}");
        }
        assert_eq!(
            body["result"]["findings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["check"] == "semantic_backend")
                .unwrap()["first_seen_ms"],
            10
        );
    }

    #[test]
    fn diagnostics_findings_seeded_executor_cold_and_floor_conditions() {
        let mut findings = vec![];
        collect_executor(
            &json!({"interactive": {"oldest_age_ms": 60_001},
            "maintenance": {"oldest_age_ms": 70_000},
            "running": {"oldest_worker_owned_maintenance_age_ms": 80_000},
            "phantom_running_maintenance": 2}),
            1_000_000,
            &mut findings,
        );
        assert_eq!(findings.len(), 4);
        collect_cold_waits(Ok((7, 100)), 1_000_000, &mut findings);
        collect_floor_refusals(
            Some(std::collections::BTreeMap::from([(
                "aft_db/requires_newer_reader".to_owned(),
                2,
            )])),
            1_000_000,
            &mut findings,
        );
        let cold = findings
            .iter()
            .find(|f| f.check == "cold_build_slot_waits")
            .unwrap();
        assert_eq!((cold.count, cold.first_seen_ms), (7, 100));
        assert_eq!(
            findings
                .iter()
                .find(|f| f.check == "storage_reader_floor")
                .unwrap()
                .count,
            2
        );
        let mut boundary = vec![];
        collect_executor(
            &json!({"interactive": {"oldest_age_ms": 60_000}, "maintenance": {}, "running": {}}),
            1_000_000,
            &mut boundary,
        );
        assert!(boundary.is_empty());
    }

    #[test]
    fn diagnostics_findings_cleanup_is_counted_and_windowed() {
        let mut metrics = healthy_metrics();
        metrics["storage_retention"] = json!({"/private/cache": {"at_ms": 950_000,
            "errors": ["secret/path", "secret/error"],
            "trash": {"at_ms": 950_000, "incomplete": false,
                "kept_by_reason": {"unlink_guard": 4, "bind_pending": 10}}}});
        let findings = snapshot(&metrics);
        assert_eq!(findings.len(), 2);
        assert_eq!(
            findings
                .iter()
                .find(|f| f.key.ends_with("/retention"))
                .unwrap()
                .count,
            2
        );
        assert_eq!(
            findings
                .iter()
                .find(|f| f.key.ends_with("/trash/unlink_guard"))
                .unwrap()
                .count,
            4
        );
        metrics["storage_retention"]["/private/cache"]["at_ms"] = json!(939_999);
        let findings = snapshot(&metrics);
        assert_eq!(
            findings.len(),
            1,
            "standing trash survives an older pass; retention history does not"
        );
        assert!(findings[0].key.ends_with("/trash/unlink_guard"));
    }

    #[test]
    fn diagnostics_findings_unknown_is_error_not_healthy() {
        let metrics = healthy_metrics();
        for (age, reason) in [
            (None, None),
            (Some(30_001), None),
            (Some(0), Some("cache_contended")),
        ] {
            let findings = from_health(&metrics, age, reason, 1_000_000, 1);
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].severity, Severity::Error);
        }
        let mut metrics = metrics;
        metrics["roots"][0]["diagnostics_evidence"]["build_breaker_error"] =
            json!("breaker_read_failed");
        metrics["roots"][0]["diagnostics_evidence"]["lsp_start_failures"] = Value::Null;
        metrics["root_details_omitted"] = json!(9);
        let mut findings = snapshot(&metrics);
        collect_cold_waits(Err("limiter_busy_or_poisoned"), 1_000_000, &mut findings);
        collect_floor_refusals(None, 1_000_000, &mut findings);
        collect_executor(&json!({"scheduler_busy": true}), 1_000_000, &mut findings);
        assert_eq!(findings.len(), 6);
        assert!(findings
            .iter()
            .all(|f| f.severity == Severity::Error && f.key.contains("unavailable")));
    }

    fn scope(kind: ScopeKind) -> ScopeStamp {
        let mut stamp = ScopeStamp::new(
            subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".to_owned(),
            },
            "test-scope",
            1,
            kind,
        );
        stamp.owner_authorized = true;
        stamp.attributes.agent_id = Some("alf".to_owned());
        stamp
    }

    #[test]
    fn diagnostics_findings_head_and_daemon_flow_admission() {
        assert!(admit(Some(&scope(ScopeKind::Head)), 100).is_ok());
        let mut flow = scope(ScopeKind::Ephemeral);
        flow.attributes.agent_id = None;
        flow.attributes.flow_id = Some("fleet-self-diagnosis".to_owned());
        assert!(admit(Some(&flow), 100).is_ok());
        flow.owner_authorized = false;
        assert!(admit(Some(&flow), 100).is_err());
        flow.owner_authorized = true;
        flow.expires_at_ms = Some(100);
        assert!(admit(Some(&flow), 100).is_err());
        assert!(admit(None, 100).is_err());
        assert!(admit(Some(&scope(ScopeKind::Worker)), 100).is_err());
        assert!(admit(Some(&scope(ScopeKind::Ephemeral)), 100).is_err());
        let mut run = scope(ScopeKind::Head);
        run.attributes.run_id = Some("worker-run".to_owned());
        assert!(admit(Some(&run), 100).is_err());
    }

    #[tokio::test]
    async fn diagnostics_findings_worker_route_refuses_forged_flow_params() {
        let (writer, mut replies) = tokio::sync::mpsc::channel(2);
        let envelope =
            json!({"method": OPERATION, "params": {"flow_id": "fake", "owner_authorized": true}});
        let frame = super::super::Frame::build(
            super::super::FrameType::Request,
            super::super::control_flags(),
            41,
            1,
            7,
            serde_json::to_vec(&envelope).unwrap(),
        )
        .unwrap();
        dispatch(
            &writer,
            &super::super::DispatchPathMetrics::new(),
            &frame,
            Some(&scope(ScopeKind::Worker)),
            &envelope,
            &super::super::HealthRollupCache::new(),
            &crate::executor::Executor::new(),
        )
        .await
        .unwrap();
        let answer = replies.recv().await.unwrap();
        assert_eq!(answer.header.ty, super::super::FrameType::Error);
        assert_eq!(answer.header.corr, 7);
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(body["code"], "permission_denied");
    }

    #[test]
    fn diagnostics_findings_manifest_declares_flow_query_not_tool() {
        let manifest = super::super::manifest::build_manifest();
        manifest.validate_flow_operations().unwrap();
        assert_eq!(manifest.flow_operations.as_ref().unwrap(), &[OPERATION]);
        let manifest = serde_json::to_value(manifest).unwrap();
        assert!(manifest["capabilities"]["provides"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == subc_protocol::scope::FLOW_SCOPES_CAPABILITY));
        let providers = manifest["provides"].as_array().unwrap();
        assert!(providers
            .iter()
            .any(|p| p["operations"].as_array().is_some_and(|ops| ops
                .iter()
                .any(|op| op["name"] == OPERATION && op["kind"] == "query"))));
        assert!(!providers.iter().any(|p| p["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == OPERATION))));
    }

    #[tokio::test]
    async fn flow_scoped_route_serves_only_diagnostics_findings() {
        use super::super::*;

        async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let frame = read_frame(daemon).await.unwrap().unwrap();
                    if frame.header.channel == channel && frame.header.corr == corr {
                        return frame;
                    }
                }
            })
            .await
            .expect("flow route response timed out")
        }

        async fn request(
            daemon: &mut tokio::io::DuplexStream,
            channel: u16,
            corr: u64,
            body: Value,
        ) -> Frame {
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                channel,
                u32::from(channel != 0),
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            write_frame(daemon, &frame).await.unwrap();
            receive(daemon, channel, corr).await
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("no-project-setup");
        let module_root = root.clone();
        let (mut daemon, module) = tokio::io::duplex(64 * 1024);
        let module_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (read, write) = tokio::io::split(module);
            runtime.block_on(run_module_loop(
                read,
                write,
                &module_root.join("absent-connection.json"),
                App::default_shared(),
                Arc::new(Executor::new()),
                |_, _| panic!("flow scope must not execute commands"),
                None,
                false,
                usize::MAX,
                None,
                &module_root.join("storage"),
                QuiescedLsp::ReapInBackground,
                None,
                None,
            ))
        });
        assert_eq!(
            receive(&mut daemon, 0, HELLO_CORR).await.header.ty,
            FrameType::Hello
        );
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::HelloAck,
                control_flags(),
                0,
                0,
                HELLO_CORR,
                b"{}".to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let mut flow = scope(ScopeKind::Ephemeral);
        flow.attributes.agent_id = None;
        flow.attributes.flow_id = Some("fleet-self-diagnosis".into());
        let bind = |channel, target, role_versions| ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target,
            identity: subc_protocol::BindIdentity::new(&root, "alfonso", "flow-diagnostics"),
            principal: Some(Principal::Reserved {
                module_id: "prefrontal-core".into(),
            }),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: Some(flow.clone()),
            role_versions,
            call_mode: Some(subc_protocol::call_mode::CallMode::Flow),
        };
        let ack = request(
            &mut daemon,
            0,
            41,
            serde_json::to_value(bind(
                41,
                RouteTarget::ManagementSurface {
                    module_id: "aft".into(),
                },
                None,
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(ack.header.ty, FrameType::Response, "{ack:?}");
        assert!(matches!(
            serde_json::from_slice::<ModuleControlResponse>(&ack.body).unwrap(),
            ModuleControlResponse::RouteBindAck {}
        ));

        let findings = request(
            &mut daemon,
            41,
            42,
            json!({"method": OPERATION, "params": {}}),
        )
        .await;
        assert_eq!(findings.header.ty, FrameType::Response, "{findings:?}");
        let findings: Value = serde_json::from_slice(&findings.body).unwrap();
        assert!(findings["result"]["findings"].is_array(), "{findings}");

        // Include provider reads and unknown operations: none may fall through
        // to a database read or a general tool just because it lacks a method.
        for body in [
            json!({"method": "memory.census", "params": {}}),
            json!({"op": "tool.call", "name": "read", "arguments": {"path": "secret"}}),
            json!({"name": "read", "arguments": {"path": "secret"}}),
            json!({"op": "tool.list"}),
            json!({"method": "unknown.operation"}),
        ] {
            let answer = request(&mut daemon, 41, 43, body).await;
            assert_eq!(answer.header.ty, FrameType::Error, "{answer:?}");
            let error: ErrorBody = serde_json::from_slice(&answer.body).unwrap();
            assert_eq!(error.code, "flow_scope_not_supported");
        }

        for (channel, role_versions) in [
            (44, None),
            (
                45,
                Some(std::collections::BTreeMap::from([(
                    "tool-provider".into(),
                    "v1".into(),
                )])),
            ),
        ] {
            let answer = request(
                &mut daemon,
                0,
                u64::from(channel),
                serde_json::to_value(bind(
                    channel,
                    RouteTarget::ToolProvider {
                        module_id: "aft".into(),
                    },
                    role_versions,
                ))
                .unwrap(),
            )
            .await;
            assert_eq!(answer.header.ty, FrameType::Error, "{answer:?}");
            let error: ErrorBody = serde_json::from_slice(&answer.body).unwrap();
            assert_eq!(error.code, "flow_scope_not_supported");
        }
        drop(daemon);
        module_thread.join().unwrap().unwrap();
        assert!(
            !root.exists(),
            "flow admission must not initialize a project or database"
        );
    }

    #[test]
    fn diagnostics_findings_serving_replies_match_vector_shape_and_windows() {
        let cache = super::super::HealthRollupCache::new();
        let executor = crate::executor::Executor::new();
        let head = scope(ScopeKind::Head);
        let mut flow = scope(ScopeKind::Ephemeral);
        flow.attributes.agent_id = None;
        flow.attributes.flow_id = Some("fleet-self-diagnosis".to_owned());
        for stamp in [&head, &flow] {
            for (bytes, expected_window) in
                VECTORS[..3]
                    .iter()
                    .zip([DEFAULT_WINDOW_MS, 60_000, MAX_WINDOW_MS])
            {
                let request = serde_json::from_str(bytes).unwrap();
                let body = reply(Some(stamp), &request, &cache, &executor).unwrap();
                let result: Findings = serde_json::from_value(body["result"].clone()).unwrap();
                assert_eq!(result.window_ms, expected_window);
                assert!(result.generated_at_ms > 1_000_000_000_000);
                assert!(result
                    .findings
                    .iter()
                    .all(|f| f.count > 0 && f.samples.len() <= 5));
                assert!(
                    result.findings.iter().any(|f| f.check == "health_snapshot"),
                    "an unmeasured cache is not healthy"
                );
            }
        }
    }

    const VECTORS: [&str; 6] = [
        include_str!("../../tests/fixtures/diagnostics-findings-v1/request-empty.json"),
        include_str!("../../tests/fixtures/diagnostics-findings-v1/request-window.json"),
        include_str!("../../tests/fixtures/diagnostics-findings-v1/request-max-window.json"),
        include_str!("../../tests/fixtures/diagnostics-findings-v1/reply-healthy.json"),
        include_str!("../../tests/fixtures/diagnostics-findings-v1/reply-core-findings.json"),
        include_str!("../../tests/fixtures/diagnostics-findings-v1/reply-routing-findings.json"),
    ];

    #[test]
    fn diagnostics_findings_vectors_preserve_pinned_bytes() {
        let provenance = include_str!("../../tests/fixtures/diagnostics-findings-v1/PROVENANCE.md");
        for (name, bytes) in [
            ("request-empty.json", VECTORS[0]),
            ("request-window.json", VECTORS[1]),
            ("request-max-window.json", VECTORS[2]),
            ("reply-healthy.json", VECTORS[3]),
            ("reply-core-findings.json", VECTORS[4]),
            ("reply-routing-findings.json", VECTORS[5]),
            (
                "README.md",
                include_str!("../../tests/fixtures/diagnostics-findings-v1/README.md"),
            ),
        ] {
            let digest = format!("{:x}", Sha256::digest(bytes.as_bytes()));
            assert!(
                provenance.contains(&format!("| `{name}` | `{digest}` |")),
                "digest mismatch for {name}"
            );
        }
    }

    #[test]
    fn diagnostics_findings_vector_conformance() {
        for (vector, expected) in
            VECTORS[..3]
                .iter()
                .zip([DEFAULT_WINDOW_MS, 60_000, MAX_WINDOW_MS])
        {
            let request: Value = serde_json::from_str(vector).unwrap();
            assert_eq!(request["method"], "diagnostics.findings");
            assert_eq!(window_ms(&request["params"]).unwrap(), expected);
        }
        for vector in &VECTORS[3..] {
            let body: Value = serde_json::from_str(vector).unwrap();
            let reply: Findings = serde_json::from_value(body["result"].clone()).unwrap();
            assert_eq!(reply.window_ms, DEFAULT_WINDOW_MS);
            for finding in &reply.findings {
                assert!(finding.count > 0);
                assert!(finding.samples.len() <= 5);
                assert!(finding.samples.iter().all(Value::is_object));
                assert!(finding.first_seen_ms <= finding.last_seen_ms);
            }
            let serialized = bounded_body(reply);
            assert!(serde_json::to_vec(&serialized).unwrap().len() < BODY_LIMIT);
            // Routing's legacy spelling is accepted, but never emitted by AFT.
            assert!(serialized["result"]["findings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["severity"] != "warning"));
        }
        let healthy: Value = serde_json::from_str(VECTORS[3]).unwrap();
        assert_eq!(
            bounded_body(Findings {
                generated_at_ms: healthy["result"]["generated_at_ms"].as_u64().unwrap(),
                window_ms: DEFAULT_WINDOW_MS,
                findings: vec![],
            }),
            healthy
        );
    }

    #[test]
    fn diagnostics_findings_window_validation() {
        for params in [Value::Null, json!({}), json!({"window_ms": null})] {
            assert_eq!(window_ms(&params).unwrap(), DEFAULT_WINDOW_MS);
        }
        for window in [1, MAX_WINDOW_MS] {
            assert_eq!(window_ms(&json!({"window_ms": window})).unwrap(), window);
        }
        for params in [
            json!({"window_ms": 0}),
            json!({"window_ms": MAX_WINDOW_MS + 1}),
            json!({"window_ms": -1}),
            json!({"window_ms": 1.5}),
            json!({"window_ms": "1"}),
            json!({"window_ms": true}),
            json!([]),
            json!(42),
        ] {
            assert!(window_ms(&params).is_err(), "accepted {params}");
        }
    }

    fn finding(check: &str, severity: Severity, count: u64) -> Finding {
        Finding {
            check: check.to_owned(),
            key: check.to_owned(),
            severity,
            count,
            first_seen_ms: 1,
            last_seen_ms: 2,
            summary: "Seeded condition.".to_owned(),
            samples: vec![json!({"code": "seeded"})],
            hint: "Read health.".to_owned(),
        }
    }

    #[test]
    fn diagnostics_findings_order_and_body_cap_preserve_counts() {
        let body = bounded_body(Findings {
            generated_at_ms: 3,
            window_ms: 1,
            findings: vec![
                finding("z", Severity::Warn, 10),
                finding("b", Severity::Error, 2),
                finding("a", Severity::Error, 2),
                finding("c", Severity::Error, 3),
            ],
        });
        let names: Vec<_> = body["result"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["check"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["c", "a", "b", "z"]);

        let mut findings = vec![];
        for index in 0..400 {
            let mut f = finding(&format!("condition_{index:04}"), Severity::Warn, 77);
            f.summary = "bounded text ".repeat(50);
            f.samples = vec![json!({"detail": "x".repeat(4000)}); 7];
            findings.push(f);
        }
        let body = bounded_body(Findings {
            generated_at_ms: 3,
            window_ms: 1,
            findings,
        });
        assert!(serde_json::to_vec(&body).unwrap().len() < BODY_LIMIT);
        let kept = body["result"]["findings"].as_array().unwrap();
        assert!(!kept.is_empty() && kept.len() < 400);
        assert!(kept
            .iter()
            .all(|f| f["count"] == 77 && f["samples"] == json!([])));
        assert_eq!(kept[0]["check"], "condition_0000");
    }

    #[test]
    fn diagnostics_findings_trims_samples_from_most_severe_before_tail_findings() {
        let mut error = finding("error", Severity::Error, 9);
        error.samples = vec![json!({"detail": "ü".repeat(7000)}); 5];
        let warn = finding("warn", Severity::Warn, 10);
        let body = bounded_body(Findings {
            generated_at_ms: 3,
            window_ms: 1,
            findings: vec![warn, error],
        });
        let findings = body["result"]["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0]["samples"].as_array().unwrap().len(), 4);
        assert_eq!(findings[1]["samples"].as_array().unwrap().len(), 1);
        assert_eq!(findings[0]["count"], 9);
        assert!(serde_json::to_vec(&body).unwrap().len() < BODY_LIMIT);

        let mut small = finding("small", Severity::Warn, 20);
        small.samples = vec![json!({"index": 1}); 7];
        let body = bounded_body(Findings {
            generated_at_ms: 3,
            window_ms: 1,
            findings: vec![small],
        });
        assert_eq!(
            body["result"]["findings"][0]["samples"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(body["result"]["findings"][0]["count"], 20);
    }
}
