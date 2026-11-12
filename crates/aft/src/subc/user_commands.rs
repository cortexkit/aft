//! Person-facing commands relayed by Occipital, never offered as agent tools.
use super::*;
use cortexkit_role_user_commands as contract;
use serde::de::DeserializeOwned;

fn method(body: &Value) -> Option<&str> {
    body.get("method").and_then(Value::as_str)
}

pub(super) fn is_request(body: &Value) -> bool {
    method(body).is_some_and(|op| contract::MODULE_OPS.contains(&op))
}

pub(super) fn is_occipital(principal: Option<&Principal>) -> bool {
    matches!(principal, Some(Principal::Reserved { module_id }) if module_id == "occipital")
}

fn admit(principal: Option<&Principal>) -> Result<(), ErrorBody> {
    // Protocol 0.35 carries the attested principal, not the daemon's additional
    // first-party attestation. Require that extra fact once the daemon sends it.
    contract::check_admission(principal.unwrap_or(&Principal::Unverified), false, false)
}

fn status_command() -> contract::Command {
    contract::Command::new(
        "status",
        "AFT status",
        "Shows whether AFT's search and code map are ready for this chat's project, its open errors, and its running shells.",
        contract::SubjectKind::Session,
        vec![],
        vec!["read".into()],
        None,
    )
}

fn params<T: DeserializeOwned>(body: &Value) -> Result<T, ErrorBody> {
    serde_json::from_value(body.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|_| contract::ValidationError::new("params").refusal())
}

fn encode(body: &impl serde::Serialize) -> Value {
    serde_json::to_value(body).expect("command bodies contain only finite JSON values")
}

fn select(id: &str) -> Result<contract::Command, ErrorBody> {
    if id != "status" {
        return Err(ErrorBody::new(contract::UNKNOWN_COMMAND, "unknown command"));
    }
    Ok(status_command())
}

fn call(
    principal: Option<&Principal>,
    body: &Value,
    resolve: impl FnOnce(&contract::SessionRef) -> Result<Option<Arc<AppContext>>, ErrorBody>,
) -> Result<Value, ErrorBody> {
    admit(principal)?;
    let result = (|| match method(body) {
        Some("commands.describe") => {
            let _: contract::DescribeRequest = params(body)?;
            Ok(encode(
                &contract::DescribeReply::new(vec![status_command()]).expect("valid declaration"),
            ))
        }
        Some("commands.preview") => {
            let request: contract::ModulePreviewRequest = params(body)?;
            let command = select(&request.id)?;
            // check applies Command::resolve_args, including its unknown-key refusal.
            request.check(&command).map_err(|error| error.refusal())?;
            let contract::Subject::Session { session } = &request.subject else {
                return Err(contract::ValidationError::new("subject.kind").refusal());
            };
            let preview = match resolve(session)? {
                Some(ctx) => {
                    let (snapshot, running) = shells::project_status(&ctx);
                    preview(&snapshot, running)
                }
                None => unavailable("not_bound"),
            };
            Ok(encode(&preview))
        }
        Some("commands.run") => {
            let request: contract::ModuleRunRequest = params(body)?;
            let command = select(&request.id)?;
            request.check(&command).map_err(|error| error.refusal())?;
            unreachable!("status is read-only; contract validation refuses run")
        }
        _ => Err(ErrorBody::new(contract::UNKNOWN_COMMAND, "unknown command")),
    })();
    let outcome = match &result {
        Ok(reply) if reply.get("commands").is_some() => "described",
        Ok(reply) => reply["reason"].as_str().unwrap_or("available"),
        Err(error) => &error.code,
    };
    // Forwarded caller facts are audit data only. JSON formatting escapes line
    // breaks even in malformed claims, preserving one audit line per call.
    log::info!(
        "user-commands/v1 op={:?} command={} principal=reserved:occipital caller={} outcome={:?}",
        method(body),
        body["params"]["id"],
        body["params"]["caller"],
        outcome,
    );
    result
}

fn runner_matches(identity: &RouteIdentity, runner: &str) -> bool {
    // SessionRef.runner is the bare module id, never `reserved:<id>` or the
    // `direct` identity used by a plugin connected without a runner module.
    identity
        .task_identity()
        .principal
        .as_deref()
        .and_then(|id| id.strip_prefix("reserved:"))
        == Some(runner)
}

fn resolve_session(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    executor: &Executor,
    session: &contract::SessionRef,
) -> Result<Option<Arc<AppContext>>, ErrorBody> {
    if !runner_notices::known_runner(&session.runner) {
        return Err(ErrorBody::new(contract::UNKNOWN_SUBJECT, "unknown subject"));
    }
    let mut found = None;
    for identity in routes.values() {
        // Match the harness, session and runner regardless of whether tools
        // execute as classic calls or through codemode.
        if identity.harness != session.harness
            || identity.session != session.session
            || !runner_matches(identity, &session.runner)
        {
            continue;
        }
        if let Some(ctx) = executor.actor_context(&identity.root) {
            if ctx.config().project_root.is_none() {
                continue;
            }
            if found
                .as_ref()
                .is_some_and(|(root, _)| root != &identity.root)
            {
                // An ambiguous binding must not pick an arbitrary project.
                return Ok(None);
            }
            found = Some((identity.root.clone(), ctx));
        }
    }
    Ok(found.map(|(_, ctx)| ctx))
}

fn unavailable(reason: &str) -> contract::Preview {
    let mut preview = contract::Preview::new(reason, false);
    preview.reason = Some(reason.into());
    preview
}

