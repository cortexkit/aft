//! Captured tool-provider frames for the read-only `reserved:occipital` identity.

use super::*;
use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
use crate::bash_background::BgTaskStatus;
use std::path::{Path, PathBuf};

const TASK_ID: &str = "bash-0123456789abcdef";
const SOURCE: &str = "fn greeting() -> &'static str {\n    \"hello phone\"\n}\n\nfn main() {\n    println!(\"{}\", greeting());\n}\n";

struct Case {
    name: &'static str,
    tool: &'static str,
    arguments: Value,
    error: Option<&'static str>,
}

fn cases() -> Vec<Case> {
    let mut cases = vec![
        ("read", json!({"path":"main.rs"})),
        ("aft_outline", json!({"target":"main.rs"})),
        (
            "aft_zoom",
            json!({"batch":[{"target":"main.rs","symbols":["greeting"]}]}),
        ),
        ("aft_search", json!({"pattern":"greeting"})),
        ("grep", json!({"pattern":"greeting","include":"*.rs"})),
        ("glob", json!({"pattern":"*.rs"})),
        (
            "aft_callgraph",
            json!({"path":"main.rs","symbol":"greeting","op":"callers"}),
        ),
        ("bash_status", json!({"taskId":TASK_ID})),
        ("bash_watch", json!({"taskId":TASK_ID})),
    ]
    .into_iter()
    .map(|(tool, arguments)| Case {
        name: tool,
        tool,
        arguments,
        error: None,
    })
    .collect::<Vec<_>>();
    assert_eq!(
        cases.iter().map(|case| case.tool).collect::<Vec<_>>(),
        carrier_read_only::ALLOWED_TOOLS,
        "Every admitted tool needs a successful wire sample"
    );
    for (name, tool, arguments, error) in [
        (
            "refused-edit",
            "edit",
            json!({"path":"main.rs","edits":[]}),
            "carrier_read_only",
        ),
        (
            "refused-web",
            "aft_outline",
            json!({"target":"https://example.com/phone"}),
            "carrier_read_only",
        ),
        (
            "refused-github-issue",
            "read",
            json!({"path":"issue://935"}),
            "carrier_read_only",
        ),
        (
            "refused-github-pr",
            "read",
            json!({"path":"pr://935"}),
            "carrier_read_only",
        ),
        (
            "refused-invalid-arguments",
            "read",
            json!({"path":"main.rs","unexpected":true}),
            "invalid_request",
        ),
    ] {
        cases.push(Case {
            name,
            tool,
            arguments,
            error: Some(error),
        });
    }
    cases
}

fn fixture() -> (tempfile::TempDir, ProjectRootId, Arc<AppContext>) {
    let (dir, _) = test_support::test_root("carrier-samples");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("main.rs"), SOURCE).unwrap();
    let root = ProjectRootId::from_path(&project).unwrap();
    let ctx = test_support::test_ctx();
    let config = json!({"indexes":{"trigram":true,"semantic":false,"callgraph":true}});
    let request: RawRequest = serde_json::from_value(json!({
        "id":"configure-carrier-samples", "command":"configure", "harness":"runner",
        "project_root":project, "storage_dir":dir.path().join("storage"),
        "config":[{"tier":"user","source":dir.path().join("aft.jsonc"),"doc":config.to_string()}]
    }))
    .unwrap();
    let response = crate::commands::configure::handle_configure(&request, &ctx);
    assert!(response.success, "configure: {response:?}");
    crate::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        crate::runtime_drain::drain_search_index_events(&ctx);
        crate::runtime_drain::drain_callgraph_store_events(&ctx);
        let search_ready = ctx
            .search_index()
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|index| index.ready);
        let graph_ready = matches!(
            ctx.callgraph_store_for_ops(),
            crate::context::CallgraphStoreAccess::Ready(_)
        );
        if search_ready && graph_ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture indexes did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // A completed task avoids a live process and makes its duration reproducible.
    let storage = crate::bash_background::task_storage_dir(&ctx);
    let paths = task_paths(&storage, "session-s", TASK_ID).unwrap();
    let mut task = PersistedTask::starting(
        TASK_ID.into(),
        "session-s".into(),
        "printf 'hello phone\\n'".into(),
        project.clone(),
        Some(project),
        None,
        false,
        false,
    );
    task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    task.duration_ms = Some(12);
    write_task(&paths.json, &task).unwrap();
    std::fs::write(&paths.stdout, "hello phone\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    ctx.bash_background()
        .replay_session(&storage, "session-s")
        .unwrap();
    (dir, root, ctx)
}

