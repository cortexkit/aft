use super::*;
#[cfg(test)]
use aft_engine::test_bridge::bash_remote::BgTaskRegistryTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::bash_remote::ScheduledTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::bash_remote::SchedulerTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::bash_remote::SchedulerTestExt as _;
use aft_engine::test_bridge::bash_remote::{BgTaskTestExt as _, PersistedTaskTestExt as _};

fn notice_route(session: &str) -> CodeSearchNotice {
    CodeSearchNotice {
        address: Some(Address {
            module: "broca".into(),
            identity: BindIdentity::new("broca", session),
            scope: None,
            worker: true,
        }),
        call_key: "notice-call".into(),
        preset: "worker".into(),
        remaining: Arc::new(std::sync::Mutex::new(CODE_SEARCH_NOTICE_TIMEOUT)),
    }
}

#[test]
fn notice_templates_rewritten_bash_sends_once_without_footers() {
    install(PathBuf::from("unused-test-notice-connection"));
    for (command, id) in [
        ("cat fixture.txt", "aft.bash.prefer_tool"),
        ("grep -rn needle .", "aft.bash.code_search"),
    ] {
        let session = format!("notice-rewrite-{id}");
        let capture = test_endpoint::capture(&session, false);
        capture.delivery_order(Some(true));
        capture.cleared(true);
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("fixture.txt"), "needle\n").unwrap();
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.path().into()),
                experimental_bash_rewrite: true,
                ..Default::default()
            },
        );
        filetime::set_file_mtime(
            root.path(),
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - Duration::from_secs(120),
            ),
        )
        .unwrap();
        let response = crate::bash_rewrite::try_rewrite_for_request(
            command,
            "notice-call",
            Some(&session),
            &Default::default(),
            &ctx,
            &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        )
        .unwrap();
        assert_eq!(response.notices.items.len(), 1);
        let mut result = super::super::bash::finalized_bash_result(
            response,
            &ctx,
            &session,
            &crate::subc_format::FormatContext {
                runner_notice: Some(notice_route(&session)),
                ..Default::default()
            },
            false,
            None,
        );
        assert!(!result.text.contains("Prefer `"));
        assert!(!result.text.contains("DO NOT search code"));
        assert!(!serde_json::to_string(&result.response)
            .unwrap()
            .contains("aft.bash."));
        let sent = capture.receiver.try_recv().unwrap().request;
        assert_eq!(sent["params"]["template"]["id"], id);
        assert_eq!(
            sent["params"]["delivery"],
            json!(["turn_system_cleared", "tool_result"])
        );
        if id == "aft.bash.prefer_tool" {
            assert_eq!(
                sent["params"]["template"]["slots"],
                json!({"tool":"read", "shell":"bash"})
            );
        }
        crate::subc::runner_notices::notice_templates::deliver(
            &mut result.response,
            Some(&mut result.text),
        );
        assert!(capture.receiver.try_recv().is_err());
    }
}

fn shared_send_sample(name: &str) -> Vec<u8> {
    let body = std::fs::read(
        Path::new(crate::test_support::manifest_dir())
            .join("tests/fixtures/llm-runner")
            .join(name),
    )
    .unwrap();
    // The published bodies omit the transport envelope; preserve their bytes.
    [br#"{"result":"#.as_slice(), &body, b"}"].concat()
}

#[test]
fn runner_notices_shared_send_samples_have_pinned_bytes() {
    use sha2::{Digest, Sha256};
    let root = Path::new(crate::test_support::manifest_dir()).join("tests/fixtures/llm-runner");
    for (folder, digest, count) in [
        (
            "send-unknown-v2",
            "8991bba0732b9c511c84ed714d14ba0ceb8caae19d1b25baaf79336fdd6332d5",
            9,
        ),
        (
            "send-answers-early-v1",
            "5d6241426bf86020d36106fcf75a981d10d7f87376b50be5fa640250c06c8485",
            4,
        ),
        (
            "send-not-delivered-v2",
            "e770d026b020b1c0ae5eb935f00af7969c7fe135499e75e0467ee62b650c5815",
            8,
        ),
    ] {
        let root = root.join(folder);
        let manifest = std::fs::read(root.join("SHA256SUMS")).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&manifest)),
            digest,
            "{folder}"
        );
        let manifest = std::str::from_utf8(&manifest).unwrap();
        assert_eq!(manifest.lines().count(), count, "{folder}");
        for line in manifest.lines() {
            let (digest, name) = line.split_once("  ").unwrap();
            let bytes = std::fs::read(root.join(name)).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(bytes)),
                digest,
                "{folder}/{name}"
            );
        }
    }
}

#[test]
fn notice_templates_repeat_observations_send_typed_counts_without_appending() {
    install(PathBuf::from("unused-test-notice-connection"));
    for identical in [true, false] {
        let session = format!("notice-repeat-{identical}");
        let capture = test_endpoint::capture(&session, false);
        capture.delivery_order(Some(true));
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let args = json!({"path":"fixture"});
        let call = crate::response_finalize::repeat_breaker::RepeatCall::new("read", &args);
        let hash = crate::response_finalize::repeat_breaker::output_hash("unchanged");
        let now = std::time::Instant::now();
        for ago in [40, 20] {
            assert!(ctx
                .repeat_breaker()
                .observe_at(
                    &session,
                    &call,
                    if identical {
                        hash
                    } else {
                        hash.wrapping_add(ago)
                    },
                    now - Duration::from_secs(ago)
                )
                .is_none());
        }
        let mut response = Response::success("notice-call", json!({"text":"unchanged"}));
        response.notices.route = Some(notice_route(&session).into_sink());
        let mut text = "unchanged".to_owned();
        crate::run_tool_call::RepeatObservation::for_agent_call(
            &session, "read", &args, false, true, true,
        )
        .unwrap()
        .observe(&ctx, &mut response, &mut text);
        assert_eq!(text, "unchanged");
        let sent = capture.receiver.try_recv().unwrap().request;
        assert_eq!(
            sent["params"]["template"]["id"],
            if identical {
                "aft.repeat_call.identical"
            } else {
                "aft.repeat_call.drifting"
            }
        );
        assert_eq!(sent["params"]["template"]["slots"]["count"], 3);
        assert_eq!(sent["params"]["template"]["slots"]["span"], 40);
        assert_eq!(
            sent["params"]["template"]["slots"]["instruction"],
            crate::response_finalize::repeat_instruction(true, true, false)
        );
        assert_eq!(
            sent["params"]["delivery"],
            json!(["turn_system", "tool_result"])
        );
        assert!(capture.receiver.try_recv().is_err());
    }
}

#[test]
fn notice_templates_refusal_falls_back_once_at_user_authority() {
    install(PathBuf::from("unused-test-notice-connection"));
    use crate::subc::runner_notices::notice_templates::{self as notices, Notice, Target};
    for id in [
        notices::CODE_SEARCH,
        notices::PREFER_TOOL,
        notices::IDENTICAL,
        notices::DRIFTING,
    ] {
        let session = format!("notice-refusal-{id}");
        let capture = test_endpoint::capture(&session, false);
        capture.delivery_order(Some(true));
        capture.refuse("invalid_params", Some("template"));
        let slots = match id {
            notices::CODE_SEARCH => notices::code_search_slots(Some("search"), "bash"),
            notices::PREFER_TOOL => json!({"tool":"read", "shell":"bash"}),
            _ => {
                json!({"count":3, "span":42, "instruction":crate::response_finalize::repeat_instruction(true, true, false)})
            }
        };
        let mut response = Response::success("notice-call", json!({"text":"body"}));
        response.notices.route = Some(notice_route(&session).into_sink());
        response.notices.items.push(Notice::template(
            id,
            slots,
            "standalone-only text".into(),
            Target::Text,
        ));
        notices::deliver(&mut response, None);
        assert_eq!(response.data["text"], "body");
        let template = capture.receiver.try_recv().unwrap().request;
        let fallback = capture.receiver.try_recv().unwrap().request;
        assert_eq!(template["params"]["template"]["id"], id);
        assert!(template["params"].get("prompt").is_none());
        assert!(fallback["params"].get("template").is_none());
        assert_eq!(fallback["params"]["delivery"], json!(["tool_result"]));
        assert_ne!(fallback["params"]["send_id"], template["params"]["send_id"]);
        assert_eq!(
            fallback["params"]["dedupe_key"],
            template["params"]["dedupe_key"]
        );
        assert!(!fallback["params"]["prompt"]
            .as_str()
            .unwrap()
            .contains("standalone-only"));
        assert!(capture.receiver.try_recv().is_err());
    }
}

#[test]
fn notice_templates_unordered_runner_gets_no_nudge() {
    install(PathBuf::from("unused-test-notice-connection"));
    let capture = test_endpoint::capture("notice-legacy", false);
    capture.delivery_order(Some(false));
    let mut response = Response::success("notice-call", json!({"text":"body"}));
    response.notices.route = Some(notice_route("notice-legacy").into_sink());
    response.notices.items.push(
        crate::subc::runner_notices::notice_templates::Notice::template(
            crate::subc::runner_notices::notice_templates::PREFER_TOOL,
            json!({"tool":"read", "shell":"bash"}),
            "Prefer `read` tool over bash.".into(),
            crate::subc::runner_notices::notice_templates::Target::Text,
        ),
    );
    crate::subc::runner_notices::notice_templates::deliver(&mut response, None);
    // A plain send would wait for the turn to end and then start one of its
    // own, so a runner without ordered delivery gets nothing, and the result
    // is not appended to either.
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(response.data["text"], "body");
}

#[test]
fn notice_templates_fallback_and_later_notices_share_two_second_budget() {
    install(PathBuf::from("unused-test-notice-connection"));
    let capture = test_endpoint::capture("notice-shared-budget", false);
    capture.delivery_order(Some(true));
    capture.refuse("invalid_params", Some("template"));
    capture.delay_reply(Duration::from_millis(1200));
    let route = notice_route("notice-shared-budget");
    let notice = crate::subc::runner_notices::notice_templates::Notice::template(
        crate::subc::runner_notices::notice_templates::PREFER_TOOL,
        json!({"tool":"read", "shell":"bash"}),
        "Prefer `read` tool over bash.".into(),
        crate::subc::runner_notices::notice_templates::Target::Text,
    );
    let start = std::time::Instant::now();
    route.send(&notice);
    route.clone().send(
        &crate::subc::runner_notices::notice_templates::Notice::diagnostic("diagnostic".into()),
    );
    assert!((Duration::from_millis(1800)..Duration::from_secs(3)).contains(&start.elapsed()));
    assert_eq!(
        capture.receiver.try_recv().unwrap().request["params"]["template"]["id"],
        "aft.bash.prefer_tool"
    );
    assert_eq!(
        capture.receiver.try_recv().unwrap().request["params"]["delivery"],
        json!(["tool_result"])
    );
    assert!(
        capture.receiver.try_recv().is_err(),
        "no fresh deadline for later notices"
    );
    assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
}

#[test]
fn notice_templates_diagnostics_remain_free_text_tool_result_sends() {
    install(PathBuf::from("unused-test-notice-connection"));
    let session = "notice-diagnostics";
    let capture = test_endpoint::capture(session, false);
    capture.delivery_order(Some(true));
    capture.cleared(true);
    let root = tempfile::tempdir().unwrap();
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        Default::default(),
    );
    use crate::response_finalize::alert_render::{AlertDiagnostic, AlertEngine, AlertSeverity};
    let mut alerts = AlertEngine::default();
    alerts.observe_authoritative(session, root.path(), "rust-analyzer", vec![]);
    alerts.observe_authoritative(
        session,
        root.path(),
        "rust-analyzer",
        vec![AlertDiagnostic {
            file: "src/fixture.rs".into(),
            line: 2,
            column: 3,
            end_line: 2,
            end_column: 4,
            severity: AlertSeverity::Error,
            source: Some("rust-analyzer".into()),
            code: None,
            message: "expected fixture diagnostic".into(),
        }],
    );
    let mut response = Response::success("notice-call", json!({"text":"body"}));
    response.notices.route = Some(notice_route(session).into_sink());
    crate::response_finalize::finalize_response_for_dispatch_root(
        &mut response,
        &ctx,
        &mut alerts,
        session,
        root.path(),
        "read",
        false,
    );
    assert_eq!(response.data["text"], "body");
    let sent = capture.receiver.try_recv().unwrap().request;
    assert_eq!(sent["params"]["delivery"], json!(["tool_result"]));
    assert!(sent["params"].get("template").is_none());
    let prompt = sent["params"]["prompt"].as_str().unwrap();
    assert!(prompt.contains("src/fixture.rs"));
    assert!(prompt.contains("expected fixture diagnostic"));
    assert!(capture.receiver.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn code_search_nudge_plain_grep_on_runner_heads() {
    use super::super::{bash_selector_tests::call_v1_bash, test_support, tool_provider, BindTrust};
    use crate::executor::Executor;
    use subc_protocol::scope::{ScopeKind, ScopeStamp};

    install(PathBuf::from("unused-test-notice-connection"));
    for (module, raw_harness, harness) in [
        ("broca", "runner", "broca"),
        ("thalamus-harness", "opencode2", "opencode2"),
    ] {
        let session = format!("plain-grep-head-{module}");
        let capture = test_endpoint::capture(&session, false);
        capture.probe_incarnation("plain-grep-head-daemon", 1);
        capture.delivery_order(Some(true));
        capture.reply(SendReply::new("pending"));
        let (dir, root) = test_support::test_root("plain-grep-head");
        let src = root.as_path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("fixture.txt"), "foo\n").unwrap();
        filetime::set_file_mtime(&src, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let ctx = Arc::new(crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.as_path().into()),
                storage_dir: Some(dir.path().join("storage")),
                experimental_bash_rewrite: false,
                bash_permissions: false,
                ..Default::default()
            },
        ));
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx));
        let mut route = test_support::route_identity_with_harness(
            &root,
            &session,
            BindTrust::FirstParty,
            raw_harness,
        );
        let data = Arc::get_mut(&mut route.0).unwrap();
        data.role = tool_provider::RouteRole::ToolProviderV1;
        data.scope = Some(ScopeStamp::new(
            Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            session.clone(),
            1,
            ScopeKind::Head,
        ));
        let AuthenticatedPrincipal::RouteBind {
            harness: bound_harness,
            principal_id,
            ..
        } = &mut data.spawn_principal
        else {
            unreachable!();
        };
        *bound_harness = raw_harness.into();
        *principal_id = Some(format!("reserved:{module}"));

        let done = call_v1_bash(&executor, route, "grep -rn foo src/", Some("head")).await;
        let response = done.response_for_test();
        assert!(response.success, "{module}: {response:?}");
        assert_eq!(response.data["exit_code"], 0, "{module}: {response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        assert_eq!(
            response.data["output"],
            format!("src/fixture.txt:1:foo\n[task {task_id}]")
        );
        for _ in 0..2 {
            let described = capture
                .receiver
                .try_recv()
                .expect("runner capability probe");
            assert_eq!(described.request["method"], "role.describe");
            assert_eq!(described.harness, harness);
            assert_eq!(described.session, "aft:runner-capability-probe");
            assert_eq!(described.scope, None);
        }
        let sent = capture
            .receiver
            .try_recv()
            .expect("ordered code-search nudge");
        assert_eq!(sent.harness, harness);
        assert_eq!(sent.session, session);
        assert_eq!(sent.scope.unwrap().scope_ref, session);
        assert_eq!(sent.request["method"], "session.send");
        let params = &sent.request["params"];
        assert_eq!(
            params["send_id"],
            "aft:nudge:aft.bash.code_search:subc-41-7"
        );
        assert_eq!(params["delivery"], json!(["turn_system", "tool_result"]));
        assert_eq!(params["template"]["id"], "aft.bash.code_search");
        assert!(
            capture.receiver.try_recv().is_err(),
            "only one notice before the result"
        );
    }
}

