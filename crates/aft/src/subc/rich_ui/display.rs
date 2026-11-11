//! Optional provider-owned display publication, isolated from runner replies.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Instant;

use super::{
    outbox::{self, Identity, Outbox},
    records,
};
use cortexkit_role_tool_display::{
    schema::{validate_completion, validate_record},
    DisplayKind,
};
use serde_json::{json, Value};
use subc_protocol::Principal;

struct Writer {
    outbox: Mutex<Outbox>,
    wake: tokio::sync::mpsc::Sender<()>,
    limits: Mutex<cortexkit_role_tool_display::DisplayLimits>,
}
static SENDER: OnceLock<Writer> = OnceLock::new();
static CALLS: LazyLock<Mutex<HashMap<String, Arc<Mutex<Capture>>>>> =
    LazyLock::new(Default::default);
thread_local! { static ACTIVE: RefCell<Option<Arc<Mutex<Capture>>>> = const { RefCell::new(None) }; }

pub(crate) fn key(key: &crate::db::call_ledger::Key) -> String {
    format!("{}:{}{}", key.carrier.len(), key.carrier, key.call_key)
}

pub(crate) struct Capture {
    identity: Identity,
    capture_key: String,
    tool: String,
    root: PathBuf,
    began: Option<Instant>,
    processing_ms: u64,
    held_at: Option<Instant>,
    held_ms: Option<u64>,
    record_sent: bool,
    finished: bool,
    files: Vec<FileChange>,
    file_snapshot: Option<(Value, Option<Value>)>,
    masker: Option<crate::log_redact::SecretValues>,
    bash: Option<(Value, Value)>,
    terminal_bash: Option<(Value, Value)>,
    deferred: bool,
    terminal: bool,
    // A bash task can exit before its launch reply is sent. The cached exit
    // facts are published only once this is true: the task's stored record
    // shows a reply was produced for a deferred call and no completion was
    // attempted yet, so the completion is written exactly once.
    completion_claimed: bool,
}

#[derive(Clone)]
struct FileChange {
    path: PathBuf,
    initial_path: PathBuf,
    old_path: Option<PathBuf>,
    before: Option<String>,
    after: Option<String>,
    change: &'static str,
    existed_before: bool,
    exists_after: bool,
}

#[cfg(test)]
#[derive(Clone)]
struct Publication {
    identity: Identity,
    kind: DisplayKind,
    body: Value,
}
#[cfg(test)]
thread_local! { static TEST_SINK: RefCell<Option<Vec<Publication>>> = const { RefCell::new(None) }; }
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if TEST_SINK.with(|sink| sink.borrow().is_some()) {
        return true;
    }
    SENDER.get().is_some()
}

pub(in crate::subc) fn register(
    ledger_key: &crate::db::call_ledger::Key,
    route: &crate::subc::RouteIdentity,
    call: &subc_protocol::tool_call::ToolCallRequest,
) {
    // The outer key is used only by execution's existing replay ledger. An
    // origin-only, keyless forward is display-free until it has durable
    // display-attempt retention independent of execution replay.
    if call.call_key.is_none() {
        return;
    }
    if !enabled()
        || !matches!(
            call.name.as_str(),
            "bash" | "edit" | "write" | "apply_patch"
        )
    {
        return;
    }
    let crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind {
        principal_id: Some(principal),
        ..
    } = &route.spawn_principal
    else {
        return;
    };
    // Plugin routes are direct/unverified. The harness name is not evidence:
    // an attested runner can legitimately use the same harness as a plugin.
    let Some(module_id) = principal.strip_prefix("reserved:") else {
        return;
    };
    // Basal's inner program calls are not runner tool calls. Only an explicit
    // origin can associate one with a runner's transcript call.
    if (module_id == "basal" || route.run_owner.is_some()) && call.origin.is_none() {
        return;
    }
    let (runner, call_key, pin) = if let Some(origin) = &call.origin {
        (
            origin.carrier.clone(),
            Some(origin.call_key.clone()),
            origin.schema_pin.clone(),
        )
    } else {
        (
            Principal::Reserved {
                module_id: module_id.into(),
            },
            call.call_key.clone(),
            call.schema_pin.clone(),
        )
    };
    let (Some(call_key), Some(schema_pin)) = (call_key, pin) else {
        return;
    };
    if subc_protocol::tool_call::validate_call_key(&call_key).is_err()
        || subc_protocol::tool_call::validate_schema_pin(&schema_pin).is_err()
    {
        return;
    }
    if crate::subc::tool_views::view(&schema_pin).is_none() {
        return;
    }
    let identity = Identity {
        runner,
        call_key,
        schema_pin,
    };
    if let Ok(mut calls) = CALLS.lock() {
        calls.retain(|_, capture| capture.try_lock().map_or(true, |capture| !capture.finished));
        if calls.len() >= 64 {
            return;
        }
        calls.entry(key(ledger_key)).or_insert_with(|| {
            Arc::new(Mutex::new(Capture {
                identity,
                capture_key: key(ledger_key),
                tool: call.name.clone(),
                root: route.project_root.clone(),
                began: None,
                processing_ms: 0,
                held_at: None,
                held_ms: None,
                record_sent: false,
                finished: false,
                files: Vec::new(),
                file_snapshot: None,
                masker: None,
                bash: None,
                terminal_bash: None,
                deferred: false,
                terminal: false,
                completion_claimed: false,
            }))
        });
    }
}

