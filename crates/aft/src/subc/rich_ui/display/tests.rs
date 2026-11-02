use super::*;
use crate::bash_background::{persistence::PersistedTask, BgTaskStatus};
use crate::db::call_ledger::Key;
use std::time::Duration;

struct Fixture {
    _dir: tempfile::TempDir,
    key: Key,
    capture: Arc<Mutex<Capture>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        CALLS.lock().unwrap().remove(&key(&self.key));
        TEST_SINK.with(|sink| {
            sink.replace(None);
        });
    }
}
fn fixture(tool: &str, name: &str) -> Fixture {
    TEST_SINK.with(|sink| {
        sink.replace(Some(Vec::new()));
    });
    let (dir, root) = crate::subc::test_support::test_root(name);
    let mut route = crate::subc::test_support::route_identity(&root, name);
    let data = Arc::get_mut(&mut route.0).unwrap();
    data.role = crate::subc::tool_provider::RouteRole::ToolProviderV1;
    if let crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut data.spawn_principal
    {
        *principal_id = Some("reserved:broca".into());
    }
    let mut call = subc_protocol::tool_call::ToolCallRequest::new(tool, json!({}));
    call.call_key = Some(name.into());
    call.schema_pin = Some(
        crate::subc::tool_views::SHIPPED
            .keys()
            .find(|pin| pin.starts_with(&format!("tp1:{tool}:")))
            .unwrap()
            .clone(),
    );
    let key_value = crate::subc::tool_provider::LedgerEdge::key(&route, name);
    register(&key_value, &route, &call);
    let capture = CALLS.lock().unwrap()[&key(&key_value)].clone();
    Fixture {
        _dir: dir,
        key: key_value,
        capture,
    }
}
fn publications() -> Vec<Publication> {
    TEST_SINK.with(|sink| sink.borrow().clone().unwrap())
}
fn held(f: &Fixture) {
    reply(&f.key, true, false, None);
    f.capture.lock().unwrap().held_at = Some(Instant::now() - Duration::from_secs(2));
}
fn task() -> PersistedTask {
    PersistedTask::starting(
        "bash-demo1".into(),
        "s".into(),
        "printf ok".into(),
        PathBuf::from("."),
        None,
        None,
        true,
        false,
    )
}
fn terminal(f: &Fixture, display: &mut TaskDisplay, status: BgTaskStatus, exit: Option<i64>) {
    let mut metadata = task();
    metadata.status = status;
    metadata.exit_code = exit;
    metadata.group_kill_sent = metadata.status == BgTaskStatus::Killed;
    let bodies = bash_bodies(&metadata, Some(3), Some(("ok\n".into(), "".into())));
    let snapshot = display.clone();
    if display.reply_produced && display.deferred {
        display.attempted = true;
    }
    bash_terminal(&snapshot, bodies, "bash-demo1", Some(2400));
    let capture = f.capture.lock().unwrap();
    assert!(capture.terminal || capture.finished);
}

#[test]
fn held_file_completion_excludes_held_time_and_aggregates_actual_changes() {
    let f = fixture("edit", "display-held-file");
    held(&f);
    let _active = enter(Some(&f.key));
    let path = f._dir.path().join("a.rs");
    std::fs::write(&path, "new\n").unwrap();
    file(
        path.to_str().unwrap(),
        None,
        Some("old\n"),
        Some("new\n"),
        "modified",
    );
    f.capture.lock().unwrap().began = Some(Instant::now() - Duration::from_millis(12));
    reply(&f.key, false, false, None);
    let writes = publications();
    assert_eq!(
        writes
            .iter()
            .filter(|entry| entry.kind == DisplayKind::Record)
            .count(),
        1
    );
    let completion = writes
        .iter()
        .find(|entry| entry.kind == DisplayKind::Completion)
        .unwrap();
    assert_eq!(completion.body["fileCount"], 1);
    assert_eq!(completion.body["addedLines"], 1);
    assert!(completion.body["heldMs"].as_u64().unwrap() >= 2000);
    assert!(completion.body["durationMs"].as_u64().unwrap() < 1000);
    assert_eq!(
        writes
            .iter()
            .filter(|entry| entry.kind == DisplayKind::Detail)
            .count(),
        1
    );
}