#[test]
fn code_search_nudge_runner_send_is_once_and_leaves_result_bytes_unchanged() {
    install(PathBuf::from("unused-test-notice-connection"));
    let session = "code-search-nudge-runner";
    let capture = test_endpoint::capture(session, false);
    capture.delivery_order(Some(true));
    let notice = CodeSearchNotice {
        address: Some(Address {
            module: "broca".into(),
            identity: BindIdentity::new("broca", session),
            scope: None,
            worker: true,
        }),
        call_key: "call-17".into(),
        preset: "worker".into(),
        remaining: Arc::new(std::sync::Mutex::new(CODE_SEARCH_NOTICE_TIMEOUT)),
    };
    let context = crate::subc_format::BashCodeSearchHint {
        command: "echo marker; rg foo".into(),
        workdir: None,
        search_tool: Some("search".into()),
        grep_tool: Some("grep".into()),
        shell_tool: "bash".into(),
        runner_notice: Some(notice),
    };
    let root = tempfile::tempdir().unwrap();
    let response = crate::protocol::Response::success(
        "call-17",
        json!({
            "task_id": "bash-native", "status": "completed", "exit_code": 0, "output": "matches\n[task bash-native]"
        }),
    );
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().to_path_buf()),
            ..Default::default()
        },
    );
    let format_context = crate::subc_format::FormatContext::default();
    let expected = super::super::bash::finalized_bash_result(
        crate::protocol::Response::success("call-17", response.data.clone()),
        &ctx,
        session,
        &format_context,
        false,
        None,
    );
    let (result, logs) = crate::logging::capture_log_lines(|| {
        super::super::bash::finalized_bash_result(
            response,
            &ctx,
            session,
            &crate::subc_format::FormatContext {
                bash_code_search_hint: Some(context),
                ..format_context
            },
            false,
            None,
        )
    });
    assert_eq!(
        serde_json::to_vec(&result.response).unwrap(),
        serde_json::to_vec(&expected.response).unwrap()
    );
    assert_eq!(result.text, expected.text);
    let got = capture.receiver.try_recv().unwrap();
    assert!(
        capture.receiver.try_recv().is_err(),
        "exactly one send before returning the result"
    );
    assert_eq!(got.request["method"], "session.send");
    let params = &got.request["params"];
    assert_eq!(params["send_id"], "aft:nudge:aft.bash.code_search:call-17");
    assert_eq!(params["delivery"], json!(["turn_system", "tool_result"]));
    assert_eq!(params["dedupe_key"], "aft:nudge:aft.bash.code_search");
    assert_eq!(params["template"]["id"], "aft.bash.code_search");
    assert_eq!(
        params["template"]["slots"],
        json!({"shell":"bash", "redirect":"Use the `search` tool instead (it auto-routes concepts, identifiers, regex, and literals)."})
    );
    assert!(params.get("prompt").is_none());
    assert_eq!(logs, ["INFO runner code-search nudge call_key=call-17 send_id=aft:nudge:aft.bash.code_search:call-17 session=code-search-nudge-runner harness=broca probe=confirmed; delivered reason=runner confirmed delivery"]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!((now + 55_000..=now + 60_000).contains(&params["expires_at_ms"].as_u64().unwrap()));
}

#[test]
fn code_search_nudge_runner_deadline_preserves_result_bytes_and_drops_once() {
    install(PathBuf::from("unused-test-notice-connection"));
    let session = "code-search-nudge-deadline";
    let capture = test_endpoint::capture(session, false);
    capture.delivery_order(Some(true));
    capture.delay_reply(Duration::from_secs(5));
    let root = tempfile::tempdir().unwrap();
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().to_path_buf()),
            ..Default::default()
        },
    );
    let data = json!({
        "task_id": "bash-native", "status": "completed", "exit_code": 0,
        "output": "matches\n[task bash-native]"
    });
    let format_context = crate::subc_format::FormatContext::default();
    let expected = super::super::bash::finalized_bash_result(
        crate::protocol::Response::success("call-deadline", data.clone()),
        &ctx,
        session,
        &format_context,
        false,
        None,
    );
    let started = std::time::Instant::now();
    let (result, logs) = crate::logging::capture_log_lines(|| {
        super::super::bash::finalized_bash_result(
            crate::protocol::Response::success("call-deadline", data),
            &ctx,
            session,
            &crate::subc_format::FormatContext {
                bash_code_search_hint: Some(crate::subc_format::BashCodeSearchHint {
                    command: "rg foo".into(),
                    workdir: None,
                    search_tool: Some("search".into()),
                    grep_tool: Some("grep".into()),
                    shell_tool: "bash".into(),
                    runner_notice: Some(CodeSearchNotice {
                        address: Some(Address {
                            module: "broca".into(),
                            identity: BindIdentity::new("broca", session),
                            scope: None,
                            worker: true,
                        }),
                        call_key: "call-deadline".into(),
                        preset: "worker".into(),
                        remaining: Arc::new(std::sync::Mutex::new(CODE_SEARCH_NOTICE_TIMEOUT)),
                    }),
                }),
                ..format_context
            },
            false,
            None,
        )
    });
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1800),
        "deadline fired too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "slow runner held the tool result: {elapsed:?}"
    );
    assert_eq!(
        serde_json::to_vec(&result.response).unwrap(),
        serde_json::to_vec(&expected.response).unwrap()
    );
    assert_eq!(result.text, expected.text);
    assert!(!result.text.contains("DO NOT search code"));
    capture.receiver.try_recv().unwrap();
    assert!(
        capture.receiver.try_recv().is_err(),
        "no retry after timeout"
    );
    assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
    assert_eq!(logs, ["INFO runner code-search nudge call_key=call-deadline send_id=aft:nudge:aft.bash.code_search:call-deadline session=code-search-nudge-deadline harness=broca probe=confirmed; dropped class=Timeout code=none detail=code-search notice send exceeded 2s total deadline"]);
}

#[test]
fn code_search_nudge_runner_drops_refused_undelivered_unordered_and_missing_address() {
    install(PathBuf::from("unused-test-notice-connection"));
    let root = tempfile::tempdir().unwrap();
    // `sends` is whether the runner sees a send at all. Without confirmed
    // ordered delivery a nudge would start a turn of its own, so none is sent.
    for (case, missing, ordered, expected) in [
        (
            "pending",
            false,
            Some(true),
            "probe=confirmed; sent, ordered, awaiting delivery reason=eligible code search",
        ),
        (
            "refused",
            false,
            Some(true),
            "probe=confirmed; refused; dropped code=delivery_unsupported",
        ),
        (
            "not-delivered",
            false,
            Some(true),
            "probe=confirmed; not_delivered reason=handover_failed; dropped",
        ),
        (
            "expired",
            false,
            Some(true),
            "probe=confirmed; dropped reason=notice expired",
        ),
        (
            "older-runner",
            false,
            Some(false),
            "probe=not advertised; dropped class=NoOrderedDelivery",
        ),
        (
            "probe-failed",
            false,
            None,
            "probe=unavailable; dropped class=NoOrderedDelivery",
        ),
        (
            "no-address",
            true,
            Some(true),
            "probe=not completed; dropped class=Address",
        ),
    ] {
        let session = format!("code-search-nudge-{case}");
        let capture = test_endpoint::capture(&session, false);
        capture.delivery_order(ordered);
        match case {
            "pending" => capture.reply(SendReply::new("pending")),
            "refused" => capture.refuse("delivery_unsupported", None),
            "not-delivered" => capture.reply(undelivered("handover_failed")),
            "expired" => capture.reply(SendReply::new(states::EXPIRED)),
            _ => {}
        }
        let context = crate::subc_format::BashCodeSearchHint {
            command: "grep -rn foo src/".into(),
            workdir: None,
            search_tool: Some("aft_search".into()),
            grep_tool: Some("grep".into()),
            shell_tool: "shell".into(),
            runner_notice: Some(CodeSearchNotice {
                address: (!missing).then(|| Address {
                    module: "broca".into(),
                    identity: BindIdentity::new("broca", &session),
                    scope: None,
                    worker: true,
                }),
                call_key: format!("call-{case}"),
                preset: "worker".into(),
                remaining: Arc::new(std::sync::Mutex::new(CODE_SEARCH_NOTICE_TIMEOUT)),
            }),
        };
        let mut response = crate::protocol::Response::success(
            "call",
            json!({"task_id": "native", "status": "completed", "exit_code": 0, "output": "hit"}),
        );
        let before = serde_json::to_vec(&response).unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        filetime::set_file_mtime(&src, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let (_, logs) =
            crate::logging::capture_log_lines(|| context.deliver(&mut response, root.path()));
        assert_eq!(serde_json::to_vec(&response).unwrap(), before, "{case}");
        assert_eq!(logs.len(), 1, "{case}: {logs:?}");
        assert!(logs[0].contains(expected), "{case}: {logs:?}");
        let sends = !missing && ordered == Some(true);
        if sends {
            let got = capture.receiver.try_recv().unwrap();
            assert!(got.request["params"]["delivery"].is_array(), "{case}");
            let send_id = got.request["params"]["send_id"].as_str().unwrap();
            assert!(
                logs[0].contains(&format!(
                    "send_id={send_id} session={session} harness=broca"
                )),
                "{case}: {logs:?}"
            );
        }
        assert!(
            capture.receiver.try_recv().is_err(),
            "{case} never retries, sends unordered or appends"
        );
    }
}

#[test]
fn code_search_nudge_runner_logs_only_search_candidates_that_are_not_eligible() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("fixture.txt"), "foo\n").unwrap();
    let data = json!({"task_id": "native", "status": "completed", "exit_code": 0, "output": "foo"});
    for (command, reason) in [
        ("echo 'x | grep foo'", None),
        (
            "echo foo | grep foo",
            Some("search is not the first pipeline stage"),
        ),
        ("grep foo fixture.txt", Some("search targets a file")),
    ] {
        let mut response = Response::success("notice-call", data.clone());
        let context = crate::subc_format::BashCodeSearchHint {
            command: command.into(),
            workdir: None,
            search_tool: Some("aft_search".into()),
            grep_tool: Some("grep".into()),
            shell_tool: "bash".into(),
            runner_notice: Some(notice_route("ineligible-code-search")),
        };
        let (_, logs) =
            crate::logging::capture_log_lines(|| context.deliver(&mut response, root.path()));
        let expected: Vec<_> = reason.into_iter().map(|reason| format!("INFO runner code-search nudge call_key=notice-call session=ineligible-code-search harness=broca not eligible reason={reason}")).collect();
        assert_eq!(logs, expected, "{command}");
        assert_eq!(response.data, data, "{command}");
    }
}

