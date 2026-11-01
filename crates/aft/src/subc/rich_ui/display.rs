//! Optional provider-owned display publication, isolated from runner replies.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Instant;

use cortexkit_role_tool_display::{schema::{validate_completion, validate_record}, DisplayKind};
use serde_json::{json, Value};
use subc_protocol::Principal;
use super::{outbox::{self, Identity, Outbox}, records};

static SENDER: OnceLock<tokio::sync::mpsc::Sender<Publication>> = OnceLock::new();
static CALLS: LazyLock<Mutex<HashMap<String, Arc<Mutex<Capture>>>>> = LazyLock::new(Default::default);
thread_local! { static ACTIVE: RefCell<Option<Arc<Mutex<Capture>>>> = const { RefCell::new(None) }; }

pub(crate) fn key(key: &crate::db::call_ledger::Key) -> String { format!("{}:{}{}", key.carrier.len(), key.carrier, key.call_key) }

pub(crate) struct Capture {
    identity: Identity,
    tool: String,
    root: PathBuf,
    began: Option<Instant>,
    held_at: Option<Instant>,
    held_ms: Option<u64>,
    record_sent: bool,
    finished: bool,
    files: Vec<FileChange>,
    bash: Option<(Value, Value)>,
    deferred: bool,
}

#[derive(Clone)]
struct FileChange { path: PathBuf, old_path: Option<PathBuf>, before: Option<String>, after: Option<String>, change: &'static str }

struct Publication { identity: Identity, kind: DisplayKind, body: Value }

pub(crate) fn register(ledger_key: &crate::db::call_ledger::Key, route: &crate::subc::RouteIdentity, call: &subc_protocol::tool_call::ToolCallRequest) {
    if SENDER.get().is_none() || !matches!(call.name.as_str(), "bash" | "edit" | "write" | "apply_patch") { return; }
    let crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { principal_id: Some(principal), .. } = &route.spawn_principal else { return; };
    // Plugin routes are direct/unverified. The harness name is not evidence:
    // an attested runner can legitimately use the same harness as a plugin.
    let Some(module_id) = principal.strip_prefix("reserved:") else { return; };
    let (runner, call_key, pin) = if let Some(origin) = &call.origin {
        (origin.carrier.clone(), Some(origin.call_key.clone()), origin.schema_pin.clone())
    } else { (Principal::Reserved { module_id: module_id.into() }, call.call_key.clone(), call.schema_pin.clone()) };
    let (Some(call_key), Some(schema_pin)) = (call_key, pin) else { return; };
    if crate::subc::tool_views::view(&schema_pin).is_none() { return; }
    let identity = Identity { runner, call_key, schema_pin };
    if let Ok(mut calls) = CALLS.lock() {
        if calls.len() >= 64 { return; }
        calls.entry(key(ledger_key)).or_insert_with(|| Arc::new(Mutex::new(Capture {
            identity, tool: call.name.clone(), root: route.project_root.clone(), began: None,
            held_at: None, held_ms: None, record_sent: false, finished: false, files: Vec::new(), bash: None, deferred: false,
        })));
    }
}

pub(crate) struct Guard(Option<Arc<Mutex<Capture>>>);
impl Drop for Guard { fn drop(&mut self) { ACTIVE.with(|slot| { slot.replace(self.0.take()); }); } }

pub(crate) fn enter(key_value: Option<&crate::db::call_ledger::Key>) -> Guard {
    let capture = key_value.and_then(|key_value| CALLS.lock().ok()?.get(&key(key_value)).cloned());
    if let Some(capture) = &capture {
        if let Ok(mut capture) = capture.lock() {
            capture.began.get_or_insert_with(Instant::now);
            if let Some(held_at) = capture.held_at.take() { capture.held_ms = Some(held_at.elapsed().as_millis() as u64); }
        }
    }
    Guard(ACTIVE.with(|slot| slot.replace(capture)))
}

pub(crate) fn current_identity() -> Option<Identity> { ACTIVE.with(|slot| Some(slot.borrow().as_ref()?.lock().ok()?.identity.clone())) }

pub(crate) fn file(file: &str, old_path: Option<&str>, before: Option<&str>, after: Option<&str>, change: &'static str) {
    ACTIVE.with(|slot| {
        let active = slot.borrow();
        let Some(capture) = active.as_ref() else { return; };
        let Ok(mut capture) = capture.lock() else { return; };
        let path = PathBuf::from(file);
        let path = if path.is_absolute() { path } else { capture.root.join(path) };
        let old_path = old_path.map(PathBuf::from).map(|path| if path.is_absolute() { path } else { capture.root.join(path) });
        if let Some(entry) = capture.files.iter_mut().find(|entry| entry.path == path) {
            entry.after = after.map(str::to_owned);
        } else {
            capture.files.push(FileChange { path, old_path, before: before.map(str::to_owned), after: after.map(str::to_owned), change });
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
            Err(_) => { suffix.push(ancestor.file_name()?); ancestor = ancestor.parent()?; }
        }
    };
    let mut resolved = resolved;
    for component in suffix.into_iter().rev() { resolved.push(component); }
    let root = root.canonicalize().ok()?;
    let relative = resolved.strip_prefix(root).ok()?.to_str()?.replace('\\', "/");
    cortexkit_role_tool_display::schema::validate_project_path(&relative).then_some(relative)
}

fn file_bodies(capture: &Capture, success: bool) -> (Value, Option<Value>) {
    let mut record = json!({"outcome": if success {"success"} else if capture.files.is_empty() {"error"} else {"partial"}, "fileCount":0,"modifiedCount":0,"createdCount":0,"deletedCount":0,"movedCount":0});
    let mut files = Vec::new();
    let (mut added, mut removed, mut known) = (0u64, 0u64, true);
    let mut changes = capture.files.clone();
    changes.sort_by(|a,b| a.path.cmp(&b.path));
    for change in changes {
        if change.change == "modified" && change.before == change.after { continue; }
        let category = format!("{}Count", change.change);
        record["fileCount"] = json!(record["fileCount"].as_u64().unwrap() + 1);
        record[&category] = json!(record[&category].as_u64().unwrap() + 1);
        let path = relative(&capture.root, &change.path);
        let old_path = change.old_path.as_ref().map(|path| relative(&capture.root, path));
        let mut entry = json!({"change":change.change});
        if let (Some(before), Some(after)) = (change.before.as_deref(), change.after.as_deref()) {
            let counts = crate::edit::compute_diff_counts(before, after);
            added += counts["additions"].as_u64().unwrap_or(0);
            removed += counts["deletions"].as_u64().unwrap_or(0);
        } else { known = false; }
        if path.is_none() || old_path.as_ref().is_some_and(Option::is_none) {
            record["location"] = json!("outside_project");
            entry["location"] = json!("outside_project");
            entry["diffOmitted"] = json!(true);
        } else {
            entry["path"] = json!(path);
            if let Some(old_path) = old_path { entry["oldPath"] = json!(old_path); }
            // The vault has no per-agent matcher for file content yet. Withhold
            // content rather than persist a diff containing an unresolved secret.
            entry["diffOmitted"] = json!(true);
        }
        files.push(entry);
    }
    if known && !files.is_empty() { record["addedLines"] = json!(added); record["removedLines"] = json!(removed); }
    if files.len() == 1 && files[0].get("location").is_none() { record["path"] = files[0]["path"].clone(); }
    let detail = (!files.is_empty()).then(|| {
        let mut detail = json!({"schema":"aft/files-detail/1","revision":1,"files":files});
        if let Some(location) = record.get("location") { detail["location"] = location.clone(); }
        detail
    });
    (record, detail)
}

fn emit(capture: &Capture, kind: DisplayKind, body: Value) {
    let schema = if capture.tool == "bash" { records::bash_schema() } else { records::files_schema() };
    let Some(map) = body.as_object() else { return; };
    let valid = match kind { DisplayKind::Record => validate_record(map, &schema), DisplayKind::Completion => validate_completion(map, &schema), _ => cortexkit_role_tool_display::schema::validate_detail(map) };
    if valid.is_err() { return; }
    if let Some(sender) = SENDER.get() { let _ = sender.try_send(Publication { identity: capture.identity.clone(), kind, body }); }
}

/// Called only for newly executed keyed calls, never for ledger replays.
pub(crate) fn reply(ledger_key: &crate::db::call_ledger::Key, held: bool, error: bool, reason: Option<&str>) {
    let capture = CALLS.lock().ok().and_then(|calls| calls.get(&key(ledger_key)).cloned());
    let Some(capture) = capture else { return; };
    let Ok(mut capture) = capture.lock() else { return; };
    if capture.finished { return; }
    let held_ms = capture.held_ms.unwrap_or(0);
    let duration = capture.began.map_or(0, |began| began.elapsed().as_millis() as u64).saturating_sub(held_ms);
    if held {
        if !capture.record_sent {
            let mut body = json!({"durationMs":duration,"outcome":"held","reason":"consent"});
            if capture.tool == "bash" { body["phase"] = json!("held"); body["execution"] = json!("local"); }
            emit(&capture, DisplayKind::Record, body);
            capture.record_sent = true;
            capture.held_at = Some(Instant::now());
        }
        return;
    }
    let completion = capture.record_sent;
    let (mut body, detail) = if capture.tool == "bash" {
        capture.bash.clone().map(|(body,detail)| (body, Some(detail))).unwrap_or_else(|| (json!({"outcome":"error","execution":"local","phase":"foreground_finished"}), None))
    } else { file_bodies(&capture, !error) };
    if capture.tool == "bash" && capture.deferred {
        if completion { return; }
        body["phase"] = json!("background_started");
        body["outcome"] = json!("success");
        if let Some(map) = body.as_object_mut() { map.remove("exitCode"); map.remove("endReason"); }
    }
    body["durationMs"] = json!(duration);
    if let Some(held_ms) = capture.held_ms { body["heldMs"] = json!(held_ms); }
    if error && body.get("reason").is_none() { body["reason"] = json!(if matches!(reason, Some("cancelled")) { "cancelled" } else if matches!(reason, Some("consent")) {"consent"} else {"permission_denied"}); }
    emit(&capture, if completion { DisplayKind::Completion } else { DisplayKind::Record }, body);
    capture.record_sent = true;
    if !capture.deferred {
        if let Some(detail) = detail { emit(&capture, DisplayKind::Detail, detail); }
        capture.finished = true;
        drop(capture);
        if let Ok(mut calls) = CALLS.lock() { calls.remove(&key(ledger_key)); }
    }
}

pub(crate) fn start(connection_file: PathBuf) {
    if std::env::var("AFT_TOOL_DISPLAY").as_deref() != Ok("1") { return; }
    let (sender, receiver) = tokio::sync::mpsc::channel(192);
    if SENDER.set(sender).is_err() { return; }
    tokio::spawn(send_loop(connection_file, receiver));
}

async fn send_loop(connection_file: PathBuf, mut receiver: tokio::sync::mpsc::Receiver<Publication>) {
    use subc_client_rs::{BindIdentity, ConsumerOptions, SubcConsumer};
    let mut outbox = Outbox::new();
    let mut consumer = None;
    loop {
        tokio::select! {
            publication = receiver.recv() => {
                let Some(mut publication) = publication else { return; };
                if publication.kind == DisplayKind::Detail {
                    let Some(cut) = records::cut_detail(publication.body, 3_145_728) else { continue; };
                    publication.body = cut;
                }
                outbox.admit(publication.identity, publication.kind, publication.body, &outbox::ceilings(), Instant::now());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
        }
        let Some(entry) = outbox.pop_due(Instant::now()) else { continue; };
        if consumer.is_none() {
            consumer = SubcConsumer::connect(&connection_file, ConsumerOptions::default()).await.ok();
        }
        let mut retry = true;
        if let Some(client) = consumer.as_ref() {
            // resolve_provider uses catalog capabilities, includes warming modules,
            // and refuses ambiguity. Resolve again before every write and retry.
            if let Ok(module_id) = client.resolve_provider("tool-display/v1").await {
                let route = client.open_route(subc_protocol::RouteTarget::ToolProvider { module_id }, BindIdentity::new(".", "aft", "tool-display")).await;
                if let Ok(route) = route {
                    if let Ok(bytes) = client.request(&route, entry.frame.clone()).await {
                        if let Ok(reply) = serde_json::from_slice::<Value>(&bytes) {
                            retry = !matches!(reply["result"].as_str(), Some("stored" | "already_stored"));
                            if matches!(reply["code"].as_str(), Some("display_write_conflict" | "invalid_params" | "display_writer_unverified" | "display_too_large")) { retry = false; }
                        }
                    }
                    let _ = client.close_handle(&route).await;
                }
            }
        }
        if retry { outbox.retry(entry, Instant::now()); }
    }
}
