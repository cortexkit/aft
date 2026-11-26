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
use crate::bash_background::frames::BashBodies;
use cortexkit_role_tool_display::{
    schema::{validate_completion, validate_record},
    DisplayKind,
};
use serde_json::{json, Value};
use subc_protocol::Principal;

pub(crate) mod bash_tail;
pub(super) mod counters;
mod diagnostics;
mod edit_hunks;
mod notice_pull;
mod store;
use counters::{count, Counter::*};
pub(crate) use notice_pull::publish_awaited_notice;
pub(in crate::subc) use notice_pull::{stage_awaited_notice, stage_notice};

struct Writer {
    outbox: Mutex<Outbox>,
    wake: tokio::sync::mpsc::Sender<()>,
    limits: Mutex<cortexkit_role_tool_display::DisplayLimits>,
    store: store::Store,
}
static SENDER: OnceLock<Writer> = OnceLock::new();

#[cfg(test)]
static EMPTY_OUTBOX_SCANNED: tokio::sync::Notify = tokio::sync::Notify::const_new();

#[cfg(test)]
pub(crate) async fn wait_for_empty_outbox_scan_for_test() {
    EMPTY_OUTBOX_SCANNED.notified().await;
}

type Captures = Mutex<HashMap<String, Arc<Mutex<Capture>>>>;
static CALLS: LazyLock<Captures> = LazyLock::new(Default::default);
thread_local! { static ACTIVE: RefCell<Option<Arc<Mutex<Capture>>>> = const { RefCell::new(None) }; }

pub(crate) fn key(key: &crate::db::call_ledger::Key) -> String {
    format!("{}:{}{}", key.carrier.len(), key.carrier, key.call_key)
}

pub(crate) struct Capture {
    identity: Identity,
    capture_key: String,
    tool: String,
    root: PathBuf,
    write_input_path: Option<PathBuf>,
    began: Option<Instant>,
    processing_ms: u64,
    held_at: Option<Instant>,
    held_ms: Option<u64>,
    record_sent: bool,
    launch_admitted: bool,
    deferred_persisted: bool,
    finished: bool,
    files: Vec<FileChange>,
    file_snapshot: Option<(Value, Option<Value>)>,
    bash: Option<BashBodies>,
    terminal_bash: Option<BashBodies>,
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
    input_path: Option<PathBuf>,
    initial_input_path: Option<PathBuf>,
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

/// The most bytes a detail body may hold once published; text past it is cut.
/// Producers can stop reading their output there.
pub(crate) fn detail_byte_limit() -> u64 {
    SENDER
        .get()
        .and_then(|writer| {
            writer
                .limits
                .try_lock()
                .ok()
                .map(|limits| limits.max_detail_bytes)
        })
        .unwrap_or_else(|| outbox::ceilings().max_detail_bytes)
}

// Module-mode publication is on by default, with an explicit opt-out.
fn display_switch() -> Result<(), &'static str> {
    match std::env::var("AFT_TOOL_DISPLAY").as_deref() {
        Ok("0") => Err("opt_out"),
        _ => Ok(()),
    }
}

pub(crate) fn health() -> Value {
    let enabled = SENDER.get().is_some();
    let reason = if enabled {
        None
    } else if let Err(reason) = display_switch() {
        Some(reason)
    } else if std::env::var("SUBC_MODULE_ID").as_deref() != Ok("aft") {
        Some("not_module")
    } else if !crate::subc::registration::identity_ready() {
        Some("consumer_identity_missing")
    } else {
        Some("not_started")
    };
    json!({"enabled": enabled, "reason": reason, "last_error": diagnostics::health(), "zero_counters_omitted": true,
        "counters": counters::snapshot()})
}

pub(in crate::subc) fn register(
    ledger_key: &crate::db::call_ledger::Key,
    route: &crate::subc::RouteIdentity,
    call: &subc_protocol::tool_call::ToolCallRequest,
) {
    register_in(&CALLS, ledger_key, route, call);
}