#[test]
fn runner_notices_shared_send_samples_follow_until_receipt_or_not_delivered() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let samples = [
        (
            "send-answers-early-v1/01-opencode2-send-reply.json",
            "pending",
        ),
        ("send-answers-early-v1/02-pi-send-reply.json", "pending"),
        (
            "send-answers-early-v1/03-claude-code-send-reply.json",
            "pending",
        ),
        ("send-unknown-v2/01-head-send-reply.json", "pending"),
        (
            "send-unknown-v2/02-head-send-status-unknown.json",
            "accepted",
        ),
        (
            "send-unknown-v2/03-head-send-resend-same-id.json",
            "accepted",
        ),
        ("send-unknown-v2/04-steer-send-reply.json", "pending"),
        (
            "send-unknown-v2/05-steer-send-status-unknown.json",
            "accepted",
        ),
        (
            "send-unknown-v2/06-steer-send-resend-same-id.json",
            "accepted",
        ),
        (
            "send-unknown-v2/07-head-send-status-pending.json",
            "pending",
        ),
        (
            "send-unknown-v2/08-head-send-status-delivered.json",
            "accepted",
        ),
        (
            "send-not-delivered-v2/01-send-reply-pending.json",
            "pending",
        ),
        (
            "send-not-delivered-v2/02-send-status-not-delivered-handover-failed.json",
            "not_delivered",
        ),
        (
            "send-not-delivered-v2/03-send-resend-same-id-not-delivered.json",
            "not_delivered",
        ),
        (
            "send-not-delivered-v2/07-send-status-not-delivered-withdrawn.json",
            "not_delivered",
        ),
    ];
    let pending = read_reply::<SendReply>(&shared_send_sample(samples[0].0)).unwrap();
    let delivered = read_reply::<SendReply>(&shared_send_sample(
        "send-unknown-v2/08-head-send-status-delivered.json",
    ))
    .unwrap();
    let not_delivered = read_reply::<SendReply>(&shared_send_sample(
        "send-not-delivered-v2/02-send-status-not-delivered-handover-failed.json",
    ))
    .unwrap();
    for (index, (name, expected)) in samples.into_iter().enumerate() {
        let bytes = shared_send_sample(name);
        let input: Value = serde_json::from_slice(&bytes).unwrap();
        let reply: SendReply = read_reply(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&reply).unwrap().get("run_id"),
            input["result"].get("run_id"),
            "{name}"
        );
        let observed = match decode_send_reply(&bytes, true) {
            Outcome::Accepted => "accepted",
            Outcome::Pending => "pending",
            Outcome::NotDelivered(reason) => {
                assert_eq!(Some(reason.as_str()), input["result"]["reason"].as_str());
                "not_delivered"
            }
            other => panic!("{name}: unexpected outcome {other:?}"),
        };
        assert_eq!(observed, expected, "{name}");
        for (mode, (following, typed)) in [(false, true), (true, true), (true, false)]
            .into_iter()
            .enumerate()
        {
            let task = Completion::new_runner(
                &format!("bash-00000000001{index:03x}{mode:02x}"),
                0,
                "reserved:thalamus-harness",
                "thalamus-harness",
            );
            let capture = test_endpoint::capture(&task.task, false);
            capture.delivery_order(Some(true));
            capture.send_status(typed);
            if following {
                capture.reply(pending.clone());
            }
            capture.reply(reply.clone());
            let scheduler = Scheduler::default();
            let clock = Arc::new(AtomicU64::new(task.finished));
            task.offer(&scheduler, clock.clone());
            let mut due = std::time::Instant::now();
            let mut next = step(&scheduler, due);
            let first = capture.receiver.try_recv().unwrap();
            assert_eq!(first.request["method"], "session.send", "{name}");
            if following {
                assert!(matches!(next, Next::Retry(delay) if delay == Duration::from_secs(5)));
                assert!(!task.delivered(), "{name}");
                next = advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5));
                assert_sample_followup(&capture.receiver.try_recv().unwrap(), &first, typed, name);
            }
            if expected == "pending" {
                let delay = Duration::from_secs(if following { 30 } else { 5 });
                assert!(
                    matches!(next, Next::Retry(actual) if actual == delay),
                    "{name}"
                );
                assert!(
                    !task.delivered(),
                    "{name}: a run ID is not a delivery receipt"
                );
                let _scope =
                    crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
                let progress = task
                    .registry
                    .notice_metadata(&task.task)
                    .unwrap()
                    .runner_completion_delivery
                    .unwrap();
                assert_eq!(progress.generation, 1, "{name}");
                assert!(
                    matches!(progress.phase, RunnerCompletionPhase::Pending { .. }),
                    "{name}"
                );
                assert_eq!(
                    task.frozen().send_id,
                    first.request["params"]["send_id"].as_str().unwrap()
                );
                // An unresolved first reply must still observe a later failed hand-over.
                capture.reply(not_delivered.clone());
                next = advance_notice(&scheduler, &clock, &mut due, delay);
                assert_sample_followup(&capture.receiver.try_recv().unwrap(), &first, typed, name);
            }
            if expected != "accepted" {
                assert!(
                    matches!(next, Next::Retry(delay) if delay == Duration::from_millis(250)),
                    "{name}"
                );
                assert!(!task.delivered(), "{name}");
                assert_eq!(
                    task.frozen().send_id,
                    format!("aft:bash:{}:2", task.task),
                    "{name}"
                );
                capture.reply(delivered.clone());
                next = advance_notice(&scheduler, &clock, &mut due, Duration::from_millis(250));
                let resent = capture.receiver.try_recv().unwrap();
                let mut expected_request = first.request.clone();
                expected_request["params"]["send_id"] = json!(task.frozen().send_id);
                assert_eq!(
                    resent.request, expected_request,
                    "{name}: only send_id may change"
                );
            }
            assert!(matches!(next, Next::Done), "{name}");
            assert!(task.delivered(), "{name}");
            assert!(scheduler.entries.lock().unwrap().is_empty(), "{name}");
            assert!(
                capture.receiver.try_recv().is_err(),
                "{name}: no extra send"
            );
        }
        let task = Completion::new_runner(
            &format!("bash-00000000001{index:03x}03"),
            0,
            "reserved:thalamus-harness",
            "thalamus-harness",
        );
        let capture = test_endpoint::capture(&task.task, false);
        capture.delivery_order(Some(true));
        capture.reply(reply);
        let scheduler = Scheduler::default();
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            task.reminder(600_000),
            PathBuf::new(),
            false,
            || 0
        ));
        assert!(
            matches!(step(&scheduler, std::time::Instant::now()), Next::Done),
            "{name}: reminders must not follow or resend"
        );
        assert_eq!(capture.receiver.try_iter().count(), 1, "{name}");
        assert!(scheduler.entries.lock().unwrap().is_empty(), "{name}");
    }
}

fn assert_sample_followup(
    check: &test_endpoint::Received,
    first: &test_endpoint::Received,
    typed: bool,
    name: &str,
) {
    if typed {
        assert_eq!(
            check.request,
            json!({"method":"session.send_status", "params":{"send_id":first.request["params"]["send_id"]}}),
            "{name}"
        );
    } else {
        assert_eq!(
            check.bytes, first.bytes,
            "{name}: same-ID follow-up must preserve the request"
        );
    }
}

#[test]
fn runner_notices_active_first_reply_tracks_only_negotiated_receipts() {
    let active = read_reply::<SendReply>(&shared_send_sample(
        "send-unknown-v2/04-steer-send-reply.json",
    ))
    .unwrap();
    let delivered = read_reply::<SendReply>(&shared_send_sample(
        "send-unknown-v2/08-head-send-status-delivered.json",
    ))
    .unwrap();
    for (index, capability) in [None, Some(false), Some(true)].into_iter().enumerate() {
        for (mode, typed) in [true, false].into_iter().enumerate() {
            let task = Completion::new(&format!("bash-00000000002{index:03x}{mode:02x}"), 0);
            let capture = test_endpoint::capture(&task.task, false);
            capture.delivery_order(capability);
            capture.send_status(typed);
            capture.reply(active.clone());
            let scheduler = Scheduler::default();
            let clock = Arc::new(AtomicU64::new(task.finished));
            task.offer(&scheduler, clock.clone());
            let mut due = std::time::Instant::now();
            let next = step(&scheduler, due);
            let first = capture.receiver.try_recv().unwrap();
            // Even legacy requests have delivery modes; only the negotiated
            // capability promises receipts for unresolved active sends.
            assert!(first.request["params"]["delivery"].is_array());
            if capability == Some(true) {
                assert!(matches!(next, Next::Retry(delay) if delay == Duration::from_secs(5)));
                assert!(!task.delivered());
                if typed {
                    capture.reply(delivered.clone());
                    assert!(matches!(
                        advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5)),
                        Next::Done
                    ));
                } else {
                    // Broca's same-ID replay can remain active without a receipt.
                    capture.reply(active.clone());
                    assert!(
                        matches!(advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5)), Next::Retry(delay) if delay == Duration::from_secs(30))
                    );
                    assert!(!task.delivered());
                    assert_sample_followup(
                        &capture.receiver.try_recv().unwrap(),
                        &first,
                        false,
                        "legacy status fallback",
                    );
                    capture.reply(delivered.clone());
                    assert!(matches!(
                        advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(30)),
                        Next::Done
                    ));
                }
                assert_sample_followup(
                    &capture.receiver.try_recv().unwrap(),
                    &first,
                    typed,
                    "negotiated status transport",
                );
            } else {
                assert!(
                    matches!(next, Next::Done),
                    "plain active replies must not wait for a receipt"
                );
            }
            assert!(task.delivered());
            assert!(scheduler.entries.lock().unwrap().is_empty());
            assert!(
                capture.receiver.try_recv().is_err(),
                "no new send or extra status check"
            );
        }
    }
}

#[test]
fn runner_notices_optional_member_fallback_accepts_plain_active_reply() {
    let active = read_reply::<SendReply>(&shared_send_sample(
        "send-unknown-v2/04-steer-send-reply.json",
    ))
    .unwrap();
    for (index, field) in ["dedupe_key", "expires_at_ms"].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000002f000{index}"), 0);
        let capture = test_endpoint::capture(&task.task, false);
        capture.delivery_order(Some(true));
        capture.send_status(true);
        capture.refuse("invalid_params", Some(field));
        capture.reply(active.clone());
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let first = capture.receiver.try_recv().unwrap();
        assert!(first.request["params"]["dedupe_key"].is_string());
        assert_eq!(
            first.request["params"]["expires_at_ms"],
            task.finished + MAX_AGE_MS
        );
        let fallback = capture.receiver.try_recv().unwrap();
        let mut expected = first.request;
        expected["params"]
            .as_object_mut()
            .unwrap()
            .remove("dedupe_key");
        expected["params"]
            .as_object_mut()
            .unwrap()
            .remove("expires_at_ms");
        assert_eq!(fallback.request, expected);
        assert_eq!(envelope(&task.frozen()), fallback.bytes);
        let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
        assert!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .dedupe
        );
        assert!(!tracks_delivery_receipt(&task.frozen(), true));
        assert!(task.delivered());
        assert!(scheduler.entries.lock().unwrap().is_empty());
        assert!(
            capture.receiver.try_recv().is_err(),
            "plain fallback must not be followed or resent"
        );
    }
}

#[test]
fn runner_notices_completion_expiry_uses_finish_or_start_and_saturates() {
    let policy = crate::bash_background::persistence::RunnerNoticePolicy {
        dedupe: true,
        reminder_interval_ms: 600_000,
    };
    for (finished, expected) in [
        (Some(500), 500 + MAX_AGE_MS),
        (None, 100 + MAX_AGE_MS),
        (Some(u64::MAX - 1), u64::MAX),
    ] {
        let request = task_request(
            request("notice".into(), "completion".into(), true, true),
            "bash-1",
            100,
            finished,
            None,
            policy,
        );
        assert_eq!(
            request.delivery_order().unwrap().unwrap().expires_at_ms,
            Some(expected)
        );
    }
}

#[test]
fn runner_notices_expired_and_superseded_completions_settle_without_resend() {
    for (index, state) in ["expired", "superseded"].into_iter().enumerate() {
        for (mode, following) in [false, true].into_iter().enumerate() {
            let task = Completion::new(&format!("bash-00000000003{index:03x}{mode:02x}"), 0);
            let capture = test_endpoint::capture(&task.task, false);
            capture.delivery_order(Some(true));
            capture.send_status(true);
            if following {
                capture.reply(SendReply::new("pending"));
            }
            capture.reply(SendReply::new(state));
            let scheduler = Scheduler::default();
            let clock = Arc::new(AtomicU64::new(task.finished));
            task.offer(&scheduler, clock.clone());
            let mut due = std::time::Instant::now();
            let mut next = step(&scheduler, due);
            let first = capture.receiver.try_recv().unwrap();
            assert_eq!(
                first.request["params"]["expires_at_ms"],
                task.finished + MAX_AGE_MS
            );
            if following {
                assert!(matches!(next, Next::Retry(delay) if delay == Duration::from_secs(5)));
                assert!(!task.delivered());
                next = advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5));
                assert_sample_followup(&capture.receiver.try_recv().unwrap(), &first, true, state);
            }
            assert!(matches!(next, Next::Done), "{state}");
            assert!(task.delivered(), "{state}");
            assert_eq!(
                task.frozen().send_id,
                first.request["params"]["send_id"].as_str().unwrap()
            );
            let _scope =
                crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
            assert_eq!(
                task.registry
                    .notice_metadata(&task.task)
                    .unwrap()
                    .runner_completion_delivery
                    .map_or(1, |progress| progress.generation),
                1
            );
            assert!(scheduler.entries.lock().unwrap().is_empty());
            assert!(
                capture.receiver.try_recv().is_err(),
                "{state}: no new generation"
            );
        }
    }
}

#[test]
fn bus_runner_targeted_not_delivered_applies_before_ack_and_uses_new_id() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let task = Completion::new("bash-00000000000fb001", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5)));
    let first = capture.receiver.try_recv().unwrap();
    scheduler.attachment_changed(true);
    capture.reply(undelivered("handover_failed"));
    let mut receipts = scheduler.bus_wake(Some(&task.frozen().send_id));
    assert_eq!(receipts.len(), 1);
    assert!(matches!(
        receipts[0].try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let Next::Retry(delay) = step(&scheduler, std::time::Instant::now()) else {
        panic!("new generation remains scheduled")
    };
    assert_eq!(receipts[0].try_recv(), Ok(()));
    let status = capture.receiver.try_recv().unwrap();
    assert_eq!(status.request["method"], "session.send_status");
    assert_eq!(
        status.request["params"]["send_id"],
        first.request["params"]["send_id"]
    );
    assert_eq!(task.frozen().send_id, format!("aft:bash:{}:2", task.task));
    due = std::time::Instant::now();
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, delay),
        Next::Done
    ));
    let resent = capture.receiver.try_recv().unwrap();
    assert_ne!(
        resent.request["params"]["send_id"],
        first.request["params"]["send_id"]
    );
    assert!(task.delivered());
}

#[test]
fn bus_runner_unknown_or_absent_id_wakes_bounded_queue_and_detach_restores_schedule() {
    let first = Completion::new("bash-00000000000fb002", 0);
    let second = Completion::new("bash-00000000000fb003", 0);
    let a = test_endpoint::capture(&first.task, false);
    let b = test_endpoint::capture(&second.task, false);
    for capture in [&a, &b] {
        capture.send_status(true);
        capture.reply(SendReply::new("pending"));
    }
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(first.finished.max(second.finished)));
    first.offer(&scheduler, clock.clone());
    second.offer(&scheduler, clock.clone());
    let due = std::time::Instant::now();
    for _ in 0..2 {
        assert!(
            matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5))
        );
    }
    a.receiver.try_recv().unwrap();
    b.receiver.try_recv().unwrap();
    scheduler.attachment_changed(true);
    let mut targeted = scheduler.bus_wake(Some(&first.frozen().send_id));
    a.reply(SendReply::new("pending"));
    assert!(
        matches!(step(&scheduler, std::time::Instant::now()), Next::Retry(delay) if delay == Duration::from_secs(600))
    );
    assert_eq!(targeted[0].try_recv(), Ok(()));
    a.receiver.try_recv().unwrap();
    assert!(
        b.receiver.try_recv().is_err(),
        "known send id narrows the lookup"
    );
    scheduler.attachment_changed(false);
    a.reply(SendReply::new("pending"));
    b.reply(SendReply::new("pending"));
    let mut restored = Vec::new();
    for _ in 0..2 {
        let Next::Retry(delay) = step(&scheduler, std::time::Instant::now()) else {
            panic!("pending sends stay followed")
        };
        restored.push(delay.as_secs());
    }
    restored.sort_unstable();
    assert_eq!(
        restored,
        vec![30, 120],
        "detach restores each send's original follow schedule"
    );
    a.receiver.try_recv().unwrap();
    b.receiver.try_recv().unwrap();
    scheduler.attachment_changed(true);
    for id in [Some("unknown"), None] {
        let mut receipts = scheduler.bus_wake(id);
        assert_eq!(receipts.len(), 2);
        a.reply(SendReply::new("pending"));
        b.reply(SendReply::new("pending"));
        for _ in 0..2 {
            assert!(
                matches!(step(&scheduler, std::time::Instant::now()), Next::Retry(delay) if delay == Duration::from_secs(600))
            );
        }
        for receipt in &mut receipts {
            assert_eq!(receipt.try_recv(), Ok(()));
        }
        assert_eq!(
            a.receiver.try_recv().unwrap().request["method"],
            "session.send_status"
        );
        assert_eq!(
            b.receiver.try_recv().unwrap().request["method"],
            "session.send_status"
        );
    }
    assert_eq!(pending_delay(0), Duration::from_secs(5));
    assert_eq!(pending_delay(1), Duration::from_secs(30));
    assert_eq!(pending_delay(2), Duration::from_secs(120));
}
use cortexkit_role_llm_runner::{
    send::SendReply,
    wire::{read_reply, reply_body},
};