pub(crate) struct Guard(Option<Arc<Mutex<Capture>>>);
impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub(crate) fn enter(key_value: Option<&crate::db::call_ledger::Key>) -> Guard {
    crate::subc::lower_hooks::register();
    let capture = key_value.and_then(|key_value| CALLS.lock().ok()?.get(&key(key_value)).cloned());
    if let Some(capture) = &capture {
        if let Ok(mut capture) = capture.lock() {
            capture.began.get_or_insert_with(Instant::now);
            end_person_wait(&mut capture);
        }
    }
    Guard(ACTIVE.with(|slot| slot.replace(capture)))
}

pub(crate) fn file(
    file: &str,
    old_path: Option<&str>,
    before: Option<&str>,
    after: Option<&str>,
    change: &'static str,
) {
    ACTIVE.with(|slot| {
        let active = slot.borrow();
        let Some(capture) = active.as_ref() else {
            return;
        };
        let Ok(mut capture) = capture.lock() else {
            return;
        };
        let path = PathBuf::from(file);
        let path = if path.is_absolute() {
            path
        } else {
            capture.root.join(path)
        };
        let old_path = old_path.map(PathBuf::from).map(|path| {
            if path.is_absolute() {
                path
            } else {
                capture.root.join(path)
            }
        });
        if let Some(entry) = capture
            .files
            .iter_mut()
            .find(|entry| entry.path == path || old_path.as_ref() == Some(&entry.path))
        {
            entry.after = after.map(str::to_owned);
            entry.exists_after = change != "deleted";
            entry.path = path;
            if change != "modified" {
                entry.change = change;
                if entry.old_path.is_none() {
                    entry.old_path = old_path;
                }
            }
        } else {
            capture.files.push(FileChange {
                initial_path: old_path.as_ref().unwrap_or(&path).clone(),
                path,
                old_path,
                before: before.map(str::to_owned),
                after: after.map(str::to_owned),
                change,
                existed_before: change != "created",
                exists_after: change != "deleted",
            });
        }
    });
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    // Resolve the nearest existing ancestor too: a newly created file can live
    // under a symlink, and a deleted file no longer canonicalizes by itself.
    let mut ancestor = path;
    let mut suffix = Vec::new();
    let resolved = loop {
        match ancestor.canonicalize() {
            Ok(path) => break path,
            Err(_) => {
                suffix.push(ancestor.file_name()?);
                ancestor = ancestor.parent()?;
            }
        }
    };
    let mut resolved = resolved;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    let root = root.canonicalize().ok()?;
    let relative = resolved
        .strip_prefix(root)
        .ok()?
        .to_str()?
        .replace('\\', "/");
    cortexkit_role_tool_display::schema::validate_project_path(&relative).then_some(relative)
}