#[test]
fn held_bash_foreground_completes_once_after_the_reply_is_produced() {
    let f = fixture("bash", "display-held-foreground");
    held(&f);
    let _active = enter(Some(&f.key));
    let mut display = current_task().unwrap();
    terminal(&f, &mut display, BgTaskStatus::Completed, Some(0));
    assert_eq!(
        publications().len(),
        1,
        "terminal race must wait for the original reply"
    );
    let mut metadata = task();
    metadata.status = BgTaskStatus::Completed;
    metadata.exit_code = Some(0);
    bash_reply(
        &mut display,
        bash_bodies(&metadata, Some(3), Some(("ok\n".into(), "".into()))),
        false,
    );
    reply(&f.key, false, false, None);
    reply(&f.key, false, false, None);
    let writes = publications();
    let completed: Vec<_> = writes
        .iter()
        .filter(|entry| entry.kind == DisplayKind::Completion)
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].body["phase"], "foreground_finished");
    assert!(completed[0].body.get("taskId").is_none());
    assert!(completed[0].body.get("heldMs").is_some());
}

fn held_background_case(name: &str, status: BgTaskStatus, exit: Option<i64>, reason: Option<&str>) {
    let f = fixture("bash", name);
    held(&f);
    let _active = enter(Some(&f.key));
    let mut display = current_task().unwrap();
    let mut running = task();
    running.status = BgTaskStatus::Running;
    bash_reply(&mut display, bash_bodies(&running, Some(0), None), true);
    reply(&f.key, false, false, None);
    assert_eq!(
        publications().len(),
        1,
        "releasing a hold into background work is not a completion"
    );
    terminal(&f, &mut display, status.clone(), exit);
    terminal(&f, &mut display, status, exit);
    let writes = publications();
    let completed: Vec<_> = writes
        .iter()
        .filter(|entry| entry.kind == DisplayKind::Completion)
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].body["phase"], "background_finished");
    assert!(completed[0].body["heldMs"].as_u64().unwrap() >= 2000);
    assert_eq!(
        completed[0].body.get("reason").and_then(Value::as_str),
        reason
    );
    assert_eq!(
        completed[0].body.get("exitCode").and_then(Value::as_i64),
        exit
    );
    assert_eq!(
        writes
            .iter()
            .filter(|entry| entry.kind == DisplayKind::Detail)
            .count(),
        1
    );
    if reason == Some("fate_unknown") {
        assert!(completed[0].body.get("endReason").is_none());
    }
}
#[test]
fn held_background_records_exit_not_hold_release() {
    held_background_case("display-held-exit", BgTaskStatus::Completed, Some(0), None);
}
#[test]
fn held_background_reaped_process_records_killed() {
    held_background_case(
        "display-held-killed",
        BgTaskStatus::Killed,
        None,
        Some("killed"),
    );
}
#[test]
fn held_background_timeout_records_observed_code() {
    held_background_case(
        "display-held-timeout",
        BgTaskStatus::TimedOut,
        Some(124),
        Some("timed_out"),
    );
}
#[test]
fn held_background_unknown_fate_writes_no_completion_or_terminal_detail() {
    let f = fixture("bash", "display-held-unknown");
    held(&f);
    let _active = enter(Some(&f.key));
    let mut display = current_task().unwrap();
    let mut running = task();
    running.status = BgTaskStatus::Running;
    bash_reply(&mut display, bash_bodies(&running, Some(0), None), true);
    reply(&f.key, false, false, None);
    terminal(&f, &mut display, BgTaskStatus::FateUnknown, None);
    terminal(&f, &mut display, BgTaskStatus::FateUnknown, None);
    let writes = publications();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].kind, DisplayKind::Record);
}