use crate::bash_background::TaskIdentityAuth;

fn owner(principal: &str, harness: &str) -> crate::bash_background::TaskIdentity {
    crate::bash_background::TaskIdentity {
        principal: Some(principal.into()),
        harness: harness.into(),
        session: "session-7".into(),
    }
}

struct Completion {
    _root: tempfile::TempDir,
    storage: tempfile::TempDir,
    registry: crate::bash_background::BgTaskRegistry,
    route: RouteIdentity,
    task: String,
    finished: u64,
}

impl Completion {
    fn new(task: &str, offers: usize) -> Self {
        Self::new_runner(task, offers, "reserved:broca", "broca")
    }

    fn new_runner(task: &str, offers: usize, principal: &str, raw: &str) -> Self {
        use crate::bash_background::persistence::{
            create_task_layout, write_task_at, PersistedTask,
        };
        let (root_dir, root) = super::super::test_support::test_root("runner-notices");
        let storage = tempfile::tempdir().unwrap();
        let mut route = super::super::test_support::route_identity_with_harness(
            &root,
            task,
            BindTrust::FirstParty,
            raw,
        );
        if let AuthenticatedPrincipal::RouteBind {
            principal_id,
            harness,
            ..
        } = &mut Arc::get_mut(&mut route.0).unwrap().spawn_principal
        {
            *principal_id = Some(principal.into());
            *harness = raw.into();
        }
        let _scope = crate::bash_background::task_identity_scope(Some(route.task_identity()));
        let paths = create_task_layout(storage.path(), task, task).unwrap();
        let mut metadata = PersistedTask::starting(
            task.into(),
            task.into(),
            "echo durable".into(),
            root.as_path().into(),
            Some(root.as_path().into()),
            None,
            true,
            false,
        );
        metadata.mark_terminal(
            crate::bash_background::BgTaskStatus::Completed,
            Some(0),
            None,
        );
        metadata.launching_raw_harness = Some(raw.into());
        metadata.runner_completion_offers = offers;
        let finished = metadata.finished_at.unwrap();
        write_task_at(&paths, &metadata).unwrap();
        let registry = crate::bash_background::BgTaskRegistry::default();
        registry.replay_session(storage.path(), task).unwrap();
        Self {
            _root: root_dir,
            storage,
            registry,
            route,
            task: task.into(),
            finished,
        }
    }

    fn frame(&self) -> PushFrame {
        let _scope = crate::bash_background::task_identity_scope(Some(self.route.task_identity()));
        self.registry
            .pending_completions_for_session(&self.task)
            .pop()
            .unwrap()
            .into_push_frame()
    }

    fn reminder(&self, elapsed_ms: u64) -> PushFrame {
        let mut frame = crate::protocol::BashLongRunningFrame::new(
            &self.task,
            &self.task,
            "echo durable",
            elapsed_ms,
        );
        frame.launching_identity = Some(self.route.task_identity());
        PushFrame::BashLongRunning(frame)
    }

    fn delivered(&self) -> bool {
        use crate::bash_background::persistence::{read_task_at, resolve_task};
        let paths = resolve_task(self.storage.path(), &self.task, &self.task).unwrap();
        read_task_at(&paths).unwrap().completion_delivered
    }

    fn frozen(&self) -> SendRequest {
        use crate::bash_background::persistence::{read_task_at, resolve_task};
        let _scope = crate::bash_background::task_identity_scope(Some(self.route.task_identity()));
        let paths = resolve_task(self.storage.path(), &self.task, &self.task).unwrap();
        let persisted = read_task_at(&paths)
            .unwrap()
            .runner_completion_notice
            .unwrap();
        let in_memory = self
            .registry
            .notice_metadata(&self.task)
            .unwrap()
            .runner_completion_notice
            .unwrap();
        assert_eq!(envelope(&persisted), envelope(&in_memory));
        persisted
    }

    fn offer(&self, scheduler: &Scheduler, now: Arc<AtomicU64>) {
        assert!(offer(
            scheduler,
            self.registry.clone(),
            self.frame(),
            PathBuf::new(),
            false,
            move || now.load(Ordering::Relaxed)
        ));
    }

    fn restart(&mut self) {
        self.registry.detach();
        self.registry = crate::bash_background::BgTaskRegistry::default();
        self.registry
            .replay_session(self.storage.path(), &self.task)
            .unwrap();
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.registry.detach();
    }
}

fn step(scheduler: &Scheduler, now: std::time::Instant) -> Next {
    let (id, mut work) = Scheduler::take(&mut scheduler.entries.lock().unwrap(), now).unwrap();
    let next = work();
    let result = match &next {
        Next::Done => Next::Done,
        Next::Retry(delay) => Next::Retry(*delay),
    };
    scheduler.finish(&id, work, next, now);
    result
}

fn run_stamp(worker: bool) -> ScopeStamp {
    let owner = json!({"kind":"reserved","module_id":"prefrontal"});
    let (scope_ref, attributes) = if worker {
        (
            "worker:session",
            json!({"on_behalf_of_agent":"agent-a","run_id":"run-a"}),
        )
    } else {
        (
            "head:session",
            json!({"agent_id":"agent-a","run_id":"run-a"}),
        )
    };
    serde_json::from_value(json!({
        "owner":owner, "ref":"run:program", "scope_epoch":1, "kind":"ephemeral",
        "attributes":attributes, "owner_authorized":true,
        "parent":{"owner":owner,"ref":scope_ref,"scope_epoch":7}, "parent_state":"linked",
    }))
    .unwrap()
}

#[test]
fn runner_notices_run_owned_task_sends_only_to_its_parent_session_scope() {
    use crate::bash_background::registry::with_scope_spawn;

    for (worker, name, expected_ref) in [
        (false, "bash-f58a000000000001", "head:session"),
        (true, "bash-f58a000000000002", "worker:session"),
    ] {
        let stamp = run_stamp(worker);
        let task = with_scope_spawn(Some(stamp.clone()), || Completion::new(name, 0));
        let capture = test_endpoint::capture(name, false);
        let scheduler = Scheduler::default();
        let mut watch = crate::protocol::BashPatternMatchFrame::new(
            name, name, "watch-a", "ready", 0, "", true,
        );
        watch.launching_identity = Some(task.route.task_identity());

        for (frame, id_prefix) in [
            (task.reminder(600_000), "aft:bash-reminder:"),
            (PushFrame::BashPatternMatch(watch), "aft:bash-watch:"),
            (task.frame(), "aft:bash:"),
        ] {
            let finished = task.finished;
            assert!(offer(
                &scheduler,
                task.registry.clone(),
                frame,
                PathBuf::new(),
                false,
                move || finished,
            ));
            assert!(matches!(step(&scheduler, Instant::now()), Next::Done));
            let notice = capture.receiver.try_recv().unwrap();
            assert_eq!(notice.session, name);
            assert_eq!(
                notice.scope,
                Some(ScopeSelector::new(
                    stamp.owner.clone(),
                    expected_ref,
                    Some(7)
                )),
                "{id_prefix} must never route under the run scope"
            );
            assert!(notice.request["params"]["send_id"]
                .as_str()
                .unwrap()
                .starts_with(id_prefix));
            let expected_delivery = if id_prefix == "aft:bash:" && !worker {
                json!(["tool_result", "steer", "idle_start"])
            } else {
                json!(["tool_result", "steer"])
            };
            assert_eq!(notice.request["params"]["delivery"], expected_delivery);
        }
        assert!(capture.receiver.try_recv().is_err());
        assert!(task.delivered());
        assert_eq!(
            task.registry.notice_metadata(name).unwrap().scope,
            Some(stamp),
            "notice delivery must not change the recorded lifecycle scope"
        );
    }
}

#[test]
fn runner_notices_run_owned_task_drops_notices_without_verified_parent() {
    use crate::bash_background::registry::with_scope_spawn;

    let linked = run_stamp(true);
    let mut missing = linked.clone();
    missing.parent = None;
    let mut pending = linked;
    pending.parent_state = Some(ParentState::Pending);
    for (name, stamp) in [
        ("bash-f58a000000000003", missing),
        ("bash-f58a000000000004", pending),
    ] {
        let task = with_scope_spawn(Some(stamp), || Completion::new(name, 0));
        let capture = test_endpoint::capture(name, false);
        let scheduler = Scheduler::default();
        for frame in [task.reminder(600_000), task.frame()] {
            let finished = task.finished;
            assert!(offer(
                &scheduler,
                task.registry.clone(),
                frame,
                PathBuf::new(),
                false,
                move || finished,
            ));
            assert!(matches!(step(&scheduler, Instant::now()), Next::Done));
            assert!(capture.receiver.try_recv().is_err(), "{name} sent a notice");
        }
    }
}

#[test]
fn code_search_nudge_on_a_run_uses_the_resolved_owner_scope() {
    use super::super::test_support::route_identity;
    use subc_protocol::call_mode::CallMode;

    let (_root_dir, root) = super::super::test_support::test_root("run-code-search-nudge");
    let session = "run-code-search-nudge";
    let mut owner = super::super::test_support::route_identity_with_harness(
        &root,
        session,
        BindTrust::FirstParty,
        "broca",
    );
    let stamp = run_stamp(false);
    let head = Arc::get_mut(&mut owner.0).unwrap();
    let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut head.spawn_principal else {
        panic!("head must have a runner bind");
    };
    *principal_id = Some("reserved:broca".into());
    let mut head_stamp = ScopeStamp::new(stamp.owner.clone(), "head:session", 7, ScopeKind::Head);
    head_stamp.owner_authorized = true;
    head_stamp.attributes.agent_id = Some("agent-a".into());
    head.scope = Some(head_stamp);
    head.call_mode = CallMode::Classic;

    let mut run = route_identity(&root, "program-session");
    let data = Arc::get_mut(&mut run.0).unwrap();
    data.call_mode = CallMode::Codemode;
    data.scope = Some(stamp.clone());
    let routes = HashMap::from([(route_key(1, 1), owner), (route_key(2, 1), run)]);
    let codemode::Admission::Owner(effective) = codemode::admit(
        &routes,
        route_key(2, 1),
        Some(&json!({"name":"bash"})),
        |_| Some(Ok(Default::default())),
    ) else {
        panic!("run must act for the head");
    };
    let address = code_search_notice(&effective, "call-a")
        .unwrap()
        .address
        .unwrap();
    assert_eq!(address.identity.session, session);
    assert_eq!(
        address.scope,
        Some(ScopeSelector::new(stamp.owner, "head:session", Some(7)))
    );
}

fn advance_notice(
    scheduler: &Scheduler,
    clock: &AtomicU64,
    due: &mut std::time::Instant,
    delay: Duration,
) -> Next {
    clock.fetch_add(delay.as_millis() as u64, Ordering::Relaxed);
    *due += delay;
    step(scheduler, *due)
}

fn undelivered(reason: &str) -> SendReply {
    SendReply::new("not_delivered").with_reason(reason)
}

// Serialize the tests that assert process-wide not_delivered counter deltas.
static DELIVERY_COUNTER_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn runner_notices_host_refused_retires_completion_without_new_generation() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    for (index, checking) in [false, true].into_iter().enumerate() {
        let mut task = Completion::new(&format!("bash-00000000000f002{index}"), 0);
        let capture = test_endpoint::capture(&task.task, false);
        if checking {
            capture.send_status(true);
            capture.reply(SendReply::new("pending"));
            capture.reply_fixture(include_bytes!(
                "../../tests/fixtures/runner-notices/send-status-host-refused.json"
            ));
        } else {
            capture.reply_fixture(include_bytes!(
                "../../tests/fixtures/runner-notices/send-host-refused.json"
            ));
        }
        let scheduler = Scheduler::default();
        let clock = Arc::new(AtomicU64::new(task.finished));
        task.offer(&scheduler, clock.clone());
        let mut due = std::time::Instant::now();
        let (next, logs) = if checking {
            assert!(
                matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5))
            );
            let first = capture.receiver.try_recv().unwrap();
            assert_eq!(first.request["method"], "session.send");
            assert_eq!(
                first.request["params"]["send_id"],
                format!("aft:bash:{}", task.task)
            );
            crate::logging::capture_log_lines(|| {
                advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5))
            })
        } else {
            crate::logging::capture_log_lines(|| step(&scheduler, due))
        };
        let last = capture.receiver.try_recv().unwrap();
        assert_eq!(
            last.request["method"],
            if checking {
                "session.send_status"
            } else {
                "session.send"
            }
        );
        assert_eq!(
            last.request["params"]["send_id"],
            format!("aft:bash:{}", task.task)
        );
        assert!(matches!(next, Next::Done), "host refusal must not retry");
        assert!(scheduler.entries.lock().unwrap().is_empty());
        assert!(
            capture.receiver.try_recv().is_err(),
            "must not send generation :2"
        );
        assert_eq!(capture.effects.load(Ordering::Relaxed), u64::from(checking));
        assert!(task.delivered());
        assert_eq!(task.frozen().send_id, format!("aft:bash:{}", task.task));
        assert_eq!(
            logs,
            vec![format!(
                "INFO runner notice task={} session={} not_delivered reason=host_refused; retired",
                task.task, task.task
            )]
        );
        task.restart();
        let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
        let delivery = task
            .registry
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_delivery
            .unwrap();
        assert_eq!(delivery.generation, 1);
        assert!(matches!(delivery.phase, RunnerCompletionPhase::Retired));
        assert!(task
            .registry
            .pending_completions_for_session(&task.task)
            .is_empty());
    }
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 2);
}