fn state(value: &Value) -> contract::FactValue {
    let code = value["status"]
        .as_str()
        .or_else(|| value["state"].as_str())
        .unwrap_or("unavailable");
    let code = match code {
        "ready" | "available" | "running" => "ready",
        "off" | "disabled" | "not_applicable" => "off",
        "building" | "loading" | "pending" | "busy" | "starting" => "building",
        "paused" => "paused",
        "stale" => "stale",
        _ => "unavailable",
    };
    let mut fact = contract::FactValue::state(code);
    if code == "building" {
        fact.progress = value["progress"]
            .as_f64()
            .filter(|p| (0.0..=1.0).contains(p));
        if fact.progress.is_none() {
            for (done, total) in [
                ("indexed_files", "files"),
                ("entries_done", "entries_total"),
                ("files_embedded", "files"),
            ] {
                if let (Some(done), Some(total)) = (value[done].as_f64(), value[total].as_f64()) {
                    if total > 0.0 {
                        fact.progress = Some((done / total).clamp(0.0, 1.0));
                        break;
                    }
                }
            }
        }
    }
    fact
}

fn number(value: Option<u64>, unit: &str) -> contract::FactValue {
    match value {
        Some(value) => {
            let mut fact = contract::FactValue::number(value as f64);
            fact.unit = Some(unit.into());
            fact
        }
        None => contract::FactValue::state("unavailable"),
    }
}

fn preview(snapshot: &Value, running: Option<usize>) -> contract::Preview {
    if [
        "search_index",
        "callgraph_store",
        "semantic_index",
        "data_store",
    ]
    .iter()
    .any(|key| snapshot[key]["reason"] == "disk_reserve")
    {
        return unavailable("paused_low_disk");
    }
    let text = state(&snapshot["search_index"]);
    let mut graph = state(&snapshot["callgraph_store"]);
    if snapshot["callgraph_store"].is_null() && snapshot["features"]["callgraph_store"] == false {
        graph = contract::FactValue::state("off");
    }
    let meaning = state(&snapshot["semantic_index"]);
    let mut errors = number(snapshot["status_bar_values"]["errors"].as_u64(), "count");
    if errors.value.is_some_and(|value| value > 0.0) {
        errors.tone = Some("danger".into());
    }
    let mut reply = contract::Preview::new("status", true);
    reply.effect = Some(format!(
        "Search {}; code map {}",
        text.state.as_deref().unwrap(),
        graph.state.as_deref().unwrap()
    ));
    reply.facts = Some(vec![
        contract::Fact::new("Text search", text.clone()),
        contract::Fact::new("Code map", graph.clone()),
        contract::Fact::new("Errors", errors),
        contract::Fact::new(
            "Warnings",
            number(snapshot["status_bar_values"]["warnings"].as_u64(), "count"),
        ),
        contract::Fact::new(
            "Shells running",
            number(running.map(|count| count as u64), "count"),
        ),
    ]);
    reply.table = Some(contract::Table::new(
        vec!["State".into(), "Files".into(), "Size".into()],
        [
            ("Text search", "search_index", text),
            ("Code map", "callgraph_store", graph),
            ("Meaning search", "semantic_index", meaning),
        ]
        .into_iter()
        .map(|(label, key, state)| {
            contract::TableRow::new(
                label,
                vec![
                    state,
                    number(snapshot[key]["files"].as_u64(), "count"),
                    // Shared index storage cannot be assigned to one table row.
                    number(snapshot[key]["bytes"].as_u64(), "bytes"),
                ],
            )
        })
        .collect(),
    ));
    // Hash exactly what the person sees, not transient runtime counters.
    reply.version = blake3::hash(&serde_json::to_vec(&reply).expect("finite preview"))
        .to_hex()
        .to_string();
    debug_assert!(reply.check().is_ok());
    reply
}

fn response_frame(request: &Frame, body: &impl serde::Serialize) -> Result<Frame, SubcError> {
    Frame::build_with_version(
        request.header.ver,
        FrameType::Response,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(body).map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)
}

pub(super) async fn dispatch(
    tx: &WriterSender,
    metrics: &Arc<DispatchPathMetrics>,
    frame: &Frame,
    principal: Option<&Principal>,
    body: &Value,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    executor: &Arc<Executor>,
) -> Result<(), SubcError> {
    if let Err(error) = admit(principal) {
        return send_provider_error(tx, metrics, frame, error).await;
    }
    let (tx, metrics, frame, principal, body, routes, executor) = (
        tx.clone(),
        metrics.clone(),
        frame.clone(),
        principal.cloned(),
        body.clone(),
        routes.clone(),
        executor.clone(),
    );
    let response_guard = ResponseTaskGuard::new(&metrics);
    tokio::spawn(async move {
        let _response = response_guard;
        let result = tokio::task::spawn_blocking(move || {
            call(principal.as_ref(), &body, |session| {
                resolve_session(&routes, &executor, session)
            })
        })
        .await
        .unwrap_or_else(|_| Err(ErrorBody::new("internal_error", "command preview failed")));
        let sent = match result {
            Ok(reply) => match response_frame(&frame, &reply) {
                Ok(response) => {
                    send_reliable_writer_frame(&tx, &metrics, response, "user-commands/v1 reply")
                        .await
                }
                Err(error) => Err(error),
            },
            Err(error) => send_provider_error(&tx, &metrics, &frame, error).await,
        };
        if let Err(error) = sent {
            log::warn!("user-commands/v1 reply: {error}");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests;