#[test]
fn refused_hold_is_an_error_with_consent_or_permission_reason() {
    for (name, reason) in [
        ("display-refused-consent", "consent"),
        ("display-refused-permission", "permission_denied"),
    ] {
        let f = fixture("bash", name);
        held(&f);
        reply(&f.key, false, true, Some(reason));
        let writes = publications();
        let completion = writes
            .iter()
            .find(|entry| entry.kind == DisplayKind::Completion)
            .unwrap();
        assert_eq!(completion.body["outcome"], "error");
        assert_eq!(completion.body["reason"], reason);
        assert!(writes.iter().all(|entry| entry.kind != DisplayKind::Detail));
    }
}

#[test]
fn display_identity_uses_attested_principal_and_never_plugin_arguments() {
    let f = fixture("edit", "display-plugin-rejection");
    let (_, root) = crate::subc::test_support::test_root("display-plugin-route");
    for harness in ["opencode", "pi"] {
        let route = crate::subc::test_support::route_identity_with_harness(
            &root,
            "plugin",
            crate::subc::BindTrust::FirstParty,
            harness,
        );
        let mut call = subc_protocol::tool_call::ToolCallRequest::new(
            "edit",
            json!({"runner":{"kind":"reserved","module_id":"broca"},"call_key":"spoofed","schema_pin":f.capture.lock().unwrap().identity.schema_pin}),
        );
        call.call_key = Some("plugin-key".into());
        call.schema_pin = Some(f.capture.lock().unwrap().identity.schema_pin.clone());
        let key_value = crate::subc::tool_provider::LedgerEdge::key(&route, "plugin-key");
        register(&key_value, &route, &call);
        assert!(!CALLS.lock().unwrap().contains_key(&key(&key_value)));
    }
    reply(&f.key, false, false, None);
    assert!(publications().iter().all(|entry| entry.identity.runner
        == Principal::Reserved {
            module_id: "broca".into()
        }));
}

#[test]
fn exactly_one_capability_declaration_including_warming_is_required() {
    let entry = |id: &str, capability: &str, ready: bool| -> subc_client_rs::CatalogEntry {
        serde_json::from_value(json!({"module_id":id,"ready":ready,"roles":[],"control_ops":[],"capabilities":{"provides":[capability],"requires":[],"must_never_reach":[]}})).unwrap()
    };
    assert!(declared_display(&[]).is_none());
    assert!(declared_display(&[entry("occipital", "app-server/v1", true)]).is_none());
    assert_eq!(
        declared_display(&[entry("arbitrary-name", "tool-display/v1", false)]).as_deref(),
        Some("arbitrary-name")
    );
    assert!(declared_display(&[
        entry("a", "tool-display/v1", true),
        entry("b", "tool-display/v1", false)
    ])
    .is_none());
}