#[test]
fn runner_notices_not_delivered_completion_uses_new_generation_and_same_body() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    let task = Completion::new("bash-00000000000f0001", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.delivery_order(Some(true));
    capture.reply(undelivered("handover_failed"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, due) else {
        panic!("must retry")
    };
    assert_eq!(delay, Duration::from_millis(250));
    assert!(!task.delivered());
    let first = capture.receiver.try_recv().unwrap();
    assert_eq!(
        first.request["params"]["send_id"],
        format!("aft:bash:{}", task.task)
    );
    let frozen = task.frozen();
    assert_eq!(frozen.send_id, format!("aft:bash:{}:2", task.task));
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, delay),
        Next::Done
    ));
    let second = capture.receiver.try_recv().unwrap();
    assert_eq!(
        second.request["params"]["send_id"],
        format!("aft:bash:{}:2", task.task)
    );
    assert_ne!(
        first.request["params"]["send_id"],
        second.request["params"]["send_id"]
    );
    let mut expected = first.request;
    expected["params"]["send_id"] = json!(frozen.send_id);
    assert_eq!(expected, second.request);
    assert_eq!(
        second.request["params"]["dedupe_key"],
        format!("aft:bash-task:{}", task.task)
    );
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(task.delivered());
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 1);
}

#[test]
fn runner_notices_not_delivered_restart_replays_persisted_new_id() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let mut task = Completion::new("bash-00000000000f0002", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.reply(undelivered("routing_refused"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, due) else {
        panic!("must retry")
    };
    capture.receiver.try_recv().unwrap();
    let frozen = envelope(&task.frozen());
    drop(scheduler);
    task.restart();
    let scheduler = Scheduler::default();
    task.offer(&scheduler, clock.clone());
    due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(left) if left == delay));
    assert!(
        capture.receiver.try_recv().is_err(),
        "restart must respect persisted backoff"
    );
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, delay),
        Next::Done
    ));
    let replay = capture.receiver.try_recv().unwrap();
    assert_eq!(replay.bytes, frozen);
    assert_eq!(
        replay.request["params"]["send_id"],
        format!("aft:bash:{}:2", task.task)
    );
    assert!(capture.receiver.try_recv().is_err());
}

#[test]
fn runner_notices_not_delivered_cap_is_five_and_is_retired_durably() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    let mut task = Completion::new("bash-00000000000f0003", 0);
    let capture = test_endpoint::capture(&task.task, false);
    for _ in 0..5 {
        capture.reply(undelivered("handover_failed"));
    }
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    let mut logs = Vec::new();
    for generation in 1..=5 {
        let (next, lines) = crate::logging::capture_log_lines(|| step(&scheduler, due));
        logs.extend(lines);
        let call = capture.receiver.try_recv().unwrap();
        let expected = if generation == 1 {
            format!("aft:bash:{}", task.task)
        } else {
            format!("aft:bash:{}:{generation}", task.task)
        };
        assert_eq!(call.request["params"]["send_id"], expected);
        if generation < 5 {
            let Next::Retry(delay) = next else {
                panic!("retired too soon")
            };
            clock.fetch_add(delay.as_millis() as u64, Ordering::Relaxed);
            due += delay;
        } else {
            assert!(matches!(next, Next::Done));
        }
    }
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 5);
    assert_eq!(logs.iter().filter(|line| line.starts_with("ERROR ")).collect::<Vec<_>>(), vec![&format!("ERROR runner notice task={} session={} not_delivered after 5 generations reason=handover_failed", task.task, task.task)]);
    assert!(task.delivered());
    let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
    assert!(matches!(
        task.registry
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_delivery
            .unwrap()
            .phase,
        RunnerCompletionPhase::Retired
    ));
    task.restart();
    assert!(task
        .registry
        .pending_completions_for_session(&task.task)
        .is_empty());
}

#[test]
fn runner_notices_not_delivered_reminder_drops_once_without_resend() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    let task = Completion::new("bash-00000000000f0004", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.reply(undelivered("handover_failed"));
    let scheduler = Scheduler::default();
    assert!(offer(
        &scheduler,
        task.registry.clone(),
        task.reminder(600_000),
        PathBuf::new(),
        false,
        || 0
    ));
    let (next, logs) =
        crate::logging::capture_log_lines(|| step(&scheduler, std::time::Instant::now()));
    assert!(matches!(next, Next::Done));
    assert!(scheduler.entries.lock().unwrap().is_empty());
    assert_eq!(capture.receiver.try_iter().count(), 1);
    assert_eq!(
        logs,
        vec![format!(
            "INFO runner reminder task={} not_delivered reason=handover_failed; dropped",
            task.task
        )]
    );
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 1);
}

#[test]
fn runner_notices_pending_check_not_delivered_sends_exactly_one_new_generation() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    for (index, typed) in [true, false].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000000f001{index}"), 0);
        let capture = test_endpoint::capture(&task.task, false);
        capture.send_status(typed);
        capture.reply(SendReply::new("pending"));
        capture.reply(undelivered("handover_failed"));
        let scheduler = Scheduler::default();
        let clock = Arc::new(AtomicU64::new(task.finished));
        task.offer(&scheduler, clock.clone());
        let mut due = std::time::Instant::now();
        assert!(
            matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5))
        );
        let first = capture.receiver.try_recv().unwrap();
        assert!(
            capture.receiver.try_recv().is_err(),
            "pending initially sends only once"
        );
        assert!(!task.delivered());
        let Next::Retry(delay) =
            advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5))
        else {
            panic!("must retry new generation")
        };
        let check = capture.receiver.try_recv().unwrap();
        assert_eq!(
            check.request["params"]["send_id"],
            first.request["params"]["send_id"]
        );
        if typed {
            assert_eq!(check.request["method"], "session.send_status");
        } else {
            assert_eq!(check.bytes, first.bytes);
        }
        assert!(matches!(
            advance_notice(&scheduler, &clock, &mut due, delay),
            Next::Done
        ));
        assert_eq!(
            capture.receiver.try_recv().unwrap().request["params"]["send_id"],
            format!("aft:bash:{}:2", task.task)
        );
        assert!(capture.receiver.try_recv().is_err());
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_pending_stays_durable_until_24h_without_new_send() {
    let task = Completion::new("bash-00000000000f0020", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let before = PENDING_UNRESOLVED.load(Ordering::Relaxed);
    let mut due = std::time::Instant::now();
    let mut next = step(&scheduler, due);
    assert_eq!(
        capture.receiver.try_recv().unwrap().request["method"],
        "session.send"
    );
    let mut delays = Vec::new();
    let mut logs = Vec::new();
    while let Next::Retry(delay) = next {
        assert!(!task.delivered());
        delays.push(delay.as_secs());
        let (result, lines) = crate::logging::capture_log_lines(|| {
            advance_notice(&scheduler, &clock, &mut due, delay)
        });
        next = result;
        logs.extend(lines);
    }
    assert_eq!(&delays[..4], &[5, 30, 120, 600]);
    assert_eq!(clock.load(Ordering::Relaxed), task.finished + MAX_AGE_MS);
    let calls: Vec<_> = capture.receiver.try_iter().collect();
    assert_eq!(calls.len(), 146);
    assert!(calls
        .iter()
        .all(|call| call.request["method"] == "session.send_status"
            && call.request["params"]["send_id"] == format!("aft:bash:{}", task.task)));
    assert_eq!(PENDING_UNRESOLVED.load(Ordering::Relaxed) - before, 1);
    assert_eq!(
        logs,
        vec![format!(
            "INFO runner notice task={} session={} send still pending after 24 hours",
            task.task, task.task
        )]
    );
    assert!(task.delivered());
}

#[test]
fn runner_notices_pending_restart_keeps_next_check_and_never_resends() {
    let mut task = Completion::new("bash-00000000000f0021", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    capture.reply(SendReply::new("finished"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5)));
    capture.receiver.try_recv().unwrap();
    drop(scheduler);
    task.restart();
    let scheduler = Scheduler::default();
    task.offer(&scheduler, clock.clone());
    due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5)));
    assert!(capture.receiver.try_recv().is_err());
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5)),
        Next::Done
    ));
    assert_eq!(
        capture.receiver.try_recv().unwrap().request,
        json!({"method":"session.send_status", "params":{"send_id":format!("aft:bash:{}", task.task)}})
    );
    assert!(capture.receiver.try_recv().is_err());
    assert!(task.delivered());
}

#[test]
fn runner_notices_pending_status_failures_retry_once_then_resume_schedule() {
    let task = Completion::new("bash-00000000000f0022", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    step(&scheduler, due);
    capture.receiver.try_recv().unwrap();
    capture.refuse("invalid_params", None);
    capture.retry(1);
    assert!(
        matches!(advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5)), Next::Retry(delay) if delay == Duration::from_millis(250))
    );
    assert!(
        matches!(advance_notice(&scheduler, &clock, &mut due, Duration::from_millis(250)), Next::Retry(delay) if delay == Duration::from_secs(30))
    );
    assert!(!task.delivered());
    capture.reply(SendReply::new("finished"));
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(30)),
        Next::Done
    ));
    let calls: Vec<_> = capture.receiver.try_iter().collect();
    assert_eq!(calls.len(), 3);
    assert!(calls
        .iter()
        .all(|call| call.request["method"] == "session.send_status"));
    assert!(task.delivered());
}

#[test]
fn runner_notices_pending_followup_stops_only_on_receipt_or_contract_final_state() {
    for state in ["finished", "superseded", "expired", "template_part_gone"] {
        assert!(matches!(
            decode_send_reply(&reply_body(&SendReply::new(state)).unwrap(), true),
            Outcome::Accepted
        ));
    }
    for state in ["pending", "active", "paused", "future_state"] {
        assert!(matches!(
            decode_send_reply(&reply_body(&SendReply::new(state)).unwrap(), true),
            Outcome::Pending
        ));
    }
    let reply = SendReply::new("active")
        .with_delivered(cortexkit_role_llm_runner::send::Delivered::new("step").with_ref("row-1"));
    assert!(matches!(
        decode_send_reply(&reply_body(&reply).unwrap(), true),
        Outcome::Accepted
    ));
}

fn wait_for(scheduler: &Scheduler, task: &str, finished: bool) {
    let id = format!("aft:bash:{task}");
    wait_for_id(scheduler, &id, finished);
}

fn wait_for_id(scheduler: &Scheduler, id: &str, finished: bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut entries = scheduler.entries.lock().unwrap();
    loop {
        if if finished {
            !entries.contains_key(id)
        } else {
            entries
                .get(id)
                .is_some_and(|entry| entry.retrying && entry.work.is_some())
        } {
            return;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "notice did not reach scheduled state: {id}"
        );
        entries = scheduler.wake().wait_timeout(entries, left).unwrap().0;
    }
}

#[test]
fn runner_notices_completion_survives_long_outage_and_acks_only_acceptance() {
    let task = Completion::new("bash-00000000000b0001", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.retry(30);
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let start = std::time::Instant::now();
    let mut due = start;
    for attempt in 0..30 {
        let Next::Retry(delay) = step(&scheduler, due) else {
            panic!("retired attempt {attempt}")
        };
        assert!(delay <= Duration::from_secs(60));
        if attempt >= 8 {
            assert_eq!(delay, Duration::from_secs(60));
        }
        assert!(!task.delivered(), "retry must not ack attempt {attempt}");
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        due += delay;
        clock.fetch_add(delay.as_millis() as u64, Ordering::Relaxed);
    }
    assert!(due.duration_since(start) > Duration::from_secs(20 * 60));

    // The runner has recovered, but the notice is not runnable until its
    // scheduled retry. That retry must send and ack without another backoff.
    assert!(Scheduler::take(
        &mut scheduler.entries.lock().unwrap(),
        due - Duration::from_millis(1),
    )
    .is_none());
    assert_eq!(capture.receiver.try_iter().count(), 30);
    assert!(matches!(step(&scheduler, due), Next::Done));
    assert!(scheduler.entries.lock().unwrap().is_empty());
    assert!(task.delivered());
    assert_eq!(capture.receiver.try_iter().count(), 1);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
}

#[test]
fn runner_notices_completion_expires_from_finish_even_with_exhausted_old_budget() {
    let task = Completion::new("bash-00000000000b0002", 100);
    let capture = test_endpoint::capture(&task.task, false);
    let scheduler = Scheduler::default();
    let before = EXPIRED.load(Ordering::Relaxed);
    task.offer(
        &scheduler,
        Arc::new(AtomicU64::new(task.finished + MAX_AGE_MS)),
    );
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert!(task.delivered());
    assert!(EXPIRED.load(Ordering::Relaxed) > before);
    assert!(
        capture.receiver.try_recv().is_err(),
        "expired task must not be sent"
    );
}

#[test]
fn runner_notices_restart_offers_pending_completion_with_high_persisted_count() {
    let task = Completion::new("bash-00000000000b0003", 100);
    task.registry.detach();
    let restarted = crate::bash_background::BgTaskRegistry::default();
    restarted
        .replay_session(task.storage.path(), &task.task)
        .unwrap();
    let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
    let frame = restarted
        .pending_completions_for_session(&task.task)
        .pop()
        .unwrap()
        .into_push_frame();
    let capture = test_endpoint::capture(&task.task, false);
    let scheduler = Scheduler::default();
    let now = task.finished;
    assert!(offer(
        &scheduler,
        restarted.clone(),
        frame,
        PathBuf::new(),
        false,
        move || now
    ));
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(capture.receiver.try_iter().count(), 1);
    assert!(task.delivered());
    assert_eq!(
        restarted
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_offers,
        101
    );
    restarted.detach();
}

#[test]
fn runner_notices_route_bind_expedites_retry_without_parallel_send() {
    let task = Completion::new("bash-00000000000b0004", 100);
    let capture = test_endpoint::capture(&task.task, false);
    capture.retry(1);
    assert!(enqueue(task.registry.clone(), task.frame()));
    let scheduler = scheduler().unwrap();
    wait_for(scheduler, &task.task, false);
    let first = capture.receiver.try_recv().unwrap();
    let id = format!("aft:bash:{}", task.task);
    assert!(
        scheduler.entries.lock().unwrap()[&id].due()
            > std::time::Instant::now() + Duration::from_secs(50)
    );
    let (entered, release) = capture.block_next();
    route_bound(task.registry.clone(), &task.route);
    entered.recv_timeout(Duration::from_millis(500)).unwrap();
    for _ in 0..20 {
        route_bound(task.registry.clone(), &task.route);
    }
    assert!(capture.receiver.try_recv().is_ok());
    assert!(capture.receiver.try_recv().is_err());
    release.send(()).unwrap();
    wait_for(scheduler, &task.task, true);
    assert!(task.delivered());
    assert_eq!(capture.max_active(), 1);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert_eq!(first.request["params"]["send_id"], id);
    assert!(capture.receiver.try_recv().is_err());
}

#[test]
fn runner_notices_waiting_retries_do_not_occupy_workers() {
    let scheduler = scheduler().unwrap();
    let mut waiting = Vec::new();
    for n in 0..20 {
        let task = Completion::new(&format!("bash-{:016x}", 0xc0000 + n), 100);
        let capture = test_endpoint::capture(&task.task, false);
        capture.retry(u64::MAX);
        assert!(enqueue(task.registry.clone(), task.frame()));
        wait_for(scheduler, &task.task, false);
        waiting.push((task, capture));
    }
    let task = Completion::new("bash-00000000000b0005", 0);
    let capture = test_endpoint::capture(&task.task, false);
    assert!(enqueue(task.registry.clone(), task.frame()));
    capture
        .receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap();
    wait_for(scheduler, &task.task, true);
    assert!(task.delivered());
    assert!(health().0 >= 20);
    for (task, _) in waiting {
        assert!(!task.delivered());
        scheduler
            .entries
            .lock()
            .unwrap()
            .remove(&format!("aft:bash:{}", task.task));
    }
}

#[test]
fn runner_notices_bounded_queue_defers_without_ack() {
    let scheduler = Scheduler::default();
    for n in 0..QUEUE_CAPACITY {
        assert!(scheduler.offer(n.to_string(), false, Box::new(|| Next::Done), false));
    }
    let task = Completion::new("bash-00000000000b0006", 0);
    let capture = test_endpoint::capture(&task.task, false);
    assert!(!offer(
        &scheduler,
        task.registry.clone(),
        task.frame(),
        PathBuf::new(),
        false,
        move || 0
    ));
    assert!(!task.delivered());
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(scheduler.entries.lock().unwrap().len(), QUEUE_CAPACITY);
}

#[test]
fn runner_notices_startup_replay_offers_only_named_pending_runner_completions() {
    let task = Completion::new("bash-00000000000b0007", 100);
    let capture = test_endpoint::capture(&task.task, false);
    let replayed = vec![(
        task.task.clone(),
        vec!["bash-not-present".into(), task.task.clone()],
    )];
    let _scope = crate::bash_background::task_identity_scope(None);
    offer_replayed(task.registry.clone(), &[(task.task.clone(), vec![])]);
    assert!(capture.receiver.try_recv().is_err());
    offer_replayed(task.registry.clone(), &replayed);
    capture
        .receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap();
    wait_for(scheduler().unwrap(), &task.task, true);
    assert!(task.delivered());
    offer_replayed(task.registry.clone(), &replayed);
    assert!(capture.receiver.try_recv().is_err());
}

#[test]
fn runner_notices_consumed_terminal_result_suppresses_stale_completion_frame() {
    for (id, ack_before_offer) in [
        ("bash-00000000000b0008", true),
        ("bash-00000000000b0009", false),
    ] {
        let task = Completion::new(id, 0);
        let frame = task.frame();
        let capture = test_endpoint::capture(&task.task, false);
        let scheduler = Scheduler::default();
        let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
        if ack_before_offer {
            task.registry
                .ack_terminal_result_for_session(&task.task, &task.task);
        }
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            frame,
            PathBuf::new(),
            false,
            move || 0
        ));
        if !ack_before_offer {
            task.registry
                .ack_terminal_result_for_session(&task.task, &task.task);
        }
        assert!(task.delivered());
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        assert!(
            capture.receiver.try_recv().is_err(),
            "a consumed result must not be announced again"
        );
        assert_eq!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_completion_offers,
            0
        );
    }
}