fn register_in(
    registry: &Captures,
    ledger_key: &crate::db::call_ledger::Key,
    route: &crate::subc::RouteIdentity,
    call: &subc_protocol::tool_call::ToolCallRequest,
) {
    // The outer key is used only by execution's existing replay ledger. An
    // origin-only, keyless forward is display-free until it has durable
    // display-attempt retention independent of execution replay.
    if call.call_key.is_none() {
        count(RegisterNoCallKey);
        return;
    }
    if !enabled() {
        count(RegisterDisabled);
        if display_switch().is_err() {
            count(RegisterOptOut);
        }
        return;
    }
    if !matches!(
        call.name.as_str(),
        "bash" | "edit" | "write" | "apply_patch"
    ) {
        count(RegisterUnsupportedTool);
        return;
    }
    let crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind {
        principal_id: Some(principal),
        ..
    } = &route.spawn_principal
    else {
        count(RegisterUnattested);
        return;
    };
    // Plugin routes are direct/unverified. The harness name is not evidence:
    // an attested runner can legitimately use the same harness as a plugin.
    let Some(module_id) = principal.strip_prefix("reserved:") else {
        count(RegisterNotReserved);
        return;
    };
    // Basal's inner program calls are not runner tool calls. Only an explicit
    // origin can associate one with a runner's transcript call.
    if (module_id == "basal" || route.run_owner.is_some()) && call.origin.is_none() {
        count(RegisterInnerCall);
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
        count(RegisterMissingIdentity);
        return;
    };
    if subc_protocol::tool_call::validate_call_key(&call_key).is_err() {
        count(RegisterInvalidCallKey);
        return;
    }
    if subc_protocol::tool_call::validate_schema_pin(&schema_pin).is_err() {
        count(RegisterInvalidPin);
        return;
    }
    if crate::subc::tool_views::view(&schema_pin).is_none() {
        count(RegisterUnknownView);
        return;
    }
    let identity = Identity {
        runner,
        call_key,
        schema_pin,
    };
    if let Ok(mut calls) = registry.lock() {
        calls.retain(|_, capture| capture.try_lock().map_or(true, |capture| !capture.finished));
        if calls.len() >= 64 {
            count(RegisterCallCap);
            return;
        }
        if calls.contains_key(&key(ledger_key)) {
            count(RegisterExisting);
            return;
        }
        calls.entry(key(ledger_key)).or_insert_with(|| {
            count(Registered);
            Arc::new(Mutex::new(Capture {
                identity,
                capture_key: key(ledger_key),
                tool: call.name.clone(),
                root: route.project_root.clone(),
                write_input_path: if call.name == "write" {
                    absolute_input_path(call.arguments.get("path").and_then(Value::as_str))
                } else {
                    None
                },
                began: None,
                processing_ms: 0,
                held_at: None,
                held_ms: None,
                record_sent: false,
                launch_admitted: false,
                deferred_persisted: false,
                finished: false,
                files: Vec::new(),
                file_snapshot: None,
                bash: None,
                terminal_bash: None,
                deferred: false,
                terminal: false,
                completion_claimed: false,
            }))
        });
    } else {
        count(RegisterLock);
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
    input_paths: Option<(&str, Option<&str>)>,
) {
    ACTIVE.with(|slot| {
        let active = slot.borrow();
        let Some(capture) = active.as_ref() else {
            return;
        };
        let Ok(mut capture) = capture.lock() else {
            return;
        };
        let (input_path, old_input_path) = match input_paths {
            Some((path, old)) => (absolute_input_path(Some(path)), absolute_input_path(old)),
            None => (capture.write_input_path.clone(), None),
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
            entry.input_path = input_path;
            if change != "modified" {
                entry.change = change;
                if entry.old_path.is_none() {
                    entry.old_path = old_path;
                }
            }
        } else {
            capture.files.push(FileChange {
                initial_path: old_path.as_ref().unwrap_or(&path).clone(),
                initial_input_path: if old_path.is_some() {
                    old_input_path
                } else {
                    input_path.clone()
                },
                input_path,
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

fn absolute_input_path(path: Option<&str>) -> Option<PathBuf> {
    path.map(PathBuf::from).filter(|path| path.is_absolute())
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
    let mut hunks = edit_hunks::Hunks::default();
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
        let outside = path.is_none() || old_path.as_ref().is_some_and(Option::is_none);
        let old_path = old_path.flatten().filter(|_| change.change == "moved");
        if outside {
            record["location"] = json!("outside_project");
            entry["location"] = json!("outside_project");
        } else {
            entry["path"] = json!(path);
            if let Some(old_path) = &old_path {
                entry["oldPath"] = json!(old_path);
            }
        }
        if let (Some(before), Some(after)) = (change.before.as_deref(), change.after.as_deref()) {
            let counts = crate::edit::compute_diff_counts(before, after);
            added += counts["additions"].as_u64().unwrap_or(0);
            removed += counts["deletions"].as_u64().unwrap_or(0);
            if capture.tool == "edit" {
                edit_hunks::diff(&mut hunks, before, after);
            } else {
                let relative_paths = path
                    .as_deref()
                    .filter(|_| !outside)
                    .map(|path| (old_path.as_deref().unwrap_or(path), path));
                entry["diff"] = json!(unified_diff(&change, before, after, relative_paths));
            }
        } else {
            known = false;
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
        hunks.attach(&mut detail);
        detail
    });
    let detail = if capture.tool == "edit" {
        detail.and_then(|detail| records::cut_detail(detail, crate::edit::DIFF_RESPONSE_BUDGET))
    } else {
        detail
    };
    (record, detail)
}

fn unified_diff(
    change: &FileChange,
    before: &str,
    after: &str,
    relative_paths: Option<(&str, &str)>,
) -> String {
    // Use the call's absolute spellings for display, not the resolved paths
    // used to track mutations. A relative input has no absolute spelling.
    let old_abs = change
        .initial_input_path
        .as_ref()
        .unwrap_or(&change.initial_path);
    let new_abs = change.input_path.as_ref().unwrap_or(&change.path);
    if change.change == "moved" && before == after {
        return match relative_paths {
            Some((old, new)) => format!("rename from {old}\nrename to {new}\n"),
            None => format!(
                "rename from {}\nrename to {}\n",
                old_abs.display(),
                new_abs.display()
            ),
        };
    }
    let old = match (change.change, relative_paths) {
        ("created", _) => "/dev/null".into(),
        (_, Some((old, _))) => format!("a/{old}").into(),
        (_, None) => old_abs.to_string_lossy(),
    };
    let new = match (change.change, relative_paths) {
        ("deleted", _) => "/dev/null".into(),
        (_, Some((_, new))) => format!("b/{new}").into(),
        (_, None) => new_abs.to_string_lossy(),
    };
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .header(&old, &new)
        .to_string()
}

pub(crate) use edit_hunks::capture_no_match as edit_no_match;

fn emit(capture: &Capture, kind: DisplayKind, body: Value) -> bool {
    let Some(view) = crate::subc::tool_views::view(&capture.identity.schema_pin) else {
        count(EmitUnknownView);
        return false;
    };
    let schema = &view.record_schema;
    let Some(map) = body.as_object() else {
        count(EmitNotObject);
        return false;
    };
    let valid = match kind {
        DisplayKind::Record => validate_record(map, schema),
        DisplayKind::Completion => validate_completion(map, schema),
        _ => cortexkit_role_tool_display::schema::validate_detail(map),
    };
    if valid.is_err() {
        count(match kind {
            DisplayKind::Record => EmitInvalidRecord,
            DisplayKind::Completion => EmitInvalidCompletion,
            _ => EmitInvalidDetail,
        });
        return false;
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
        count(Emitted);
        return true;
    }
    let Some(writer) = SENDER.get() else {
        count(EmitNoSender);
        return false;
    };
    let admitted = enqueue_snapshot(writer, capture.identity.clone(), kind, body);
    if admitted {
        count(Emitted);
    }
    admitted
}

fn enqueue_snapshot(
    writer: &Writer,
    identity: Identity,
    kind: DisplayKind,
    mut body: Value,
) -> bool {
    let Ok(limits) = writer.limits.try_lock() else {
        count(EnqueueLimitsLock);
        return false;
    };
    if kind == DisplayKind::Detail {
        let Some(cut) = records::cut_detail(body, limits.max_detail_bytes as usize) else {
            count(EnqueueDetailTooLarge);
            return false;
        };
        body = cut;
    }
    // No database, network, lock wait or queue wait is on the reply path.
    if let Ok(mut outbox) = writer.outbox.try_lock() {
        let admitted = outbox.admit(identity, kind, body, &limits, Instant::now());
        if admitted {
            crate::late_result_bus::display_committed();
        }
        match writer.wake.try_send(()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => count(WakeCoalesced),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => count(WakeClosed),
        }
        admitted
    } else {
        count(EnqueueOutboxLock);
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
    reply_in(&CALLS, ledger_key, held, error, reason);
}

fn reply_in(
    registry: &Captures,
    ledger_key: &crate::db::call_ledger::Key,
    held: bool,
    error: bool,
    reason: Option<&str>,
) {
    let capture_key = key(ledger_key);
    let capture = registry
        .lock()
        .ok()
        .and_then(|calls| calls.get(&capture_key).cloned());
    let Some(capture) = capture else {
        return;
    };
    let may_retire = if let Ok(mut capture) = capture.lock() {
        publish_reply(&mut capture, held, error, reason);
        capture.can_retire()
    } else {
        false
    };
    if may_retire {
        retire_capture(registry, &capture_key, false);
    }
}

fn publish_reply(capture: &mut Capture, held: bool, error: bool, reason: Option<&str>) {
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
                    .map(|bodies| bodies.facts.clone())
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
            let admitted = emit(capture, DisplayKind::Record, body);
            capture.launch_admitted = running && admitted;
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
            emit_bash_completion(capture);
        }
        return;
    }
    end_person_wait(capture);
    let completion = capture.record_sent;
    let mut tail = None;
    let (mut body, detail) = if capture.tool == "bash" {
        capture
            .bash
            .as_ref()
            .or(capture.terminal_bash.as_ref())
            .cloned()
            .map(|bodies| {
                let (detail, output_tail) = capture
                    .terminal_bash
                    .as_ref()
                    .map(|terminal| (terminal.detail.clone(), terminal.tail.clone()))
                    .unwrap_or((bodies.detail, bodies.tail));
                tail = output_tail;
                (bodies.facts, Some(detail))
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
                file_bodies(capture, !error)
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
                emit_bash_completion(capture);
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
    let mut foreground_completion = None;
    if !capture.deferred {
        if let Some(tail) = tail {
            if completion {
                body["tail"] = tail;
            } else {
                let mut body = body.clone();
                body["tail"] = tail;
                foreground_completion = Some(body);
            }
        }
    }
    let admitted = emit(
        capture,
        if completion {
            DisplayKind::Completion
        } else {
            DisplayKind::Record
        },
        body,
    );
    capture.launch_admitted = capture.deferred && !completion && admitted;
    capture.record_sent = true;
    if capture.deferred && capture.terminal {
        emit_bash_completion(capture);
    }
    if !capture.deferred {
        if let Some(mut detail) = detail {
            if capture.tool == "bash" {
                detail.as_object_mut().unwrap().remove("taskId");
            }
            emit(capture, DisplayKind::Detail, detail);
        }
        if let Some(body) = foreground_completion {
            emit(capture, DisplayKind::Completion, body);
        }
        capture.finished = true;
    }
}

pub(crate) fn start(connection_file: &Path, app: &Arc<crate::context::App>, storage_dir: &Path) {
    if display_switch().is_err() {
        count(StartDisabled);
        return;
    }
    if std::env::var("SUBC_MODULE_ID").as_deref() != Ok("aft") {
        count(StartNotModule);
        return;
    }
    if !crate::subc::registration::module_consumer_allowed() {
        count(StartNoIdentity);
        return;
    }
    let (wake, receiver) = tokio::sync::mpsc::channel(1);
    if SENDER
        .set(Writer {
            outbox: Mutex::new(Outbox::new()),
            wake,
            limits: Mutex::new(outbox::ceilings()),
            store: store::Store::new(app.clone(), storage_dir.join("aft.db")),
        })
        .is_err()
    {
        count(StartExisting);
        return;
    }
    tokio::spawn(send_loop(connection_file.to_path_buf(), receiver));
    tokio::spawn(notice_pull::flush_pending());
    count(Started);
}

async fn send_loop(connection_file: PathBuf, mut receiver: tokio::sync::mpsc::Receiver<()>) {
    use subc_client_rs::{CallOptions, CloseRouteOptions, ConsumerOptions};
    use subc_protocol::BindIdentity;
    let writer = SENDER.get().expect("display writer started");
    let mut consumer = None;
    while let Some(entry) = next_display_entry(&writer.outbox, &mut receiver).await {
        if consumer.is_none() {
            let options = ConsumerOptions {
                call_timeout: std::time::Duration::from_secs(5),
                ..Default::default()
            };
            match crate::subc::registration::connect_consumer("display", &connection_file, options)
                .await
            {
                Ok(client) => consumer = Some(client),
                Err(_) => count(WriterConnect),
            }
        }
        let mut retry = true;
        if let Some(client) = consumer.as_ref() {
            // Resolution includes warming modules and refuses ambiguity. Resolve
            // again for every attempt, even when a prior route was usable.
            let module_id = match client.catalog_list().await {
                Ok(catalog) => {
                    let display = declared_display(&catalog.modules);
                    if display.is_none() {
                        let present = catalog.modules.iter().any(|module| {
                            module.capabilities.as_ref().is_some_and(|caps| {
                                caps.provides.iter().any(|capability| {
                                    capability
                                        == cortexkit_role_tool_display::TOOL_DISPLAY_CAPABILITY
                                })
                            })
                        });
                        count(if present {
                            WriterAmbiguousDisplay
                        } else {
                            WriterNoDisplay
                        });
                    }
                    display
                }
                Err(_) => {
                    count(WriterCatalog);
                    None
                }
            };
            if let Some(module_id) = module_id {
                match client
                    .open_route(
                        subc_protocol::RouteTarget::ToolProvider { module_id },
                        BindIdentity::new("aft", "tool-display"),
                        CallOptions::default(),
                    )
                    .await
                {
                    Ok(route) => {
                        let description = client
                            .request(
                                &route,
                                br#"{"name":"role.describe","arguments":{}}"#.to_vec(),
                                CallOptions::default(),
                            )
                            .await;
                        let limits = match description {
                            Ok(bytes) => display_limits(&bytes),
                            Err(_) => {
                                count(WriterDescribe);
                                None
                            }
                        };
                        if let Some(limits) = limits {
                            if let Ok(mut cached) = writer.limits.lock() {
                                *cached = limits.clone();
                            } else {
                                count(WriterLimitsLock);
                            }
                            // An admitted body cannot be rebuilt for a smaller store.
                            // Lose that optional kind rather than change its first bytes.
                            if entry.body.len() as u64 <= limits.max_bytes(&entry.kind).unwrap_or(0)
                            {
                                match client
                                    .request(&route, entry.frame.clone(), CallOptions::default())
                                    .await
                                {
                                    Ok(bytes) => {
                                        retry = retry_reply(&bytes);
                                    }
                                    Err(subc_client_rs::CallError::Module(error)) => {
                                        retry = error.code == "module_warming";
                                        count(if retry {
                                            WriterModuleWarming
                                        } else {
                                            WriterModuleRefused
                                        });
                                    }
                                    Err(_) => count(WriterRequest),
                                }
                            } else {
                                count(WriterTooLarge);
                                retry = false;
                            }
                        }
                        if client
                            .close_handle(&route, CloseRouteOptions::default())
                            .await
                            .is_err()
                        {
                            count(WriterCloseRoute);
                        }
                    }
                    Err(error) => {
                        count(WriterOpenRoute);
                        diagnostics::route_open(&error);
                    }
                }
            }
        }
        if retry {
            if let Ok(mut outbox) = writer.outbox.lock() {
                outbox.retry(entry, Instant::now());
            } else {
                count(WriterOutboxLock);
            }
        } else if let Ok(mut outbox) = writer.outbox.lock() {
            outbox.reads.push_finished(&entry.record_id, Instant::now());
        } else {
            count(WriterOutboxLock);
        }
    }
}

async fn next_display_entry(
    outbox: &Mutex<Outbox>,
    receiver: &mut tokio::sync::mpsc::Receiver<()>,
) -> Option<outbox::Entry> {
    loop {
        let next = match outbox.lock() {
            Ok(mut outbox) => {
                if let Some(entry) = outbox.pop_due(tokio::time::Instant::now().into_std()) {
                    return Some(entry);
                }
                outbox.next_wake_at()
            }
            Err(_) => {
                count(WriterOutboxLock);
                return None;
            }
        };
        // The guard from the empty scan is no longer held. The isolated
        // startup fixture can admit its own record before the sender wakes.
        #[cfg(test)]
        if next.is_none() {
            EMPTY_OUTBOX_SCANNED.notify_one();
        }
        // Adding entries to the outbox signals the channel even during the
        // scan above. One wake is enough to drain all currently due entries.
        let wake = match next {
            Some(next) => tokio::select! {
                wake = receiver.recv() => wake,
                _ = tokio::time::sleep_until(next.into()) => continue,
            },
            None => receiver.recv().await,
        };
        if wake.is_none() {
            count(WriterStopped);
            return None;
        }
    }
}

fn display_limits(bytes: &[u8]) -> Option<cortexkit_role_tool_display::DisplayLimits> {
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => {
            count(WriterDescriptionJson);
            return None;
        }
    };
    let Some(value) = value.get("tool_display") else {
        count(WriterDescriptionMissing);
        return None;
    };
    match serde_json::from_value::<cortexkit_role_tool_display::DisplayLimits>(value.clone()) {
        Ok(limits) if limits.validate().is_ok() => Some(limits),
        _ => {
            count(WriterDescriptionLimits);
            None
        }
    }
}

fn retry_reply(bytes: &[u8]) -> bool {
    let Ok(reply) = serde_json::from_slice::<Value>(bytes) else {
        count(WriterReplyJson);
        return true;
    };
    if let Some(result @ ("stored" | "already_stored")) = reply["result"].as_str() {
        count(if result == "stored" {
            Stored
        } else {
            AlreadyStored
        });
        return false;
    }
    let refused = matches!(
        reply["code"].as_str(),
        Some(
            "display_write_conflict"
                | "invalid_params"
                | "display_writer_unverified"
                | "display_too_large"
        )
    );
    count(if refused {
        WriterReplyRefused
    } else {
        WriterReplyUnknown
    });
    !refused
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

#[cfg(test)]
pub(crate) use aft_engine::test_bridge::bash_remote::bash_bodies;

pub(in crate::subc) fn bash_tail(
    metadata: &crate::bash_background::persistence::PersistedTask,
    stdout: &str,
    stderr: &str,
    remask: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    if metadata.is_terminal()
        && metadata.display.as_ref().is_some_and(|display| {
            crate::subc::tool_views::has_bash_tail(&display.identity.schema_pin)
        })
    {
        bash_tail::from_streams(metadata, stdout, stderr, remask)
    } else {
        None
    }
}

pub(crate) fn bash_reply(display: &mut TaskDisplay, body: BashBodies, deferred: bool) {
    bash_reply_in(&CALLS, display, body, deferred);
}

fn bash_reply_in(registry: &Captures, display: &mut TaskDisplay, body: BashBodies, deferred: bool) {
    let capture = registry
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

pub(crate) fn bash_reply_persisted(display: &TaskDisplay) {
    bash_reply_persisted_in(&CALLS, display);
}

fn bash_reply_persisted_in(registry: &Captures, display: &TaskDisplay) {
    if display.reply_produced && display.deferred {
        retire_capture(registry, &display.capture_key, true);
    }
}

impl Capture {
    fn can_retire(&self) -> bool {
        if self.deferred {
            // A terminal result seen before the launch record stays in memory
            // until its completion is claimed and emitted.
            self.launch_admitted && self.deferred_persisted && !self.terminal
        } else {
            self.finished
        }
    }
}

fn retire_capture(registry: &Captures, key: &str, deferred_persisted: bool) {
    let Ok(mut calls) = registry.lock() else {
        return;
    };
    let retire = calls
        .get(key)
        .and_then(|capture| capture.lock().ok())
        .is_some_and(|mut capture| {
            capture.deferred_persisted |= deferred_persisted;
            capture.can_retire()
        });
    if retire {
        calls.remove(key);
    }
}

fn emit_bash_completion(capture: &mut Capture) {
    if capture.finished || !capture.completion_claimed {
        return;
    }
    let Some(BashBodies {
        facts: mut record,
        mut detail,
        tail,
    }) = capture.terminal_bash.clone()
    else {
        return;
    };
    record["taskId"] = detail.get("taskId").cloned().unwrap_or(Value::Null);
    record["phase"] = json!("background_finished");
    detail["phase"] = json!("background_finished");
    if let Some(held_ms) = capture.held_ms {
        record["heldMs"] = json!(held_ms);
    }
    if let Some(tail) = tail {
        record["tail"] = tail;
    }
    emit(capture, DisplayKind::Detail, detail);
    emit(capture, DisplayKind::Completion, record);
    capture.finished = true;
}

pub(crate) fn bash_terminal(
    display: &TaskDisplay,
    mut bodies: BashBodies,
    task_id: &str,
    duration_ms: Option<u64>,
) {
    // Losing track of deferred work is not proof that it ended. The initial
    // record stays, but neither a completion nor a terminal detail is written.
    if bodies.facts["reason"] == "fate_unknown" {
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
    bodies.facts["taskId"] = json!(task_id);
    bodies.detail["taskId"] = json!(task_id);
    if let Some(duration) = duration_ms {
        bodies.facts["runDurationMs"] = json!(duration);
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
            write_input_path: None,
            began: None,
            processing_ms: 0,
            held_at: None,
            held_ms: display.held_ms,
            record_sent: true,
            launch_admitted: true,
            deferred_persisted: true,
            finished: false,
            files: Vec::new(),
            file_snapshot: None,
            bash: None,
            terminal_bash: Some(bodies),
            deferred: true,
            terminal: true,
            completion_claimed: true,
        };
        emit_bash_completion(&mut capture);
    }
}

pub(crate) use crate::late_result_bus::display::declared_display;

pub(crate) fn take_due_read() -> Option<(String, Vec<u8>)> {
    with_outbox(|outbox| {
        let now = std::time::Instant::now();
        let id = outbox.reads.due(now)?;
        let bytes = outbox.reads.read(&id, now)?;
        Some((id, bytes))
    })
    .flatten()
}

pub(crate) fn mark_read_published(id: &str, published: bool) {
    with_outbox(|outbox| {
        outbox
            .reads
            .published(id, published, std::time::Instant::now());
    });
}

pub(crate) fn with_outbox<T>(f: impl FnOnce(&mut Outbox) -> T) -> Option<T> {
    let writer = SENDER.get()?;
    let mut outbox = writer.outbox.try_lock().ok()?;
    Some(f(&mut outbox))
}

pub(crate) async fn announce(
    id: String,
    bytes: Vec<u8>,
    publish: impl std::future::Future<Output = Result<(), &'static str>>,
) -> Result<(), &'static str> {
    let writer = SENDER.get().ok_or("display_store_not_started")?;
    announce_retained(writer, id, bytes, publish).await
}

async fn announce_retained(
    writer: &Writer,
    id: String,
    bytes: Vec<u8>,
    publish: impl std::future::Future<Output = Result<(), &'static str>>,
) -> Result<(), &'static str> {
    if !writer.store.persist(id.clone(), bytes).await? {
        return Err("display_store_deferred");
    }
    publish.await?;
    writer.store.acknowledged(id).await
}

pub(crate) fn check_reader(
    principal: Option<&Principal>,
    discovered: Option<&str>,
) -> Result<(), subc_protocol::ErrorBody> {
    if matches!((principal, discovered), (Some(Principal::Reserved { module_id }), Some(reader)) if module_id == reader)
    {
        Ok(())
    } else {
        Err(subc_protocol::ErrorBody::new(
            cortexkit_role_tool_display::DISPLAY_READ_DENIED,
            "display reads are restricted to the discovered display server",
        ))
    }
}

pub(crate) async fn read(
    principal: Option<&Principal>,
    arguments: Value,
) -> Result<Vec<u8>, subc_protocol::ErrorBody> {
    if !matches!(principal, Some(Principal::Reserved { .. })) {
        return check_reader(principal, None).map(|_| Vec::new());
    }
    let writer = SENDER
        .get()
        .ok_or_else(cortexkit_role_tool_display::not_ready)?;
    let discovered = if arguments
        .get("record_id")
        .and_then(Value::as_str)
        .is_some_and(super::notices::is_notice_id)
    {
        if super::notices::WRITER_CAPABILITY.is_none() {
            None
        } else {
            crate::late_result_bus::notice_display_reader()?
        }
    } else {
        crate::late_result_bus::display_reader()?
    };
    read_for_reader(
        writer,
        principal,
        discovered.as_deref(),
        arguments,
        Instant::now(),
        crate::db::display_records::now_ms(),
    )
    .await
}

async fn read_for_reader(
    writer: &Writer,
    principal: Option<&Principal>,
    discovered: Option<&str>,
    arguments: Value,
    now: Instant,
    now_ms: i64,
) -> Result<Vec<u8>, subc_protocol::ErrorBody> {
    check_reader(principal, discovered)?;
    let request = read_request(arguments)?;
    if super::notices::is_notice_id(&request.record_id) {
        // Pending notices are not readable from memory before they reach the store.
        return writer.store.read(request.record_id, now_ms).await;
    }
    let memory = writer
        .outbox
        .lock()
        .map_err(|_| cortexkit_role_tool_display::not_ready())?
        .reads
        .read(&request.record_id, now);
    if let Some(bytes) = memory {
        return Ok(bytes);
    }
    writer.store.read(request.record_id, now_ms).await
}

fn read_request(
    arguments: Value,
) -> Result<cortexkit_role_tool_display::DisplayRead, subc_protocol::ErrorBody> {
    use cortexkit_role_tool_display::{DisplayRead, InvalidParams};
    let request: DisplayRead =
        serde_json::from_value(arguments).map_err(|_| InvalidParams::new("record_id").refusal())?;
    request.validate().map_err(|error| error.refusal())?;
    Ok(request)
}

#[cfg(test)]
#[path = "display/scheduler_tests.rs"]
mod scheduler_tests;
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

pub(crate) fn handler_ready(
    ledger_key: Option<&crate::db::call_ledger::Key>,
    response: &crate::protocol::Response,
) {
    let capture =
        ledger_key.and_then(|ledger_key| CALLS.lock().ok()?.get(&key(ledger_key)).cloned());
    if let Some(capture) = capture {
        if let Ok(mut capture) = capture.lock() {
            if let Some(began) = capture.began.take() {
                capture.processing_ms = capture
                    .processing_ms
                    .saturating_add(began.elapsed().as_millis() as u64);
            }
            if capture.tool != "bash" && capture.file_snapshot.is_none() {
                let (mut record, detail) = file_bodies(&capture, response.success);
                // When an edit skips files, it sets `complete: false` instead of `partial: true`.
                if response.success
                    && (response.data["partial"].as_bool() == Some(true)
                        || (capture.tool == "edit"
                            && response.data["complete"].as_bool() == Some(false)))
                {
                    record["outcome"] = json!("partial");
                }
                if capture.tool == "edit" && response.data["syntax_valid"].as_bool() == Some(false)
                {
                    record["reason"] = json!("invalid_syntax");
                }
                capture.file_snapshot = Some((record, detail));
            }
        }
    }
}