#[test]
fn outside_project_and_symlink_escapes_never_disclose_paths_or_content() {
    let f = fixture("write", "display-outside");
    let external = tempfile::tempdir().unwrap();
    let path = external.path().join("private.txt");
    std::fs::write(&path, "secret").unwrap();
    let _active = enter(Some(&f.key));
    file(
        path.to_str().unwrap(),
        None,
        Some("old\n"),
        Some("secret\n"),
        "modified",
    );
    let capture = f.capture.lock().unwrap();
    let (record, detail) = file_bodies(&capture, true);
    assert_eq!(record["location"], "outside_project");
    let detail = detail.unwrap();
    assert!(record.get("path").is_none());
    assert_eq!(
        detail["files"][0],
        json!({"change":"modified","location":"outside_project","diffOmitted":true})
    );
    assert!(!serde_json::to_string(&detail)
        .unwrap()
        .contains("private.txt"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(external.path(), f._dir.path().join("escape")).unwrap();
        assert!(relative(f._dir.path(), &f._dir.path().join("escape/private.txt")).is_none());
    }
}

#[test]
fn per_call_masker_runs_before_detail_sizing_and_unavailable_masker_withholds() {
    use std::ffi::OsStr;
    let f = fixture("edit", "display-masker");
    let path = f._dir.path().join("a.rs");
    std::fs::write(&path, "new\n").unwrap();
    let _active = enter(Some(&f.key));
    file(
        path.to_str().unwrap(),
        None,
        Some("old\n"),
        Some("agent-opaque-value\n"),
        "modified",
    );
    let mut capture = f.capture.lock().unwrap();
    let (_, unavailable) = file_bodies(&capture, true);
    assert_eq!(unavailable.unwrap()["files"][0]["diffOmitted"], true);
    capture.masker = Some(crate::log_redact::SecretValues::from_environment([(
        OsStr::new("TOKEN"),
        OsStr::new("agent-opaque-value"),
    )]));
    let (_, detail) = file_bodies(&capture, true);
    let detail = detail.unwrap();
    let encoded = serde_json::to_string(&detail).unwrap();
    assert!(!encoded.contains("agent-opaque-value"));
    assert!(detail["files"][0]["diff"]
        .as_str()
        .unwrap()
        .contains("REDACTED"));
    let size = cortexkit_role_tool_display::canonical_body(detail.as_object().unwrap())
        .unwrap()
        .len();
    assert_eq!(records::cut_detail(detail.clone(), size), Some(detail));
}

#[test]
fn created_deleted_moved_and_repeated_hunks_count_actual_final_states() {
    let f = fixture("apply_patch", "display-file-categories");
    let _active = enter(Some(&f.key));
    let path = f._dir.path().join("a.rs");
    std::fs::write(&path, "final\n").unwrap();
    file(
        path.to_str().unwrap(),
        None,
        Some(""),
        Some("intermediate\n"),
        "created",
    );
    file(
        path.to_str().unwrap(),
        None,
        Some("intermediate\n"),
        Some("final\n"),
        "modified",
    );
    let cancelled = f._dir.path().join("cancelled.rs");
    file(
        cancelled.to_str().unwrap(),
        None,
        Some(""),
        Some("temporary\n"),
        "created",
    );
    file(
        cancelled.to_str().unwrap(),
        None,
        Some("temporary\n"),
        Some(""),
        "deleted",
    );
    let moved = f._dir.path().join("b.rs");
    std::fs::write(&moved, "same\n").unwrap();
    file(
        moved.to_str().unwrap(),
        Some(f._dir.path().join("old.rs").to_str().unwrap()),
        Some("same\n"),
        Some("same\n"),
        "moved",
    );
    let (record, detail) = file_bodies(&f.capture.lock().unwrap(), true);
    assert_eq!(record["fileCount"], 2);
    assert_eq!(record["createdCount"], 1);
    assert_eq!(record["movedCount"], 1);
    assert_eq!(record["deletedCount"], 0);
    assert_eq!(record["addedLines"], 1);
    assert!(record.get("path").is_none());
    assert_eq!(detail.unwrap()["files"].as_array().unwrap().len(), 2);
}

#[test]
fn exact_file_producer_matches_frozen_example_before_the_canonical_limit() {
    let f = fixture("edit", "display-file-exact");
    let _active = enter(Some(&f.key));
    let path = f._dir.path().join("src/a.rs");
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "new\n").unwrap();
    file(
        path.to_str().unwrap(),
        None,
        Some("old\n"),
        Some("new\n"),
        "modified",
    );
    let mut capture = f.capture.lock().unwrap();
    capture.masker = Some(crate::log_redact::SecretValues::default());
    let (mut record, detail) = file_bodies(&capture, true);
    record["durationMs"] = json!(12);
    assert_eq!(
        record,
        json!({"durationMs":12,"outcome":"success","fileCount":1,"modifiedCount":1,"createdCount":0,"deletedCount":0,"movedCount":0,"addedLines":1,"removedLines":1,"path":"src/a.rs"})
    );
    assert_eq!(
        detail.unwrap(),
        json!({"schema":"aft/files-detail/1","revision":1,"files":[{"change":"modified","path":"src/a.rs","diff":"--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}]})
    );
}

#[test]
fn retries_stop_on_real_divergence_and_success_but_not_ack_loss() {
    assert!(!retry_reply(br#"{"result":"stored"}"#));
    assert!(!retry_reply(br#"{"result":"already_stored"}"#));
    for code in [
        "display_write_conflict",
        "display_too_large",
        "display_writer_unverified",
        "invalid_params",
    ] {
        assert!(!retry_reply(
            &serde_json::to_vec(&json!({"code":code})).unwrap()
        ));
    }
    assert!(retry_reply(br#"{"code":"module_warming"}"#));
    assert!(retry_reply(b"lost ack"));
}

#[test]
fn blocked_display_and_full_wake_queue_never_delay_a_reply() {
    let (wake, mut receiver) = tokio::sync::mpsc::channel(1);
    let writer = Writer {
        outbox: Mutex::new(Outbox::new()),
        wake,
        limits: Mutex::new(outbox::ceilings()),
    };
    let identity = Identity {
        runner: Principal::Reserved {
            module_id: "broca".into(),
        },
        call_key: "nonblocking".into(),
        schema_pin: crate::subc::tool_views::SHIPPED
            .keys()
            .next()
            .unwrap()
            .clone(),
    };
    let held = writer.outbox.lock().unwrap();
    // The fake sender holds its queue lock and never acknowledges anything.
    // Enqueue must reject optional display work rather than wait for that lock.
    assert!(!enqueue_snapshot(
        &writer,
        identity.clone(),
        DisplayKind::Record,
        json!({"durationMs":1,"outcome":"success"})
    ));
    drop(held);
    assert!(enqueue_snapshot(
        &writer,
        identity.clone(),
        DisplayKind::Record,
        json!({"durationMs":1,"outcome":"success"})
    ));
    assert!(enqueue_snapshot(
        &writer,
        identity,
        DisplayKind::Detail,
        json!({"schema":"test/detail/1"})
    ));
    assert!(receiver.try_recv().is_ok());
    assert!(receiver.try_recv().is_err());
    assert!(writer
        .outbox
        .lock()
        .unwrap()
        .pop_due(Instant::now())
        .is_some());
}

#[test]
fn native_edit_write_and_patch_capture_final_mutations_without_reply_fields() {
    use crate::protocol::RawRequest;
    for tool in ["write", "edit", "apply_patch"] {
        let f = fixture(tool, &format!("display-native-{tool}"));
        let ctx = crate::subc::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(f._dir.path().into());
            config.storage_dir = Some(f._dir.path().join("storage"));
            config.format_on_edit = false;
        });
        let path = f._dir.path().join("a.txt");
        if tool != "write" {
            std::fs::write(&path, "old\n").unwrap();
        }
        let params = match tool {
            "write" => json!({"file":path,"content":"new\n"}),
            "edit" => json!({"file":path,"match":"old","replacement":"new"}),
            _ => {
                json!({"patch_text":format!("*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch", path.display())})
            }
        };
        let mut request = params;
        request["id"] = json!("native");
        request["command"] = json!(tool);
        let request: RawRequest = serde_json::from_value(request).unwrap();
        let _active = enter(Some(&f.key));
        let response = match tool {
            "write" => crate::commands::write::handle_write(&request, &ctx),
            "edit" => crate::commands::edit_match::handle_edit_match(&request, &ctx),
            _ => crate::commands::apply_patch::handle_apply_patch(&request, &ctx),
        };
        assert!(response.success, "{tool}: {response:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(response.data.get("view").is_none());
        assert!(response.data.get("record").is_none());
        let (record, _) = file_bodies(&f.capture.lock().unwrap(), true);
        assert_eq!(record["fileCount"], 1, "{tool}: {record}");
        assert_eq!(
            record[if tool == "write" {
                "createdCount"
            } else {
                "modifiedCount"
            }],
            1
        );
    }
}

#[test]
fn publication_is_disabled_without_an_explicit_writer() {
    assert!(!enabled());
    assert!(current_task().is_none());
}

fn runner_route(f: &Fixture, module: &str) -> crate::subc::RouteIdentity {
    let root = crate::path_identity::ProjectRootId::from_path(f._dir.path()).unwrap();
    let mut route = crate::subc::test_support::route_identity(&root, "display-identity");
    let data = Arc::get_mut(&mut route.0).unwrap();
    data.role = crate::subc::tool_provider::RouteRole::ToolProviderV1;
    if let crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut data.spawn_principal
    {
        *principal_id = Some(format!("reserved:{module}"));
    }
    route
}

#[test]
fn direct_runner_calls_require_their_own_key_and_pin() {
    let f = fixture("edit", "display-direct-identity");
    let route = runner_route(&f, "broca");
    let pin = f.capture.lock().unwrap().identity.schema_pin.clone();
    for (name, call_key, schema_pin) in [
        ("missing-key", None, Some(pin.clone())),
        ("missing-pin", Some("missing-pin".to_owned()), None),
    ] {
        let mut call = subc_protocol::tool_call::ToolCallRequest::new("edit", json!({}));
        call.call_key = call_key;
        call.schema_pin = schema_pin;
        let ledger = crate::subc::tool_provider::LedgerEdge::key(&route, name);
        register(&ledger, &route, &call);
        assert!(!CALLS.lock().unwrap().contains_key(&key(&ledger)));
    }
    reply(&f.key, false, false, None);
    let writes = publications();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].identity.call_key, "display-direct-identity");
    assert_eq!(writes[0].identity.schema_pin, pin);
}

#[test]
fn relays_write_only_the_origin_identity_not_the_forwarders_key_or_pin() {
    let f = fixture("edit", "display-relay-base");
    let route = runner_route(&f, "basal");
    let pin = f.capture.lock().unwrap().identity.schema_pin.clone();
    let mut call = subc_protocol::tool_call::ToolCallRequest::new("edit", json!({}));
    call.call_key = Some("forwarder-key".into());
    call.schema_pin = Some(
        crate::subc::tool_views::SHIPPED
            .keys()
            .find(|pin| pin.starts_with("tp1:bash:"))
            .unwrap()
            .clone(),
    );
    call.origin = Some(
        subc_protocol::tool_call::CallOrigin::new(
            Principal::Reserved {
                module_id: "thalamus-harness".into(),
            },
            "runner-original",
        )
        .with_schema_pin(pin.clone()),
    );
    let ledger = crate::subc::tool_provider::LedgerEdge::key(&route, "forwarder-key");
    register(&ledger, &route, &call);
    reply(&ledger, false, false, None);
    let writes = publications();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].identity.runner,
        Principal::Reserved {
            module_id: "thalamus-harness".into()
        }
    );
    assert_eq!(writes[0].identity.call_key, "runner-original");
    assert_eq!(writes[0].identity.schema_pin, pin);
}

#[test]
fn incomplete_origins_and_basal_inner_calls_without_origin_write_nothing() {
    let f = fixture("edit", "display-origin-rejections");
    let route = runner_route(&f, "basal");
    let pin = f.capture.lock().unwrap().identity.schema_pin.clone();
    let mut call = subc_protocol::tool_call::ToolCallRequest::new("edit", json!({}));
    call.call_key = Some("outer".into());
    call.schema_pin = Some(pin.clone());
    let ledger = crate::subc::tool_provider::LedgerEdge::key(&route, "outer");
    register(&ledger, &route, &call);
    assert!(!CALLS.lock().unwrap().contains_key(&key(&ledger)));
    call.origin = Some(subc_protocol::tool_call::CallOrigin::new(
        Principal::Reserved {
            module_id: "broca".into(),
        },
        "original",
    ));
    register(&ledger, &route, &call);
    assert!(!CALLS.lock().unwrap().contains_key(&key(&ledger)));
    call.origin = Some(
        subc_protocol::tool_call::CallOrigin::new(
            Principal::Reserved {
                module_id: "broca".into(),
            },
            "",
        )
        .with_schema_pin(pin.clone()),
    );
    register(&ledger, &route, &call);
    assert!(!CALLS.lock().unwrap().contains_key(&key(&ledger)));
    let missing_key = json!({"name":"edit","arguments":{},"call_key":"outer","schema_pin":pin,"origin":{"carrier":{"kind":"reserved","module_id":"broca"},"schema_pin":pin}});
    assert!(
        serde_json::from_value::<subc_protocol::tool_call::ToolCallRequest>(missing_key).is_err()
    );
    call.origin = Some(
        subc_protocol::tool_call::CallOrigin::new(
            Principal::Reserved {
                module_id: "broca".into(),
            },
            "original",
        )
        .with_schema_pin(pin),
    );
    call.call_key = None;
    register(&ledger, &route, &call);
    assert!(
        !CALLS.lock().unwrap().contains_key(&key(&ledger)),
        "origin-only calls remain display-free without an outer replay row"
    );
    assert!(publications().is_empty());
}

#[test]
fn running_hold_without_a_person_wait_omits_held_ms() {
    let f = fixture("bash", "display-running-held");
    let _active = enter(Some(&f.key));
    let mut display = current_task().unwrap();
    let mut running = task();
    running.status = BgTaskStatus::Running;
    bash_reply(&mut display, bash_bodies(&running, Some(0), None), true);
    reply(&f.key, true, false, Some("running"));
    terminal(&f, &mut display, BgTaskStatus::Completed, Some(0));
    let writes = publications();
    let completed = writes
        .iter()
        .find(|write| write.kind == DisplayKind::Completion)
        .unwrap();
    assert!(completed.body.get("heldMs").is_none());
}

#[test]
fn killed_without_a_confirmed_provider_stop_has_no_completion() {
    let f = fixture("bash", "display-kill-without-proof");
    held(&f);
    let _active = enter(Some(&f.key));
    let mut display = current_task().unwrap();
    let mut metadata = task();
    metadata.status = BgTaskStatus::Running;
    bash_reply(&mut display, bash_bodies(&metadata, Some(0), None), true);
    reply(&f.key, false, false, None);
    metadata.status = BgTaskStatus::Killed;
    metadata.group_kill_sent = false;
    bash_terminal(
        &display,
        bash_bodies(&metadata, Some(0), None),
        "bash-demo1",
        Some(10),
    );
    assert_eq!(publications().len(), 1);
}

#[test]
fn person_wait_ends_at_approval_before_the_executor_queue() {
    let f = fixture("edit", "display-person-wait");
    held(&f);
    person_answered(&f.key);
    let person_ms = f.capture.lock().unwrap().held_ms.unwrap();
    assert!(person_ms >= 2000);
    assert!(f.capture.lock().unwrap().held_at.is_none());
    let _active = enter(Some(&f.key));
    assert_eq!(f.capture.lock().unwrap().held_ms, Some(person_ms));
    reply(&f.key, true, false, Some("consent"));
    f.capture.lock().unwrap().held_at = Some(Instant::now() - Duration::from_secs(1));
    person_answered(&f.key);
    assert!(f.capture.lock().unwrap().held_ms.unwrap() >= person_ms + 1000);
    assert_eq!(
        publications().len(),
        1,
        "additional person waits must not replace the record"
    );
}

#[test]
fn handler_duration_is_frozen_before_reply_queueing() {
    let f = fixture("edit", "display-handler-duration");
    let _active = enter(Some(&f.key));
    f.capture.lock().unwrap().began = Some(Instant::now() - Duration::from_millis(12));
    handler_ready(Some(&f.key));
    let processing = f.capture.lock().unwrap().processing_ms;
    assert!(processing >= 12);
    assert!(f.capture.lock().unwrap().began.is_none());
    reply(&f.key, false, false, None);
    assert_eq!(publications()[0].body["durationMs"], processing);
}

#[test]
fn error_records_keep_valid_reason_codes_without_copying_error_text() {
    for (name, reason, expected) in [
        ("display-error-code", "syntax_error", "syntax_error"),
        ("display-error-text", "private error text", "failed"),
    ] {
        let f = fixture("edit", name);
        reply(&f.key, false, true, Some(reason));
        let writes = publications();
        assert_eq!(writes[0].body["reason"], expected);
        assert!(!serde_json::to_string(&writes[0].body)
            .unwrap()
            .contains("private error text"));
    }
}