struct HeldCompletion {
    task: Completion,
    db: Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    key: crate::db::call_ledger::Key,
}

impl HeldCompletion {
    fn new(id: &str) -> Self {
        crate::subc::lower_hooks::register();
        use crate::db::call_ledger as ledger;
        let task = Completion::new_runner(id, 0, "reserved:thalamus-harness", "opencode2");
        let db = Arc::new(std::sync::Mutex::new(
            crate::db::open(&task.storage.path().join("aft.db")).unwrap(),
        ));
        task.registry.set_harness(crate::harness::Harness::Runner);
        task.registry.set_db_pool(db.clone());
        let key = ledger::Key {
            carrier: "reserved:thalamus-harness".into(),
            call_key: format!("held-{id}"),
        };
        let registered = task.registry.test_task(id).unwrap();
        let mut state = registered.test_lock_state().unwrap();
        *state.metadata_mut().call_key_mut() = Some(crate::bash_background::TaskCallKey {
            requester: key.carrier.clone(),
            key: key.call_key.clone(),
            minted: false,
        });
        let paths =
            crate::bash_background::persistence::resolve_task(task.storage.path(), id, id).unwrap();
        crate::bash_background::persistence::write_task_at(&paths, state.metadata()).unwrap();
        std::fs::write(&paths.paths.stdout, "recovered held output\n").unwrap();
        let conn = db.lock().unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &conn,
            &state
                .metadata()
                .to_bash_task_row("runner", &paths.paths)
                .unwrap(),
        )
        .unwrap();
        let scope = ledger::ScopeIdentity {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: id.into(),
            scope_epoch: 1,
        };
        ledger::admit(
            &conn,
            &key,
            "held-command",
            Some(&scope),
            ledger::State::DispatchStarted,
        )
        .unwrap();
        ledger::link_task(&conn, &key, id).unwrap();
        ledger::hold_running(
            &conn,
            &key,
            id,
            br#"{"content":[],"held":{"reason":"running"}}"#,
            false,
        )
        .unwrap();
        drop(conn);
        drop(state);
        Self { task, db, key }
    }

    fn result(&self) -> serde_json::Value {
        let conn = self.db.lock().unwrap();
        let row = crate::db::call_ledger::get(&conn, &self.key)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, crate::db::call_ledger::State::Settled);
        let page: serde_json::Value = serde_json::from_slice(
            &crate::db::call_ledger::late_results(
                &conn,
                &self.key.carrier,
                "held-notice-test",
                None,
                None,
                crate::db::call_ledger::now_ms(),
                1_000_000,
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        let entries = page["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["call_key"], self.key.call_key);
        entries[0].clone()
    }

    fn offer(&self, scheduler: &Scheduler) {
        self.task
            .offer(scheduler, Arc::new(AtomicU64::new(self.task.finished)));
    }
}

#[test]
fn runner_notices_recovered_held_call_after_route_reopens_delivers_only_call_result() {
    let held = HeldCompletion::new("bash-00000000000b0101");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    // Retain an already-queued completion across the runner's route restart.
    held.offer(&scheduler);
    route_unbound(&held.task.route);
    let mut reopened = super::super::test_support::route_identity_with_harness(
        &held.task.route.root,
        &held.task.task,
        BindTrust::FirstParty,
        "opencode2",
    );
    Arc::get_mut(&mut reopened.0).unwrap().spawn_principal =
        held.task.route.spawn_principal.clone();
    route_bound(held.task.registry.clone(), &reopened);
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(held.result()["result"]["content"]
        .to_string()
        .contains("recovered held output"));
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    wait_for(super::scheduler().unwrap(), &held.task.task, true);
    assert!(
        capture.receiver.try_recv().is_err(),
        "the real result already reached the held call"
    );
}

#[test]
fn runner_notices_recovered_held_call_withdrawn_before_completion_keeps_notice() {
    let held = HeldCompletion::new("bash-00000000000b0102");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    {
        let conn = held.db.lock().unwrap();
        let row = crate::db::call_ledger::get(&conn, &held.key)
            .unwrap()
            .unwrap();
        let withdrawal = crate::db::call_ledger::withdraw(
            &conn,
            &held.key,
            row.scope.as_ref(),
            crate::db::call_ledger::now_ms(),
            1_000_000,
        )
        .unwrap();
        assert!(matches!(
            withdrawal,
            crate::db::call_ledger::Withdrawal::Answer {
                kill_task: Some(_),
                ..
            }
        ));
    }
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(
        capture.receiver.try_iter().count(),
        1,
        "withdrawn calls need their completion notice"
    );
}

#[test]
fn runner_notices_recovered_held_call_cancelled_before_completion_keeps_notice() {
    let held = HeldCompletion::new("bash-00000000000b0106");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    let cancelled = crate::db::call_ledger::RecordedFrame {
        ty: FrameType::Error,
        body: br#"{"code":"cancelled","message":"call cancelled by runner"}"#.to_vec(),
    };
    crate::db::call_ledger::settle(
        &held.db.lock().unwrap(),
        &held.key,
        &cancelled,
        "cancelled",
        None,
        crate::db::call_ledger::now_ms(),
    )
    .unwrap();
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(capture.receiver.try_iter().count(), 1);
}

#[test]
fn runner_notices_recovered_plain_background_task_keeps_notice() {
    let held = HeldCompletion::new("bash-00000000000b0103");
    let capture = test_endpoint::capture(&held.task.task, false);
    {
        let conn = held.db.lock().unwrap();
        // A background launch has already settled its call with task guidance,
        // not a command result or a retained foreground hold.
        let frame = crate::db::call_ledger::RecordedFrame {
            ty: FrameType::Response,
            body: br#"{"content":[{"type":"text","text":"Background task started"}]}"#.to_vec(),
        };
        crate::db::call_ledger::settle(
            &conn,
            &held.key,
            &frame,
            "ok",
            None,
            crate::db::call_ledger::now_ms(),
        )
        .unwrap();
    }
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(capture.receiver.try_iter().count(), 1);
}

#[test]
fn runner_notices_recovered_held_settlement_races_send_with_one_delivery() {
    let held = HeldCompletion::new("bash-00000000000b0104");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        let registry = held.task.registry.clone();
        let task = held.task.task.clone();
        let start = &start;
        threads.spawn(move || {
            start.wait();
            registry.settle_detached_call(&task);
        });
        start.wait();
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
    });
    assert!(held.result()["result"]["content"]
        .to_string()
        .contains("recovered held output"));
    assert_eq!(capture.receiver.try_iter().count(), 0);
}

#[test]
fn runner_notices_recovered_held_failed_settlement_releases_deferred_notice() {
    let held = HeldCompletion::new("bash-00000000000b0105");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    let conn = held.db.lock().unwrap();
    conn.execute_batch("CREATE TRIGGER fail_held_settlement BEFORE UPDATE OF state ON call_ledger WHEN NEW.state='Settled' BEGIN SELECT RAISE(ABORT, 'injected settlement failure'); END;").unwrap();
    std::thread::scope(|threads| {
        let sending = threads.spawn(|| step(&scheduler, std::time::Instant::now()));
        assert!(
            capture
                .receiver
                .recv_timeout(Duration::from_millis(30))
                .is_err(),
            "notice waits for the settlement decision"
        );
        drop(conn);
        assert!(matches!(sending.join().unwrap(), Next::Done));
    });
    assert_eq!(
        capture.receiver.try_iter().count(),
        1,
        "failed recovery must not swallow the only result delivery"
    );
    {
        let conn = held.db.lock().unwrap();
        let row = crate::db::call_ledger::get(&conn, &held.key)
            .unwrap()
            .unwrap();
        let retained: serde_json::Value = serde_json::from_slice(&row.frame.unwrap().body).unwrap();
        assert_eq!(
            retained["_aft_recovery_notice"], true,
            "startup must retain the fallback lane"
        );
        conn.execute_batch("DROP TRIGGER fail_held_settlement")
            .unwrap();
    }
    wait_for_id(
        super::scheduler().unwrap(),
        &format!("aft:bash-recovery:{}", held.task.task),
        true,
    );
    let result = held.result();
    assert_eq!(result["result"]["errorCode"], "outcome_unknown");
    assert_eq!(result["result"]["content"][0]["text"], "");
    assert!(
        capture.receiver.try_recv().is_err(),
        "ending the hold must not send a second notice"
    );
}

#[test]
fn runner_notices_recovered_notice_fallback_survives_restart_observation() {
    let held = HeldCompletion::new("bash-00000000000b0107");
    {
        let conn = held.db.lock().unwrap();
        conn.execute_batch("CREATE TRIGGER fail_fallback_end BEFORE UPDATE OF state ON call_ledger WHEN NEW.state='Settled' BEGIN SELECT RAISE(ABORT, 'injected fallback failure'); END;").unwrap();
        assert!(crate::db::call_ledger::settle_task_notice_fallback(
            &conn,
            &held.key,
            &held.task.task
        )
        .is_err());
        conn.execute_batch("DROP TRIGGER fail_fallback_end")
            .unwrap();
    }
    let reopened = crate::db::open(&held.task.storage.path().join("aft.db")).unwrap();
    let real = crate::db::call_ledger::RecordedFrame {
        ty: FrameType::Response,
        body: br#"{"content":[{"type":"text","text":"real output must stay in the notice"}]}"#
            .to_vec(),
    };
    crate::db::call_ledger::observe_task(
        &reopened,
        &held.key.carrier,
        &held.key.call_key,
        &held.task.task,
        Some(&real),
        "ok",
    )
    .unwrap();
    assert_eq!(held.result()["result"]["errorCode"], "outcome_unknown");
}

#[test]
fn runner_notices_distinguishes_queued_submission_from_delivered_notice() {
    assert!(matches!(
        decode_send_reply(
            br#"{"result":{"state":"pending","submission_id":"notice-1"}}"#,
            true
        ),
        Outcome::Pending
    ));
    assert!(matches!(
        decode_send_reply(
            br#"{"result":{"state":"active","delivered":{"as":"step","ref":"notice-1"}}}"#,
            true,
        ),
        Outcome::Accepted
    ));
}