fn file_bodies(capture: &Capture, success: bool) -> (Value, Option<Value>) {
    let mut record = json!({"outcome": if success {"success"} else if capture.files.is_empty() {"error"} else {"partial"}, "fileCount":0,"modifiedCount":0,"createdCount":0,"deletedCount":0,"movedCount":0});
    let mut files = Vec::new();
    let (mut added, mut removed, mut known) = (0u64, 0u64, true);
    let mut changes = capture.files.clone();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    for mut change in changes {
        if !change.existed_before && !change.exists_after {
            continue;
        }
        change.change = if !change.existed_before {
            "created"
        } else if !change.exists_after {
            change.path = change.initial_path.clone();
            "deleted"
        } else if change.path != change.initial_path {
            "moved"
        } else {
            "modified"
        };
        if change.change == "modified" && change.before == change.after {
            continue;
        }
        let category = format!("{}Count", change.change);
        record["fileCount"] = json!(record["fileCount"].as_u64().unwrap() + 1);
        record[&category] = json!(record[&category].as_u64().unwrap() + 1);
        let path = relative(&capture.root, &change.path);
        let old_path = change
            .old_path
            .as_ref()
            .map(|path| relative(&capture.root, path));
        let mut entry = json!({"change":change.change});
        if let (Some(before), Some(after)) = (change.before.as_deref(), change.after.as_deref()) {
            let counts = crate::edit::compute_diff_counts(before, after);
            added += counts["additions"].as_u64().unwrap_or(0);
            removed += counts["deletions"].as_u64().unwrap_or(0);
        } else {
            known = false;
        }
        if path.is_none() || old_path.as_ref().is_some_and(Option::is_none) {
            record["location"] = json!("outside_project");
            entry["location"] = json!("outside_project");
            entry["diffOmitted"] = json!(true);
        } else {
            entry["path"] = json!(path);
            if change.change == "moved" {
                if let Some(old_path) = old_path {
                    entry["oldPath"] = json!(old_path);
                }
            }
            if let (Some(masker), Some(before), Some(after)) = (
                &capture.masker,
                change.before.as_deref(),
                change.after.as_deref(),
            ) {
                let path = entry["path"].as_str().unwrap();
                let diff = if change.change == "moved" && before == after {
                    format!(
                        "rename from {}\nrename to {path}\n",
                        entry["oldPath"].as_str().unwrap()
                    )
                } else {
                    let old = if change.change == "created" {
                        "/dev/null".into()
                    } else {
                        format!(
                            "a/{}",
                            entry.get("oldPath").and_then(Value::as_str).unwrap_or(path)
                        )
                    };
                    let new = if change.change == "deleted" {
                        "/dev/null".into()
                    } else {
                        format!("b/{path}")
                    };
                    similar::TextDiff::from_lines(before, after)
                        .unified_diff()
                        .header(&old, &new)
                        .to_string()
                };
                entry["diff"] = json!(masker.redact(&diff));
            } else {
                // Diffs are masked with the secret values this agent has
                // resolved (`capture.masker`). Without that list, generic
                // credential patterns could miss a secret pasted into the
                // file, so the diff is left out and only counts are kept.
                entry["diffOmitted"] = json!(true);
            }
        }
        files.push(entry);
    }
    if !success {
        record["outcome"] = json!(if files.is_empty() { "error" } else { "partial" });
    }
    if known && !files.is_empty() {
        record["addedLines"] = json!(added);
        record["removedLines"] = json!(removed);
    }
    if files.len() == 1 && files[0].get("location").is_none() {
        record["path"] = files[0]["path"].clone();
    }
    let detail = (!files.is_empty()).then(|| {
        let mut detail = json!({"schema":"aft/files-detail/1","revision":1,"files":files});
        if let Some(location) = record.get("location") {
            detail["location"] = location.clone();
        }
        detail
    });
    (record, detail)
}