fn dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    match request.command.as_str() {
        "read" => crate::commands::read::handle_read(&request, ctx),
        "outline" => crate::commands::outline::handle_outline(&request, ctx),
        "zoom" => crate::commands::zoom::handle_zoom(&request, ctx),
        "semantic_search" => {
            crate::commands::semantic_search::handle_semantic_search(&request, ctx)
        }
        "grep" => crate::commands::grep::handle_grep(&request, ctx),
        "glob" => crate::commands::glob::handle_glob(&request, ctx),
        "callers" => crate::commands::callers::handle_callers(&request, ctx),
        "bash_status" => crate::commands::bash_status::handle(&request, ctx),
        other => panic!("Unexpected carrier dispatch: {other}"),
    }
}

fn sample_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rich_ui/carrier-samples")
}

fn sample_path(case: &Case) -> PathBuf {
    let windows = sample_dir().join(format!("{}.windows.json", case.name));
    if cfg!(windows) && windows.is_file() {
        windows
    } else {
        sample_dir().join(format!("{}.json", case.name))
    }
}

fn normalized_frame(frame: &Frame, temporary_root: &Path, tool: &str) -> Value {
    let mut body = String::from_utf8(frame.body.clone())
        .unwrap()
        .replace(temporary_root.to_str().unwrap(), "${FIXTURE_ROOT}");
    let timestamps = regex::Regex::new(r#"("(?:started_at|finished_at)":)\d+"#).unwrap();
    body = timestamps.replace_all(&body, "${1}0").into_owned();
    if tool == "bash_watch" {
        // A completed task still reports scheduler-dependent time spent observing it.
        let elapsed = regex::Regex::new(r#"("elapsed_ms":)\d+"#).unwrap();
        body = elapsed.replace_all(&body, "${1}0").into_owned();
        let rendered = regex::Regex::new(r"Waited \d+ms").unwrap();
        body = rendered.replace_all(&body, "Waited 0ms").into_owned();
    }
    // Recompute the body byte length after normalizing volatile values.
    let header = &frame.header;
    let normalized = Frame::build_with_version(
        header.ver,
        header.ty,
        header.flags,
        header.channel,
        header.epoch,
        header.corr,
        body.as_bytes().to_vec(),
    )
    .unwrap();
    let header_hex: String = normalized
        .header
        .encode()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({"header_hex":header_hex,"body_utf8":body})
}

async fn run_case(case: &Case, role: RouteRole) -> Value {
    let (dir, root, ctx) = fixture();
    let mut identity = occipital_identity(&root, role);
    Arc::get_mut(&mut identity.0).unwrap().tool_result_body = true;
    let mut body = json!({"name":case.tool,"preset":"worker","arguments":case.arguments});
    if role == RouteRole::ToolProviderV1 {
        body["op"] = json!("tool.call");
    }
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    let reply = exchange_frame_with_actor(
        request.clone(),
        identity,
        ctx,
        &Arc::new(DispatchPathMetrics::new()),
        dispatch,
    )
    .await;
    if role == RouteRole::ToolProviderV1 && case.error != Some("carrier_read_only") {
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{}: {:?}",
            case.name,
            String::from_utf8_lossy(&reply.body)
        );
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(
            body,
            rich_ui::carrier::error_body("rich_ui_op_malformed"),
            "{}",
            case.name
        );
        rich_ui::carrier::validate_body(None, &body).unwrap();
    } else if let Some(code) = case
        .error
        .filter(|code| !(role == RouteRole::Legacy && *code == "invalid_request"))
    {
        assert_eq!(reply.header.ty, FrameType::Error, "{}", case.name);
        let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(error.code, code, "{}: {error:?}", case.name);
    } else {
        assert_eq!(reply.header.ty, FrameType::Response, "{}", case.name);
        let result: cortexkit_role_tool_provider::result::ToolResult =
            serde_json::from_slice(&reply.body).unwrap();
        cortexkit_role_tool_provider::result::validate(&result).unwrap();
        assert!(result.error_code.is_none(), "{}: {result:?}", case.name);
    }
    json!({"request_frame":normalized_frame(&request, dir.path(), case.tool),"reply_frame":normalized_frame(reply.frame(), dir.path(), case.tool)})
}

#[test]
fn carrier_sample_normalization_is_narrow() {
    let body = r#"{"started_at":17,"duration_ms":12,"waited":{"elapsed_ms":9},"text":"status\nWaited 9ms\n"}"#;
    let frame = Frame::build(
        FrameType::Response,
        control_flags(),
        41,
        1,
        7,
        body.as_bytes().to_vec(),
    )
    .unwrap();
    let watch = normalized_frame(&frame, Path::new("/unused-fixture-root"), "bash_watch");
    assert_eq!(
        watch["body_utf8"],
        r#"{"started_at":0,"duration_ms":12,"waited":{"elapsed_ms":0},"text":"status\nWaited 0ms\n"}"#
    );
    let status = normalized_frame(&frame, Path::new("/unused-fixture-root"), "bash_status");
    assert_eq!(
        status["body_utf8"],
        r#"{"started_at":0,"duration_ms":12,"waited":{"elapsed_ms":9},"text":"status\nWaited 9ms\n"}"#
    );
}

#[tokio::test]
#[ignore = "explicitly regenerate the reviewed CKIOS carrier wire samples"]
async fn capture_carrier_samples() {
    std::fs::create_dir_all(sample_dir()).unwrap();
    for case in cases() {
        let sample = run_case(&case, RouteRole::ToolProviderV1).await;
        let common = sample_dir().join(format!("{}.json", case.name));
        let path = if cfg!(windows) {
            let baseline: Value = serde_json::from_slice(&std::fs::read(&common).unwrap()).unwrap();
            if sample == baseline {
                println!("CARRIER_SAMPLE_UNCHANGED {}", case.name);
                continue;
            }
            // Preserve genuine platform output without rewriting the common captures.
            sample_dir().join(format!("{}.windows.json", case.name))
        } else {
            common
        };
        let json = format!("{}\n", serde_json::to_string_pretty(&sample).unwrap());
        std::fs::write(&path, &json).unwrap();
        // Closed admission refusals have no platform-dependent handler output.
        // Retire old overrides when a formerly successful sample becomes one.
        if !cfg!(windows)
            && sample["reply_frame"]["body_utf8"]
                .as_str()
                .is_some_and(|body| {
                    serde_json::from_str::<Value>(body).is_ok_and(|body| {
                        body["structuredContent"]["code"] == "rich_ui_op_malformed"
                    })
                })
        {
            let windows = sample_dir().join(format!("{}.windows.json", case.name));
            if windows.exists() {
                std::fs::remove_file(windows).unwrap();
                println!("CARRIER_SAMPLE_REMOVED {}.windows", case.name);
            }
        }
        println!(
            "CARRIER_SAMPLE {} {sample}",
            path.file_stem().unwrap().to_str().unwrap()
        );
    }
}

#[tokio::test]
async fn carrier_samples_match_wire() {
    for case in cases() {
        let path = sample_path(&case);
        let expected: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|error| {
                panic!(
                    "{}: {error}; run capture_carrier_samples explicitly",
                    path.display()
                )
            }))
            .unwrap();
        assert_eq!(
            run_case(&case, RouteRole::ToolProviderV1).await,
            expected,
            "carrier sample {}",
            case.name
        );
        // Legacy-route cases retain the original gate and handler coverage.
        run_case(&case, RouteRole::Legacy).await;
    }
}