#[test]
fn runner_notices_decode_open_states_and_error_envelopes() {
    assert!(matches!(
        decode_send_reply(br#"{"result":{"state":"pending"}}"#, true),
        Outcome::Pending
    ));
    for (reason, expected) in [
        (Some("handover_failed"), "handover_failed"),
        (Some("future_reason"), "future_reason"),
    ] {
        let mut reply = json!({"state":"not_delivered"});
        if let Some(reason) = reason {
            reply["reason"] = json!(reason);
        }
        let bytes = cortexkit_role_llm_runner::wire::reply_body(&reply).unwrap();
        assert!(
            matches!(decode_send_reply(&bytes, true), Outcome::NotDelivered(reason) if reason == expected)
        );
    }
    for state in ["active", "paused", "future_notice_state"] {
        let bytes = serde_json::to_vec(&json!({"result":{"state":state,"notice":true}})).unwrap();
        assert!(matches!(decode_send_reply(&bytes, true), Outcome::Pending));
        assert!(matches!(
            decode_send_reply(&bytes, false),
            Outcome::Accepted
        ));
    }
    assert!(
        matches!(decode_send_reply(br#"{"result":{"state":"not_delivered"}}"#, true), Outcome::Retry(failure) if failure.class == FailureClass::ReplyDecode)
    );
    for body in [
        json!({"error":{"code":"session_released","message":"gone"}}),
        json!({"code":"session_released","message":"gone"}),
    ] {
        assert!(
            matches!(decode_send_reply(&serde_json::to_vec(&body).unwrap(), true),
            Outcome::Final(failure) if failure.code.as_deref() == Some("session_released") && failure.detail.contains("gone"))
        );
    }
    assert!(
        matches!(decode_send_reply(br#"{"error":{"code":"module_warming","message":"starting"}}"#, true),
        Outcome::Retry(failure) if failure.code.as_deref() == Some("module_warming"))
    );
    assert!(
        matches!(decode_send_reply(br#"{"result":{"status":"pending"}}"#, true),
        Outcome::Retry(failure) if failure.class == FailureClass::ReplyDecode && failure.detail.contains("state"))
    );
}

#[test]
fn runner_alias_switch_preserves_task_ownership_and_persisted_keys() {
    let principal = |raw: &str| AuthenticatedPrincipal::RouteBind {
        trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
        route_channel: 1,
        route_epoch: 1,
        project_root: PathBuf::from("/project"),
        harness: raw.into(),
        session_id: "session-7".into(),
        principal_id: Some("reserved:broca".into()),
    };
    let canonical =
        crate::bash_background::TaskIdentity::from_principal(&principal("runner")).unwrap();
    for raw in ["runner", "opencode2", "broca"] {
        let identity =
            crate::bash_background::TaskIdentity::from_principal(&principal(raw)).unwrap();
        assert_eq!(identity, canonical);
        assert_eq!(
            super::super::tool_provider::ledger_carrier(&principal(raw)),
            super::super::tool_provider::ledger_carrier(&principal("runner"))
        );
        let saved: crate::bash_background::TaskIdentity = serde_json::from_value(
            json!({"principal":"reserved:broca", "harness":raw, "session":"session-7"}),
        )
        .unwrap();
        assert_eq!(saved, canonical);
        assert_eq!(
            serde_json::to_vec(&identity).unwrap(),
            serde_json::to_vec(&canonical).unwrap()
        );
    }
}

#[derive(Default)]
struct FakeRunner {
    calls: Vec<Value>,
    stored: HashMap<String, Vec<u8>>,
    notices: usize,
    lose_first_response: bool,
    refuse: Option<&'static str>,
}

impl FakeRunner {
    fn bind(
        &self,
        address: &Address,
        versions: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), &'static str> {
        if versions != &std::collections::BTreeMap::from([("llm-runner".into(), "v1".into())]) {
            return Err("role_version_unsupported");
        }
        if !matches!(address.module.as_str(), "thalamus-harness" | "broca") {
            return Err("module_not_found");
        }
        let identity = serde_json::to_value(&address.identity).unwrap();
        assert!(identity.get("project_root").is_none());
        Ok(())
    }

    fn call(&mut self, bytes: &[u8]) -> Outcome {
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert_eq!(body["method"], "session.send");
        let request: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
        request.check_prompt().unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap()["notice"], true);
        self.calls.push(body);
        if let Some(code) = self.refuse {
            return refusal(code);
        }
        let bytes = serde_json::to_vec(&request).unwrap();
        if let Some(first) = self.stored.get(&request.send_id) {
            if first != &bytes {
                return refusal("send_id_reuse");
            }
        } else {
            self.notices += 1;
            self.stored.insert(request.send_id.clone(), bytes);
        }
        if self.lose_first_response && self.calls.len() == 1 {
            return retry(FailureClass::TestEndpoint, "response lost");
        }
        let response = reply_body(&SendReply::new("active").with_delivered(
            cortexkit_role_llm_runner::send::Delivered::new("step").with_ref("send-7"),
        ))
        .unwrap();
        decode_send_reply(&response, true)
    }
}

#[test]
fn runner_notices_retry_unknown_outcome_with_same_id_and_dedupe() {
    let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
    let mut runner = FakeRunner {
        lose_first_response: true,
        ..Default::default()
    };
    let mut retry_logged = false;
    assert!(matches!(deliver(
        &request.send_id,
        0,
        true,
        runner.call(&envelope(&request)),
        &mut retry_logged,
    ), Next::Retry(delay) if delay == Duration::from_millis(250)));
    assert!(matches!(
        deliver(
            &request.send_id,
            1,
            true,
            runner.call(&envelope(&request)),
            &mut retry_logged
        ),
        Next::Done
    ));
    assert_eq!(runner.calls.len(), 2);
    assert_eq!(runner.notices, 1);
    assert_eq!(runner.calls[0], runner.calls[1]);
    assert!(retry_logged);
}

#[test]
fn runner_notices_session_unconverted_is_final_but_transient_retries() {
    let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
    for (code, final_refusal) in [("session_unconverted", true), ("transient", false)] {
        let mut runner = FakeRunner {
            refuse: Some(code),
            ..Default::default()
        };
        let outcome = runner.call(&envelope(&request));
        if final_refusal {
            assert!(
                matches!(&outcome, Outcome::Final(failure) if failure.code.as_deref() == Some(code))
            );
        } else {
            assert!(
                matches!(&outcome, Outcome::Retry(failure) if failure.code.as_deref() == Some(code))
            );
        }
        let next = deliver(&request.send_id, 0, true, outcome, &mut false);
        assert_eq!(matches!(next, Next::Done), final_refusal);
        if !final_refusal {
            assert!(matches!(next, Next::Retry(delay) if delay == Duration::from_millis(250)));
        }
        assert_eq!(runner.calls.len(), 1);
    }
}

#[test]
fn runner_notices_final_refusals_stop_without_retry() {
    for code in [
        "scope_owner_mismatch",
        "expired",
        "session_released",
        "unknown_session",
        "send_id_reuse",
        "delivery_unsupported",
        "invalid_params",
        "session_held_elsewhere",
    ] {
        let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
        let mut runner = FakeRunner {
            refuse: Some(code),
            ..Default::default()
        };
        assert!(matches!(
            deliver(
                &request.send_id,
                0,
                true,
                runner.call(&envelope(&request)),
                &mut false,
            ),
            Next::Done
        ));
        assert_eq!(runner.calls.len(), 1, "{code}");
    }
}

#[test]
fn runner_notices_transients_keep_short_retry_budget() {
    for attempt in 0..MAX_ATTEMPTS {
        let next = deliver(
            "aft:warning:7",
            attempt,
            false,
            retry(FailureClass::TestEndpoint, "offline"),
            &mut false,
        );
        assert_eq!(matches!(next, Next::Done), attempt + 1 == MAX_ATTEMPTS);
    }
}

#[test]
fn runner_notices_raw_harness_bind_back_and_worker_delivery() {
    let stamp = subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker-7",
        3,
        subc_protocol::scope::ScopeKind::Worker,
    );
    for (principal, raw, worker) in [
        ("reserved:thalamus-harness", "opencode2", false),
        ("reserved:thalamus-harness", "pi", false),
        ("reserved:broca", "broca", true),
    ] {
        let owner = owner(principal, "runner");
        let address = Address::new(&owner, raw.into(), None).unwrap();
        assert_eq!(address.identity.harness, raw);
        assert_eq!(address.identity.session, "session-7");
        assert_eq!(address.worker, worker);
        let versions = role_versions();
        assert_eq!(
            serde_json::to_value(target(&address)).unwrap(),
            json!({"kind":"management_surface", "module_id":address.module})
        );
        let runner = FakeRunner::default();
        runner.bind(&address, &versions).unwrap();
        assert!(runner
            .bind(
                &address,
                &std::collections::BTreeMap::from([("llm-runner".into(), "v2".into())])
            )
            .is_err());
        assert!(runner
            .bind(
                &address,
                &std::collections::BTreeMap::from([("tool-provider".into(), "v1".into())])
            )
            .is_err());
        let request = request("aft:bash:7".into(), "text".into(), true, address.worker);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value["delivery"],
            if worker {
                json!(["tool_result", "steer"])
            } else {
                json!(["tool_result", "steer", "idle_start"])
            }
        );
        let scoped = Address::new(&owner, raw.into(), Some(&stamp)).unwrap();
        assert!(scoped.worker);
        assert_eq!(
            scoped.scope.unwrap(),
            ScopeSelector::new(stamp.owner.clone(), "worker-7", Some(3))
        );
        // A head scope is never a worker, whichever runner carries it: Broca
        // hosts heads too, and their completions must be able to wake them.
        let head_stamp = subc_protocol::scope::ScopeStamp::new(
            stamp.owner.clone(),
            "head-7",
            3,
            subc_protocol::scope::ScopeKind::Head,
        );
        let head = Address::new(&owner, raw.into(), Some(&head_stamp)).unwrap();
        assert!(!head.worker, "{principal} head");
    }
    assert!(Address::new(&owner("reserved:thalamus-pi", "pi"), "pi".into(), None).is_none());
    assert!(!is_runner(&owner("direct", "opencode")));
    assert!(!is_runner(&owner("direct", "pi")));
}

#[test]
fn runner_notices_only_completions_on_heads_may_start_idle_runs() {
    for completion in [true, false] {
        for worker in [true, false] {
            let request = request("test".into(), "text".into(), completion, worker);
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(
                value["delivery"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("idle_start")),
                completion && !worker
            );
        }
    }
    assert!(read_reply::<SendReply>(br#"{"result":{"state":"pending"},"extra":true}"#).is_err());
    assert!(read_reply::<SendReply>(br#"{"state":"pending"}"#).is_err());
}

#[test]
fn runner_notices_delivery_refusal_decodes_field_from_wire_and_module_errors() {
    let request = request("aft:bash:field".into(), "completion".into(), true, false);
    for field in [json!("delivery"), json!("prompt"), Value::Null, json!(7)] {
        let error =
            json!({"code": "invalid_params", "message": "refused", "detail": {"field": field}});
        let outcomes = [
            decode_send_reply(&serde_json::to_vec(&error).unwrap(), true),
            decode_send_reply(&serde_json::to_vec(&json!({"error": error})).unwrap(), true),
            request_failure(CallError::Module(serde_json::from_value(error).unwrap())),
        ];
        for outcome in outcomes {
            assert_eq!(
                idle_start_fallback(&request, &outcome).is_some(),
                field == "delivery"
            );
        }
    }
}

#[test]
fn runner_notices_completion_delivery_skips_session_head() {
    for (id, principal, raw, delivery) in [
        (
            "bash-00000000000b0f01",
            "reserved:thalamus-harness",
            "opencode2",
            json!(["tool_result", "steer", "idle_start"]),
        ),
        (
            "bash-00000000000b0f02",
            "reserved:broca",
            "broca",
            json!(["tool_result", "steer"]),
        ),
    ] {
        let task = Completion::new_runner(id, 0, principal, raw);
        let capture = test_endpoint::capture(&task.task, false);
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let calls: Vec<_> = capture.receiver.try_iter().collect();
        assert!(!calls
            .iter()
            .any(|call| call.request["method"] == "session.head"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request["method"], "session.send");
        assert_eq!(calls[0].request["params"]["delivery"], delivery);
        assert_eq!(envelope(&task.frozen()), calls[0].bytes);
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_delivery_refusal_freezes_fallback_before_retry() {
    let task = Completion::new_runner(
        "bash-00000000000b0f03",
        0,
        "reserved:thalamus-harness",
        "pi",
    );
    let capture = test_endpoint::capture(&task.task, true);
    capture.refuse("invalid_params", Some("delivery"));
    let scheduler = Scheduler::default();
    let before = health().2;
    task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
    let now = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, now) else {
        panic!("lost fallback response must retry");
    };
    let first = capture.receiver.try_recv().unwrap();
    let second = capture.receiver.try_recv().unwrap();
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(
        first.request["params"]["delivery"],
        json!(["tool_result", "steer", "idle_start"])
    );
    let mut expected = first.request;
    expected["params"]["delivery"] = json!(["tool_result", "steer"]);
    assert_eq!(second.request, expected);
    assert_eq!(
        second.request["params"]["send_id"],
        format!("aft:bash:{}", task.task)
    );
    assert_eq!(second.request["params"]["notice"], true);
    assert_eq!(envelope(&task.frozen()), second.bytes);
    assert!(health().2 > before);
    assert!(!task.delivered());
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(matches!(step(&scheduler, now + delay), Next::Done));
    let third = capture.receiver.try_recv().unwrap();
    assert_eq!(third.bytes, second.bytes);
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(task.delivered());
}

#[test]
fn runner_notices_unrelated_refusals_do_not_remove_idle_start() {
    for (index, code, field) in [
        (0, "invalid_params", Some("prompt")),
        (1, "invalid_params", None),
        (2, "scope_owner_mismatch", None),
        (3, "delivery_unsupported", Some("delivery")),
    ] {
        let id = format!("bash-{:016x}", 0xb0f10 + index);
        let task = Completion::new_runner(&id, 0, "reserved:thalamus-harness", "pi");
        let capture = test_endpoint::capture(&task.task, false);
        capture.refuse(code, field);
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let first = capture.receiver.try_recv().unwrap();
        assert_eq!(
            first.request["params"]["delivery"],
            json!(["tool_result", "steer", "idle_start"])
        );
        assert_eq!(envelope(&task.frozen()), first.bytes);
        assert!(capture.receiver.try_recv().is_err(), "{code} {field:?}");
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_delivery_refusal_falls_back_only_once_and_never_for_workers() {
    for worker in [true, false] {
        let task = if worker {
            Completion::new("bash-00000000000b0f20", 0)
        } else {
            Completion::new_runner(
                "bash-00000000000b0f21",
                0,
                "reserved:thalamus-harness",
                "pi",
            )
        };
        let capture = test_endpoint::capture(&task.task, false);
        capture.refuse("invalid_params", Some("delivery"));
        capture.refuse("invalid_params", Some("delivery"));
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let calls: Vec<_> = capture.receiver.try_iter().collect();
        assert_eq!(calls.len(), if worker { 1 } else { 2 });
        let last = calls.last().unwrap();
        assert_eq!(
            last.request["params"]["delivery"],
            json!(["tool_result", "steer"])
        );
        assert_eq!(envelope(&task.frozen()), last.bytes);
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_address_never_uses_runner_alias() {
    let broca = owner("reserved:broca", "runner");
    assert_eq!(
        Address::new(&broca, "runner".into(), None)
            .unwrap()
            .identity
            .harness,
        "broca"
    );
    assert_eq!(
        Address::new(&broca, "opencode2".into(), None)
            .unwrap()
            .identity
            .harness,
        "opencode2"
    );
    let mut head = owner("reserved:thalamus-harness", "runner");
    head.session = "unknown-concrete-head".into();
    assert!(Address::new(&head, "runner".into(), None).is_none());
    for raw in ["opencode2", "pi", "claude-code"] {
        assert_eq!(
            Address::new(&head, raw.into(), None)
                .unwrap()
                .identity
                .harness,
            raw
        );
    }
}

#[test]
fn runner_notices_legacy_thalamus_waits_for_concrete_session_binding() {
    let mut task = Completion::new_runner(
        "bash-00000000000b0010",
        100,
        "reserved:thalamus-harness",
        "runner",
    );
    let capture = test_endpoint::capture(&task.task, false);
    assert!(enqueue(task.registry.clone(), task.frame()));
    wait_for(scheduler().unwrap(), &task.task, false);
    assert!(!task.delivered());
    assert_eq!(
        task.registry
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_offers,
        100
    );
    assert!(capture.receiver.try_recv().is_err());

    let route = Arc::get_mut(&mut task.route.0).unwrap();
    route.harness = "opencode2".into();
    if let AuthenticatedPrincipal::RouteBind { harness, .. } = &mut route.spawn_principal {
        *harness = "opencode2".into();
    }
    route_bound(task.registry.clone(), &task.route);
    let received = capture
        .receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap();
    wait_for(scheduler().unwrap(), &task.task, true);
    assert_eq!(received.harness, "opencode2");
    assert!(task.delivered());
    assert_eq!(
        Address::new(&task.route.task_identity(), "pi".into(), None)
            .unwrap()
            .identity
            .harness,
        "pi"
    );
    route_unbound(&task.route);
    assert!(
        Address::new(&task.route.task_identity(), "runner".into(), None).is_none(),
        "a closed route is not a harness source"
    );
}

#[test]
fn runner_notices_wire_params_have_no_model_or_plan() {
    for worker in [true, false] {
        let body: Value = serde_json::from_slice(&envelope(&request(
            "aft:bash:keys".into(),
            "completion".into(),
            true,
            worker,
        )))
        .unwrap();
        let keys: std::collections::BTreeSet<_> = body["params"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from(["prompt", "send_id", "notice", "delivery"])
        );
    }
}

#[test]
fn runner_notices_capability_probe_avoids_readers_plan_and_caches_before_route_open() {
    let session = "head-with-readers-capability-probe";
    let capture = test_endpoint::capture(session, false);
    capture.probe_incarnation("daemon-1", 1);
    capture.delivery_order(Some(true));
    capture.send_status(true);
    capture.cleared(true);
    let address = notice_route(session).address.unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("connection.json");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // The fake head plan permits AFT to send, but its readers exclude AFT.
        let consumer = CapabilityConsumer::connect(&path, &address).await.unwrap();
        let incarnation = consumer.incarnation(&address.module).await.unwrap();
        assert_eq!(
            consumer.describe(&address, &incarnation).await,
            Err(("describe", Some("read_access_refused".into())))
        );
        consumer.close().await;
        assert_eq!(capture.receiver.try_recv().unwrap().session, session);
        for capability in [
            "delivery_order",
            "send_status",
            "delivery_order_modes:turn_system_cleared",
        ] {
            assert_eq!(
                runner_capability(&path, &address, capability).await,
                Some(true)
            );
            let described = capture.receiver.try_recv().unwrap();
            assert_eq!(described.session, "aft:runner-capability-probe");
            assert_eq!(described.harness, "broca");
            assert_eq!(described.scope, None);
            assert_eq!(
                described.request,
                json!({"method":"role.describe", "params":{}})
            );
            let routes = capture.probe_routes();
            assert_eq!(
                runner_capability(&path, &address, capability).await,
                Some(true)
            );
            assert_eq!(
                capture.probe_routes(),
                routes,
                "cache hit must not open a route"
            );
            assert!(capture.receiver.try_recv().is_err());
        }
        assert_eq!(capture.probe_routes(), 4);
        let notice = request(
            format!("aft:bash:{session}"),
            "completion".into(),
            true,
            false,
        );
        assert!(matches!(
            send(&path, &address, &notice, true).await,
            Outcome::Accepted
        ));
        let sent = capture.receiver.try_recv().unwrap();
        assert_eq!(sent.session, session);
        assert_eq!(sent.request["method"], "session.send");
        assert!(matches!(
            check_send(&path, &address, &notice, true).await,
            Outcome::Accepted
        ));
        let checked = capture.receiver.try_recv().unwrap();
        assert_eq!(checked.session, session);
        assert_eq!(checked.request["method"], "session.send_status");
        assert_eq!(capture.probe_routes(), 4);
        capture.delivery_order(Some(false));
        assert_eq!(runner_delivery_order(&path, &address).await, Some(true));
        assert_eq!(capture.probe_routes(), 4);
        capture.probe_incarnation("daemon-1", 2);
        assert_eq!(runner_delivery_order(&path, &address).await, Some(false));
        assert_eq!(capture.probe_routes(), 5);
        capture.delivery_order(Some(true));
        capture.probe_incarnation("daemon-2", 2);
        assert_eq!(runner_delivery_order(&path, &address).await, Some(true));
        assert_eq!(capture.probe_routes(), 6);
    });
}

#[test]
fn runner_notices_capability_probe_failure_is_unknown_and_not_cached() {
    let session = "head-with-unavailable-capability-probe";
    let capture = test_endpoint::capture(session, false);
    capture.probe_incarnation("daemon-1", 1);
    let address = notice_route(session).address.unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("connection.json");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        assert_eq!(runner_delivery_order(&path, &address).await, None);
        assert_eq!(capture.probe_routes(), 1);
        capture.delivery_order(Some(true));
        assert_eq!(runner_delivery_order(&path, &address).await, Some(true));
        assert_eq!(capture.probe_routes(), 2);
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn runner_notices_delivery_order_capability_cache_is_incarnation_scoped() {
    let mut cache = RunnerCapabilities::default();
    let first = RunnerIncarnation {
        daemon: "daemon-1".into(),
        generation: 1,
    };
    let restarted = RunnerIncarnation {
        generation: 2,
        ..first.clone()
    };
    let daemon_restarted = RunnerIncarnation {
        daemon: "daemon-2".into(),
        generation: 1,
    };
    let path = Path::new("connection.json");
    assert_eq!(cache.get(path, "broca", &first, "delivery_order"), None);
    cache.insert(path, "broca", first.clone(), true, "delivery_order");
    assert_eq!(
        cache.get(path, "broca", &first, "delivery_order"),
        Some(true)
    );
    assert_eq!(
        cache.get(path, "thalamus-harness", &first, "delivery_order"),
        None
    );
    assert_eq!(
        cache.get(Path::new("other.json"), "broca", &first, "delivery_order"),
        None
    );
    assert_eq!(cache.get(path, "broca", &restarted, "delivery_order"), None);
    assert_eq!(
        cache.get(path, "broca", &daemon_restarted, "delivery_order"),
        None
    );
    cache.insert(path, "broca", restarted.clone(), false, "delivery_order");
    assert_eq!(
        cache.get(path, "broca", &restarted, "delivery_order"),
        Some(false)
    );
    assert_eq!(cache.get(path, "broca", &first, "delivery_order"), None);
    for (reply, expected) in [
        (
            json!({"capabilities":["steer","delivery_order"]}),
            Some(true),
        ),
        (json!({"capabilities":["steer"]}), Some(false)),
        (json!({"capabilities":"delivery_order"}), None),
        (json!({"capabilities":["delivery_order", 1]}), None),
        (json!({"error":{"code":"unavailable"}}), None),
        (json!({}), None),
    ] {
        assert_eq!(
            described_delivery_order(&cortexkit_role_llm_runner::wire::reply_body(&reply).unwrap()),
            expected
        );
    }
    assert_eq!(
        described_delivery_order(br#"{"capabilities":["delivery_order"]}"#),
        None
    );
}

#[test]
fn runner_notices_optional_members_are_gated_and_task_choice_is_frozen() {
    for (index, capability) in [None, Some(false), Some(true)].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000000e000{index}"), 0);
        let capture = test_endpoint::capture(&task.task, false);
        capture.delivery_order(capability);
        let scheduler = Scheduler::default();
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            task.reminder(600_321),
            PathBuf::new(),
            false,
            || 0
        ));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let reminder = capture.receiver.try_recv().unwrap();
        let metadata = task.registry.notice_metadata(&task.task).unwrap();
        let policy = metadata.runner_notice_policy.unwrap();
        assert_eq!(policy.dedupe, capability == Some(true));
        assert_eq!(policy.reminder_interval_ms, 600_000);
        assert_eq!(metadata.runner_reminder_elapsed_ms, Some(600_321));
        let mut expected = json!({"method":"session.send","params":{
            "prompt":super::super::bash_notice_text::long_running(&serde_json::to_value(match task.reminder(600_321) { PushFrame::BashLongRunning(frame) => frame, _ => unreachable!() }).unwrap()),
            "send_id":format!("aft:bash-reminder:{}:600321", task.task),
            "notice":true,"delivery":["tool_result","steer"]
        }});
        if capability == Some(true) {
            expected["params"]["dedupe_key"] = json!(format!("aft:bash-task:{}", task.task));
            expected["params"]["expires_at_ms"] = json!(metadata.started_at + 1_200_321);
        }
        assert_eq!(reminder.request, expected);
        // Later discovery and configuration cannot change an accepted send's identity.
        capture.delivery_order(if capability == Some(true) {
            None
        } else {
            Some(true)
        });
        task.registry.configure_long_running_reminders(true, 1_000);
        let registered = task.registry.test_task(&task.task).unwrap();
        let recent = std::time::Instant::now() - Duration::from_secs(10);
        *registered.last_reminder_at().lock().unwrap() = Some(recent);
        task.registry.maybe_emit_long_running_reminder(&registered);
        assert_eq!(
            *registered.last_reminder_at().lock().unwrap(),
            Some(recent),
            "the frozen interval must also govern actual scheduling"
        );
        let completion_text = super::super::bash_notice_text::completion(
            &serde_json::to_value(task.frame()).unwrap(),
        );
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let completion = capture.receiver.try_recv().unwrap();
        let mut expected = json!({"prompt":completion_text,"send_id":format!("aft:bash:{}", task.task),
            "notice":true,"delivery":["tool_result","steer"]});
        if capability == Some(true) {
            expected["dedupe_key"] = json!(format!("aft:bash-task:{}", task.task));
            expected["expires_at_ms"] = json!(task.finished + MAX_AGE_MS);
        }
        assert_eq!(
            completion.request,
            json!({"method":"session.send","params":expected})
        );
        assert_eq!(
            completion.request["params"].get("expires_at_ms").is_some(),
            capability == Some(true)
        );
        assert_eq!(
            completion.request["params"].get("dedupe_key"),
            reminder.request["params"].get("dedupe_key")
        );
        assert_eq!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .reminder_interval_ms,
            600_000
        );
    }
}

#[test]
fn runner_notices_reminder_retry_and_restart_replay_preserve_bytes() {
    let task = Completion::new("bash-00000000000e0010", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.delivery_order(Some(true));
    capture.retry(1);
    let scheduler = Scheduler::default();
    assert!(offer(
        &scheduler,
        task.registry.clone(),
        task.reminder(600_123),
        PathBuf::new(),
        false,
        || 0
    ));
    let now = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, now) else {
        panic!("must retry")
    };
    let first = capture.receiver.try_recv().unwrap();
    capture.delivery_order(None);
    task.registry.configure_long_running_reminders(true, 1);
    assert!(matches!(step(&scheduler, now + delay), Next::Done));
    let second = capture.receiver.try_recv().unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);

    task.registry.detach();
    let restarted = crate::bash_background::BgTaskRegistry::default();
    restarted
        .replay_session(task.storage.path(), &task.task)
        .unwrap();
    let metadata = restarted.notice_metadata(&task.task).unwrap();
    let id = format!(
        "aft:bash-reminder:{}:{}",
        task.task,
        metadata.runner_reminder_elapsed_ms.unwrap()
    );
    // The saved identity, not time since restart, supplies the replay's elapsed value.
    let elapsed = id.rsplit(':').next().unwrap().parse().unwrap();
    assert!(offer(
        &scheduler,
        restarted.clone(),
        task.reminder(elapsed),
        PathBuf::new(),
        false,
        || u64::MAX
    ));
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    let replay = capture.receiver.try_recv().unwrap();
    assert_eq!(first.bytes, replay.bytes);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    restarted.detach();
}

#[test]
fn runner_notices_member_refusal_drops_reminder_and_freezes_completion_fallback() {
    for (index, field) in ["dedupe_key", "expires_at_ms"].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000000e002{index}"), 0);
        let capture = test_endpoint::capture(&task.task, true);
        capture.delivery_order(Some(true));
        capture.refuse("invalid_params", Some(field));
        let scheduler = Scheduler::default();
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            task.reminder(600_000),
            PathBuf::new(),
            false,
            || 0
        ));
        let (next, logs) =
            crate::logging::capture_log_lines(|| step(&scheduler, std::time::Instant::now()));
        assert!(matches!(next, Next::Done));
        assert_eq!(logs, vec![format!("INFO runner reminder task={} send_id=aft:bash-reminder:{}:600000 dropped after optional member refusal", task.task, task.task)]);
        let reminder = capture.receiver.try_recv().unwrap();
        assert!(reminder.request["params"].get("dedupe_key").is_some());
        assert!(
            capture.receiver.try_recv().is_err(),
            "a refused reminder must not be resent"
        );
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        assert!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .dedupe
        );

        capture.refuse("invalid_params", Some(field));
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        let now = std::time::Instant::now();
        let (next, logs) = crate::logging::capture_log_lines(|| step(&scheduler, now));
        let Next::Retry(delay) = next else {
            panic!("lost resend response must retry")
        };
        assert_eq!(logs, vec![format!("INFO runner notice task={} send_id=aft:bash:{} session={} retrying without dedupe_key/expires_at_ms after invalid_params refusal", task.task, task.task, task.task)]);
        let first = capture.receiver.try_recv().unwrap();
        let resend = capture.receiver.try_recv().unwrap();
        let mut expected = first.request;
        expected["params"]
            .as_object_mut()
            .unwrap()
            .remove("dedupe_key");
        expected["params"]
            .as_object_mut()
            .unwrap()
            .remove("expires_at_ms");
        assert_eq!(resend.request, expected);
        assert_eq!(envelope(&task.frozen()), resend.bytes);
        assert!(capture.receiver.try_recv().is_err());
        assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
        assert!(matches!(step(&scheduler, now + delay), Next::Done));
        assert_eq!(capture.receiver.try_recv().unwrap().bytes, resend.bytes);
        assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
        assert!(task.delivered());
        assert!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .dedupe
        );
    }
}