fn emit(capture: &Capture, kind: DisplayKind, body: Value) {
    let Some(view) = crate::subc::tool_views::view(&capture.identity.schema_pin) else {
        return;
    };
    let schema = view.record_schema;
    let Some(map) = body.as_object() else {
        return;
    };
    let valid = match kind {
        DisplayKind::Record => validate_record(map, &schema),
        DisplayKind::Completion => validate_completion(map, &schema),
        _ => cortexkit_role_tool_display::schema::validate_detail(map),
    };
    if valid.is_err() {
        return;
    }
    #[cfg(test)]
    if TEST_SINK.with(|sink| {
        if let Some(sink) = sink.borrow_mut().as_mut() {
            sink.push(Publication {
                identity: capture.identity.clone(),
                kind: kind.clone(),
                body: body.clone(),
            });
            true
        } else {
            false
        }
    }) {
        return;
    }
    let Some(writer) = SENDER.get() else {
        return;
    };
    if !enqueue_snapshot(writer, capture.identity.clone(), kind, body) {
        DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn enqueue_snapshot(
    writer: &Writer,
    identity: Identity,
    kind: DisplayKind,
    mut body: Value,
) -> bool {
    let Ok(limits) = writer.limits.try_lock() else {
        return false;
    };
    if kind == DisplayKind::Detail {
        let Some(cut) = records::cut_detail(body, limits.max_detail_bytes as usize) else {
            return false;
        };
        body = cut;
    }
    // No database, network, lock wait or queue wait is on the reply path.
    if let Ok(mut outbox) = writer.outbox.try_lock() {
        let admitted = outbox.admit(identity, kind, body, &limits, Instant::now());
        let _ = writer.wake.try_send(());
        admitted
    } else {
        false
    }
}

/// Called only for newly executed keyed calls, never for ledger replays.
pub(crate) fn reply(
    ledger_key: &crate::db::call_ledger::Key,
    held: bool,
    error: bool,
    reason: Option<&str>,
) {
    let capture = CALLS
        .lock()
        .ok()
        .and_then(|calls| calls.get(&key(ledger_key)).cloned());
    let Some(capture) = capture else {
        return;
    };
    let Ok(mut capture) = capture.lock() else {
        return;
    };
    if capture.finished {
        return;
    }
    let duration = capture
        .began
        .map_or(0, |began| began.elapsed().as_millis() as u64)
        .saturating_add(capture.processing_ms);
    if held {
        let running = reason == Some("running");
        if running {
            capture.deferred = true;
        }
        if !capture.record_sent {
            let mut body = if running {
                capture
                    .bash
                    .as_ref()
                    .map(|(body, _)| body.clone())
                    .unwrap_or_else(|| json!({}))
            } else {
                json!({})
            };
            body["durationMs"] = json!(duration);
            body["outcome"] = json!("held");
            body["reason"] = json!(if running { "running" } else { "consent" });
            if capture.tool == "bash" {
                body["phase"] = json!(if running {
                    "background_started"
                } else {
                    "held"
                });
                if body.get("execution").is_none() {
                    body["execution"] = json!("local");
                }
                if running {
                    let fields = body.as_object_mut().unwrap();
                    fields.remove("exitCode");
                    fields.remove("endReason");
                    fields.remove("runDurationMs");
                }
            }
            emit(&capture, DisplayKind::Record, body);
            capture.record_sent = true;
            capture.processing_ms = duration;
            capture.began = None;
        }
        if !running && capture.held_at.is_none() {
            capture.processing_ms = duration;
            capture.began = None;
            capture.held_at = Some(Instant::now());
        }
        if capture.deferred && capture.terminal {
            emit_bash_completion(&mut capture);
        }
        return;
    }
    end_person_wait(&mut capture);
    let completion = capture.record_sent;
    let (mut body, detail) = if capture.tool == "bash" {
        capture
            .bash
            .as_ref()
            .or(capture.terminal_bash.as_ref())
            .cloned()
            .map(|(body, detail)| {
                (
                    body,
                    Some(
                        capture
                            .terminal_bash
                            .as_ref()
                            .map(|(_, terminal)| terminal.clone())
                            .unwrap_or(detail),
                    ),
                )
            })
            .unwrap_or_else(|| {
                (
                    json!({"outcome":"error","execution":"local","phase":"foreground_finished"}),
                    None,
                )
            })
    } else {
        capture.file_snapshot.clone().unwrap_or_else(|| {
            if capture.files.is_empty() {
                file_bodies(&capture, !error)
            } else {
                (
                    json!({"outcome": if error { "error" } else { "success" }}),
                    None,
                )
            }
        })
    };
    if capture.tool == "bash" && capture.deferred {
        if completion {
            if capture.terminal {
                emit_bash_completion(&mut capture);
            }
            return;
        }
        body["phase"] = json!("background_started");
        body["outcome"] = json!("success");
        if let Some(map) = body.as_object_mut() {
            map.remove("exitCode");
            map.remove("endReason");
            map.remove("reason");
            map.remove("runDurationMs");
        }
    } else if capture.tool == "bash" {
        body.as_object_mut().unwrap().remove("taskId");
        body.as_object_mut().unwrap().remove("runDurationMs");
    }
    body["durationMs"] = json!(duration);
    if let Some(held_ms) = capture.held_ms {
        body["heldMs"] = json!(held_ms);
    }
    if error && body.get("reason").is_none() {
        body["reason"] = json!(if matches!(reason, Some("cancelled")) {
            "cancelled"
        } else if matches!(
            reason,
            Some("consent" | "bash_denied_untrusted" | "bash_denied")
        ) {
            "consent"
        } else if let Some(code) =
            reason.filter(|code| cortexkit_role_tool_display::schema::validate_code(code))
        {
            code
        } else {
            "failed"
        });
    }
    emit(
        &capture,
        if completion {
            DisplayKind::Completion
        } else {
            DisplayKind::Record
        },
        body,
    );
    capture.record_sent = true;
    if capture.deferred && capture.terminal {
        emit_bash_completion(&mut capture);
    }
    if !capture.deferred {
        if let Some(mut detail) = detail {
            if capture.tool == "bash" {
                detail.as_object_mut().unwrap().remove("taskId");
            }
            emit(&capture, DisplayKind::Detail, detail);
        }
        capture.finished = true;
        drop(capture);
        if let Ok(mut calls) = CALLS.lock() {
            calls.remove(&key(ledger_key));
        }
    }
}

pub(crate) fn start(connection_file: PathBuf) {
    if std::env::var("AFT_TOOL_DISPLAY").as_deref() != Ok("1") {
        return;
    }
    if std::env::var("SUBC_MODULE_ID").as_deref() != Ok("aft")
        || crate::launch_nonce::consumer_identity().is_none()
    {
        return;
    }
    let (wake, receiver) = tokio::sync::mpsc::channel(1);
    if SENDER
        .set(Writer {
            outbox: Mutex::new(Outbox::new()),
            wake,
            limits: Mutex::new(outbox::ceilings()),
        })
        .is_err()
    {
        return;
    }
    tokio::spawn(send_loop(connection_file, receiver));
}

async fn send_loop(connection_file: PathBuf, mut receiver: tokio::sync::mpsc::Receiver<()>) {
    use subc_client_rs::{CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
    use subc_protocol::BindIdentity;
    let writer = SENDER.get().expect("display writer started");
    let mut consumer = None;
    loop {
        tokio::select! {
            _ = receiver.recv() => {},
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
        }
        let entry = writer
            .outbox
            .lock()
            .ok()
            .and_then(|mut outbox| outbox.pop_due(Instant::now()));
        let Some(entry) = entry else {
            continue;
        };
        if consumer.is_none() {
            let options = ConsumerOptions {
                call_timeout: std::time::Duration::from_secs(5),
                ..Default::default()
            };
            consumer = SubcConsumer::connect(&connection_file, options).await.ok();
        }
        let mut retry = true;
        if let Some(client) = consumer.as_ref() {
            // Resolution includes warming modules and refuses ambiguity. Resolve
            // again for every attempt, even when a prior route was usable.
            if let Some(module_id) = client
                .catalog_list()
                .await
                .ok()
                .and_then(|catalog| declared_display(&catalog.modules))
            {
                if let Ok(route) = client
                    .open_route(
                        subc_protocol::RouteTarget::ToolProvider { module_id },
                        BindIdentity::new("aft", "tool-display"),
                        CallOptions {
                            consumer_identity: crate::launch_nonce::consumer_identity(),
                            ..CallOptions::default()
                        },
                    )
                    .await
                {
                    let description = client
                        .request(
                            &route,
                            br#"{"name":"role.describe","arguments":{}}"#.to_vec(),
                            CallOptions::default(),
                        )
                        .await;
                    let limits = description
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .and_then(|value| value.get("tool_display").cloned())
                        .and_then(|value| {
                            serde_json::from_value::<cortexkit_role_tool_display::DisplayLimits>(
                                value,
                            )
                            .ok()
                        })
                        .filter(|limits| limits.validate().is_ok());
                    if let Some(limits) = limits {
                        if let Ok(mut cached) = writer.limits.lock() {
                            *cached = limits.clone();
                        }
                        // An admitted body cannot be rebuilt for a smaller store.
                        // Lose that optional kind rather than change its first bytes.
                        if entry.body.len() as u64 <= limits.max_bytes(&entry.kind).unwrap_or(0) {
                            match client
                                .request(&route, entry.frame.clone(), CallOptions::default())
                                .await
                            {
                                Ok(bytes) => {
                                    retry = retry_reply(&bytes);
                                }
                                Err(subc_client_rs::CallError::Module(error)) => {
                                    retry = error.code == "module_warming";
                                }
                                Err(_) => {}
                            }
                        } else {
                            retry = false;
                        }
                    }
                    let _ = client
                        .close_handle(&route, CloseRouteOptions::default())
                        .await;
                }
            }
        }
        if retry {
            if let Ok(mut outbox) = writer.outbox.lock() {
                outbox.retry(entry, Instant::now());
            }
        }
    }
}

fn retry_reply(bytes: &[u8]) -> bool {
    let Ok(reply) = serde_json::from_slice::<Value>(bytes) else {
        return true;
    };
    if matches!(reply["result"].as_str(), Some("stored" | "already_stored")) {
        return false;
    }
    !matches!(
        reply["code"].as_str(),
        Some(
            "display_write_conflict"
                | "invalid_params"
                | "display_writer_unverified"
                | "display_too_large"
        )
    )
}

pub(crate) use crate::bash_background::frames::TaskDisplay;

pub(crate) fn current_task() -> Option<TaskDisplay> {
    ACTIVE.with(|slot| {
        let active = slot.borrow();
        let capture = active.as_ref()?.lock().ok()?;
        Some(TaskDisplay {
            identity: capture.identity.clone(),
            capture_key: capture.capture_key.clone(),
            reply_produced: false,
            deferred: false,
            held_ms: capture.held_ms,
            attempted: false,
        })
    })
}

pub(crate) use crate::bash_background::frames::bash_bodies;

pub(crate) fn bash_reply(display: &mut TaskDisplay, body: (Value, Value), deferred: bool) {
    let capture = CALLS
        .lock()
        .ok()
        .and_then(|calls| calls.get(&display.capture_key).cloned());
    let Some(capture) = capture else {
        return;
    };
    let Ok(mut capture) = capture.lock() else {
        return;
    };
    capture.began.get_or_insert_with(Instant::now);
    end_person_wait(&mut capture);
    if capture.bash.is_none() {
        capture.bash = Some(body);
    }
    capture.deferred = deferred;
    display.reply_produced = true;
    display.deferred = deferred;
    display.held_ms = capture.held_ms;
    if let Some(began) = capture.began.take() {
        capture.processing_ms = capture
            .processing_ms
            .saturating_add(began.elapsed().as_millis() as u64);
    }
}

fn emit_bash_completion(capture: &mut Capture) {
    if capture.finished || !capture.completion_claimed {
        return;
    }
    let Some((mut record, mut detail)) = capture.terminal_bash.clone() else {
        return;
    };
    record["taskId"] = detail.get("taskId").cloned().unwrap_or(Value::Null);
    record["phase"] = json!("background_finished");
    detail["phase"] = json!("background_finished");
    if let Some(held_ms) = capture.held_ms {
        record["heldMs"] = json!(held_ms);
    }
    emit(capture, DisplayKind::Detail, detail);
    emit(capture, DisplayKind::Completion, record);
    capture.finished = true;
}

pub(crate) fn bash_terminal(
    display: &TaskDisplay,
    mut bodies: (Value, Value),
    task_id: &str,
    duration_ms: Option<u64>,
) {
    // Losing track of deferred work is not proof that it ended. The initial
    // record stays, but neither a completion nor a terminal detail is written.
    if bodies.0["reason"] == "fate_unknown" {
        if let Some(capture) = CALLS
            .lock()
            .ok()
            .and_then(|calls| calls.get(&display.capture_key).cloned())
        {
            if let Ok(mut capture) = capture.lock() {
                capture.finished = true;
            }
        }
        return;
    }
    bodies.0["taskId"] = json!(task_id);
    bodies.1["taskId"] = json!(task_id);
    if let Some(duration) = duration_ms {
        bodies.0["runDurationMs"] = json!(duration);
    }
    let capture = CALLS
        .lock()
        .ok()
        .and_then(|calls| calls.get(&display.capture_key).cloned());
    if let Some(capture) = capture {
        if let Ok(mut capture) = capture.lock() {
            if !capture.terminal {
                capture.terminal_bash = Some(bodies);
                capture.terminal = true;
            }
            if display.reply_produced && display.deferred && !display.attempted {
                capture.completion_claimed = true;
            }
            if capture.record_sent && capture.deferred {
                emit_bash_completion(&mut capture);
            }
        }
    } else if display.reply_produced && display.deferred && !display.attempted {
        let mut capture = Capture {
            identity: display.identity.clone(),
            capture_key: display.capture_key.clone(),
            tool: "bash".into(),
            root: PathBuf::new(),
            began: None,
            processing_ms: 0,
            held_at: None,
            held_ms: display.held_ms,
            record_sent: true,
            finished: false,
            files: Vec::new(),
            file_snapshot: None,
            masker: None,
            bash: None,
            terminal_bash: Some(bodies),
            deferred: true,
            terminal: true,
            completion_claimed: true,
        };
        emit_bash_completion(&mut capture);
    }
}

fn declared_display(modules: &[subc_client_rs::CatalogEntry]) -> Option<String> {
    let mut claimants: Vec<_> = modules
        .iter()
        .filter(|module| {
            module.capabilities.as_ref().is_some_and(|capabilities| {
                capabilities
                    .provides
                    .iter()
                    .any(|capability| capability == "tool-display/v1")
            })
        })
        .map(|module| module.module_id.clone())
        .collect();
    claimants.sort();
    claimants.dedup();
    (claimants.len() == 1).then(|| claimants.remove(0))
}

#[cfg(test)]
#[path = "display/tests.rs"]
mod tests;

fn end_person_wait(capture: &mut Capture) {
    if let Some(held_at) = capture.held_at.take() {
        capture.held_ms = Some(
            capture
                .held_ms
                .unwrap_or(0)
                .saturating_add(held_at.elapsed().as_millis() as u64),
        );
    }
}

pub(crate) fn person_answered(ledger_key: &crate::db::call_ledger::Key) {
    let capture = CALLS
        .lock()
        .ok()
        .and_then(|calls| calls.get(&key(ledger_key)).cloned());
    if let Some(capture) = capture {
        if let Ok(mut capture) = capture.lock() {
            end_person_wait(&mut capture);
        }
    }
}

pub(crate) fn handler_ready(ledger_key: Option<&crate::db::call_ledger::Key>, success: bool) {
    let capture =
        ledger_key.and_then(|ledger_key| CALLS.lock().ok()?.get(&key(ledger_key)).cloned());
    if let Some(capture) = capture {
        if let Ok(mut capture) = capture.lock() {
            if let Some(began) = capture.began.take() {
                capture.processing_ms = capture
                    .processing_ms
                    .saturating_add(began.elapsed().as_millis() as u64);
            }
            if capture.tool != "bash" {
                capture.file_snapshot = Some(file_bodies(&capture, success));
            }
        }
    }
}
