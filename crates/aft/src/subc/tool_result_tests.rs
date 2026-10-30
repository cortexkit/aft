//! Validate opt-in result bodies and preserve legacy bytes over the same real replies.

use super::*;
use crate::run_tool_call::{
    finish_tool_call_response, run_tool_call, ToolCallContext, ToolCallOutcome, ToolCallResult,
};
use cortexkit_role_tool_provider::result::{validate, ToolResult};
use std::cell::Cell;

/// Removing fixture actors releases the process-wide root-cache read markers.
/// Executor shutdown alone only joins threads; it does not unregister actors.
struct FixtureActors(Arc<Executor>);

impl Drop for FixtureActors {
    fn drop(&mut self) {
        for (root, ctx) in self.0.actor_entries() {
            ctx.bash_background().shutdown();
            ctx.clear_db();
            self.0.remove_actor(&root);
        }
    }
}

#[test]
fn tool_result_error_code_is_preserved_or_falls_back() {
    for (response, expected) in [
        (
            Response::error("test", "path_outside_root", "outside"),
            "path_outside_root",
        ),
        (
            Response {
                id: "test".into(),
                success: false,
                data: json!({}),
            },
            "tool_error",
        ),
    ] {
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &ToolCallResult {
                text: "failed".into(),
                response,
            },
            opted_in(BindTrust::FirstParty),
        )
        .unwrap();
        let frame = project_frame(frame);
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["errorCode"], expected);
        validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
    }
}

fn opted_in(trust: BindTrust) -> wire::ToolResponsePolicy {
    wire::ToolResponsePolicy {
        trust,
        tool_result_body: true,
    }
}

#[test]
fn tool_result_too_large_instructions_are_complete() {
    for action in [
        "read a smaller image",
        "read a page with fewer or smaller images",
        "narrow the call with paths, pattern, or offset/limit",
    ] {
        let response = result_body::too_large("id", 3 * 1024 * 1024, action);
        assert_eq!(response.data["code"], "response_too_large");
        assert_eq!(
            response.data["message"],
            format!("reply too large to send (3.00 MiB, limit 3 MiB); {action}")
        );
    }
}

fn project_frame(mut outgoing: wire::WriterFrame) -> Frame {
    outgoing.project_tool_result().unwrap();
    outgoing.frame
}

#[test]
fn tool_result_error_code_normalization_and_success_omission() {
    for (raw, expected) in [
        ("HTTP-Error / 403", "http_error_403"),
        ("__Permission__Required__", "permission_required"),
        ("403", "error_403"),
        ("!?", "tool_error"),
    ] {
        let result = ToolCallResult {
            text: "failed".into(),
            response: Response::error("id", raw, "failed"),
        };
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &result,
            opted_in(BindTrust::Untrusted),
        )
        .unwrap();
        let frame = project_frame(frame);
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(decoded.error_code.as_deref(), Some(expected));
        validate(&decoded).unwrap();
    }
    let result = ToolCallResult {
        text: String::new(),
        response: Response::success("id", json!({"code":"not_an_error"})),
    };
    let frame = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &result,
        opted_in(BindTrust::FirstParty),
    )
    .unwrap();
    let frame = project_frame(frame);
    let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(decoded.error_code, None);
    assert_eq!(decoded.content.len(), 1);
    validate(&decoded).unwrap();
}

#[test]
fn tool_result_serialized_size_is_strictly_below_three_mib() {
    let mut result = ToolCallResult {
        text: String::new(),
        response: Response::success("id", json!({})),
    };
    let empty = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        7,
        control_flags(),
        &result,
        opted_in(BindTrust::Untrusted),
    )
    .unwrap();
    let empty = project_frame(empty);
    let capacity = result_body::MAX_RESULT_BYTES - empty.body.len();
    for (bytes, code) in [
        (capacity - 1, None),
        (capacity, Some("response_too_large")),
        (capacity + 1, Some("response_too_large")),
    ] {
        result.text = "x".repeat(bytes);
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            7,
            control_flags(),
            &result,
            opted_in(BindTrust::Untrusted),
        )
        .unwrap();
        let frame = project_frame(frame);
        assert_eq!(frame.header.corr, 7);
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(decoded.error_code.as_deref(), code);
        assert!(frame.body.len() < result_body::MAX_RESULT_BYTES);
        validate(&decoded).unwrap();
    }
}

fn scratch_project() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    tempfile::tempdir_in(scratch).unwrap()
}

#[test]
fn tool_result_images_are_ordered_and_kept_in_structured_content() {
    let project = scratch_project();
    let root = project.path();
    let path = root.join("small.png");
    image::RgbImage::new(8, 7).save(&path).unwrap();
    let ctx = context(root);
    let _budget = result_body::enter(true);
    let result = run(&ctx, root, "read", json!({"path": path}), &|req, ctx| {
        crate::commands::read::handle_read(&req, ctx)
    });
    assert!(result.response.success, "{:?}", result.response);
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &result,
            opted_in(trust),
        )
        .unwrap();
        let frame = project_frame(frame);
        validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][1]["type"], "image");
        assert_eq!(body["content"][1]["width"], 8);
        assert_eq!(body["content"][1]["height"], 7);
        assert_eq!(body["content"][1]["mimeType"], "image/png");
        assert!(body["content"][1]["alt"]
            .as_str()
            .unwrap()
            .contains("small.png"));
        if trust == BindTrust::FirstParty {
            assert_eq!(
                body["content"][1]["data"],
                body["structuredContent"]["attachments"][0]["data"]
            );
        }
    }
}

#[tokio::test]
async fn tool_result_ast_search_t6_complete_and_budget_cut_on_negotiated_route() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let (project, root_id) = test_support::test_root("ast-search-t6");
    let root = project.path().to_path_buf();
    std::fs::write(root.join("small.js"), "console.log(1);\n".repeat(3)).unwrap();
    std::fs::write(root.join("large.js"), "console.log(1);\n".repeat(24000)).unwrap();
    let module_root = root.clone();
    let executor = Arc::new(Executor::new());
    let _actors = FixtureActors(executor.clone());
    // Reuse a configured actor without starting unrelated background indexing.
    let ctx = Arc::new(context(&root));
    let app = ctx.app().clone();
    executor.register_actor(root_id, ctx.clone());
    let loop_executor = executor.clone();
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
            app,
            loop_executor,
            |request, ctx| {
                if request.command == "configure" {
                    crate::commands::configure::handle_configure(&request, ctx)
                } else {
                    crate::commands::ast_search::handle_ast_search(&request, &ctx)
                }
            },
            Some(test_support::bind_config_without_indexes(&module_root)),
            false,
            usize::MAX,
            None,
            &module_root.join("storage"),
            QuiescedLsp::ReapInBackground,
            None,
            None,
        ))
    });
    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("AST route response timed out")
    }
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
    let bind = ModuleControlRequest::RouteBind {
        route_channel: 1,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: subc_protocol::BindIdentity::new(&root, "opencode", "ast-t6"),
        principal: Some(Principal::Direct),
        consumer_capabilities: Some(vec![result_body::RESULT_FORMAT.to_owned()]),
        admission_facts: Default::default(),
        scope: None,
        role_versions: None,
        call_mode: None,
    };
    write_frame(
        &mut daemon,
        &Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&bind).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ack = receive(&mut daemon, 0, 1).await;
    assert!(matches!(
        serde_json::from_slice::<ModuleControlResponse>(&ack.body).unwrap(),
        ModuleControlResponse::RouteBindAck {}
    ));
    let configured = ctx.config();
    assert!(
        !configured.indexes.trigram
            && !configured.indexes.semantic
            && !configured.indexes.callgraph,
        "protocol fixture unexpectedly enabled background indexes"
    );
    drop(configured);
    let mut replies = Vec::new();
    for (corr, file) in [(10, "small.js"), (11, "large.js")] {
        let call = json!({"name":"ast_search", "arguments":{
            "pattern":"console.log($X)", "lang":"javascript", "paths":[file]
        }});
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::Request,
                control_flags(),
                1,
                1,
                corr,
                serde_json::to_vec(&call).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        replies.push(receive(&mut daemon, 1, corr).await);
    }
    assert!(
        !ctx.watcher_runtime_active(),
        "transport fixture started a project watcher"
    );
    drop(daemon);
    module_thread.join().unwrap().unwrap();
    for (frame, total) in replies.iter().zip([3, 24000]) {
        assert_eq!(frame.header.ty, FrameType::Response);
        validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["isError"], false, "{body}");
        let data = &body["structuredContent"];
        let text = body["content"][0]["text"].as_str().unwrap();
        assert_eq!(data["total_matches"], total);
        assert_eq!(data["skipped_files"], json!([]));
        let shown = data["matches"].as_array().unwrap().len();
        if total == 3 {
            assert_eq!(shown, 3);
            assert_eq!(data["complete"], true);
            assert!(data.get("matches_list_envelope").is_none());
            assert!(!text.contains("shown "));
        } else {
            assert!(shown > 0 && shown < 24000);
            assert_eq!(data["complete"], false);
            assert_eq!(data["truncated"], true);
            let envelope = &data["matches_list_envelope"];
            assert_eq!(envelope["shown"], shown);
            assert_eq!(envelope["total"], json!({"kind":"exact", "value":24000}));
            assert_eq!(envelope["reason"], "budget");
            assert_eq!(envelope["narrow"], json!(["paths", "globs", "pattern"]));
            let trailer =
                format!("shown {shown} of 24000 results (budget) · narrow: paths, globs, pattern");
            assert!(text.trim_end().ends_with(&trailer), "{text}");
            assert_eq!(
                text.lines()
                    .filter(|line| line.starts_with("shown "))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn tool_result_builders_fit_large_images_and_bound_ast_matches() {
    let project = scratch_project();
    let root = project.path();
    let ctx = context(root);
    let _budget = result_body::enter(true);
    let mut state = 0x12345678u32;
    let image = image::RgbImage::from_fn(1024, 1024, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    let path = root.join("large.png");
    image.save(&path).unwrap();
    let result = run(&ctx, root, "read", json!({"path":path}), &|req, ctx| {
        crate::commands::read::handle_read(&req, ctx)
    });
    assert!(result.response.success, "{:?}", result.response);
    assert!(
        result.response.data["attachments"][0]["width"]
            .as_u64()
            .unwrap()
            < 1024
    );
    assert_eq!(result.response.data["attachments"][0]["resized"], true);
    assert_eq!(result.response.data["attachments"][0]["encoding"], "jpeg");
    let image_frame = project_frame(
        wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &result,
            opted_in(BindTrust::FirstParty),
        )
        .unwrap(),
    );
    let image_body: ToolResult = serde_json::from_slice(&image_frame.body).unwrap();
    assert_eq!(
        image_body.is_error,
        Some(false),
        "fitted image must survive the final reply budget"
    );
    assert_eq!(image_body.content.len(), 2);
    validate(&image_body).unwrap();
    let ast = root.join("large.js");
    std::fs::write(&ast, "console.log(1);\n".repeat(24000)).unwrap();
    let result = run(
        &ctx,
        root,
        "ast_search",
        json!({"pattern":"console.log($X)","lang":"javascript","paths":[ast]}),
        &|req, ctx| crate::commands::ast_search::handle_ast_search(&req, ctx),
    );
    assert!(result.response.success);
    assert_eq!(result.response.data["total_matches"], 24000);
    let shown = result.response.data["matches"].as_array().unwrap().len();
    assert!(shown > 0 && shown < 24000);
    assert_eq!(result.response.data["complete"], false);
    assert_eq!(
        result.response.data["matches_list_envelope"]["shown"],
        shown
    );
    assert!(result.text.contains("shown "));
    assert!(result.text.contains("24000 results (budget)"));
    let frame = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &result,
        opted_in(BindTrust::FirstParty),
    )
    .unwrap();
    let frame = project_frame(frame);
    let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
    assert_ne!(
        decoded.is_error,
        Some(true),
        "bounded search should fit without losing its prefix"
    );
    validate(&decoded).unwrap();
}

#[test]
fn tool_result_every_preset_and_confined_catalog_declares_format() {
    for preset in tool_provider::CatalogPreset::ALL {
        for confined in [false, true] {
            let (request, preset) =
                tool_provider::validate_catalog_request(json!({"preset": preset.name()})).unwrap();
            let render = if confined {
                tool_provider::catalog_for_confined_request
            } else {
                tool_provider::catalog_for_request
            };
            let catalog = render(
                request,
                preset,
                &[],
                true,
                true,
                true,
                crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS,
            )
            .unwrap();
            let tools = catalog["tools"].as_array().unwrap();
            assert!(!tools.is_empty(), "{} confined={confined}", preset.name());
            for tool in tools {
                assert_eq!(
                    tool["result_format"],
                    "tool-result/v1",
                    "{} confined={confined} tool={}",
                    preset.name(),
                    tool["name"]
                );
            }
        }
    }
}

#[test]
fn tool_result_catalog_switch_preserves_schemas_digests_and_pins() {
    for preset in tool_provider::CatalogPreset::ALL {
        let original = tool_provider::tools_for_session(preset, &[], true, true, true);
        let mut enabled = original.clone();
        tool_provider::declare_result_format(&mut enabled, true);
        assert!(!enabled.is_empty());
        for (before, after) in original.iter().zip(&enabled) {
            assert_eq!(
                after.result_format.as_ref().unwrap().as_str(),
                result_body::RESULT_FORMAT
            );
            let mut after = serde_json::to_value(after).unwrap();
            after.as_object_mut().unwrap().remove("result_format");
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                after,
                "only result_format may change"
            );
        }
        assert!(original.iter().all(|tool| tool.result_format.is_none()));
    }
}

#[tokio::test]
async fn tool_result_capability_is_selected_per_bind_not_per_call() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let project = scratch_project();
    let root = project.path().to_path_buf();
    let module_root = root.clone();
    let executor = Arc::new(Executor::new());
    let _actors = FixtureActors(executor.clone());
    let loop_executor = executor.clone();
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
            loop_executor,
            |request, ctx| {
                if request.command == "configure" {
                    return crate::commands::configure::handle_configure(&request, ctx);
                }
                Response::error_with_data(
                    "same-id",
                    "path_outside_root",
                    "outside root",
                    json!({"producer_budget": result_body::active()}),
                )
            },
            Some(test_support::bind_config_without_indexes(&module_root)),
            false,
            usize::MAX,
            None,
            &module_root.join("storage"),
            QuiescedLsp::ReapInBackground,
            None,
            None,
        ))
    });
    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("route response timed out")
    }
    let hello = receive(&mut daemon, 0, HELLO_CORR).await;
    assert_eq!(hello.header.ty, FrameType::Hello);
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
    let mut replies = Vec::new();
    for (channel, capabilities) in [
        (1, Some(vec![result_body::RESULT_FORMAT.to_owned()])),
        (2, Some(vec!["content/v1".to_owned()])),
        (3, None),
    ] {
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(&root, "opencode", "result-bodies"),
            principal: Some(Principal::Direct),
            consumer_capabilities: capabilities,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        };
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                channel as u64,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ack = receive(&mut daemon, 0, channel as u64).await;
        assert_eq!(
            ack.header.ty,
            FrameType::Response,
            "bind refused: {}",
            String::from_utf8_lossy(&ack.body)
        );
        assert!(matches!(
            serde_json::from_slice::<ModuleControlResponse>(&ack.body).unwrap(),
            ModuleControlResponse::RouteBindAck {}
        ));
        // Both calls on a route must agree, even when a call claims the opposite capability.
        for spoof in [false, true] {
            let call = json!({"name":"read", "arguments":{"path":"missing.txt"}, "consumer_capabilities": if spoof { vec![result_body::RESULT_FORMAT] } else { vec![] }});
            write_frame(
                &mut daemon,
                &Frame::build(
                    FrameType::Request,
                    control_flags(),
                    channel,
                    1,
                    10 + u64::from(spoof),
                    serde_json::to_vec(&call).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            replies.push((
                channel,
                receive(&mut daemon, channel, 10 + u64::from(spoof)).await,
            ));
        }
    }
    drop(daemon);
    module_thread.join().unwrap().unwrap();
    for (channel, frame) in replies {
        assert_eq!(frame.header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["isError"], true);
        assert_eq!(body["structuredContent"]["producer_budget"], channel == 1);
        if channel == 1 {
            assert_eq!(body["errorCode"], "path_outside_root");
            validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
        } else {
            assert!(
                body.get("errorCode").is_none(),
                "legacy route must ignore call capability claims"
            );
        }
    }
}

#[tokio::test]
async fn tool_result_non_negotiating_routes_keep_legacy_success_and_error_bytes() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let project = scratch_project();
    let root = project.path().to_path_buf();
    let module_root = root.clone();
    let executor = Arc::new(Executor::new());
    let _actors = FixtureActors(executor.clone());
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
            executor,
            |request, ctx| {
                if request.command == "configure" {
                    return crate::commands::configure::handle_configure(&request, ctx);
                }
                if request.params["file"]
                    .as_str()
                    .is_some_and(|file| std::path::Path::new(file).ends_with("success.txt"))
                {
                    Response::success(
                        "same-id",
                        json!({"content":"route success", "producer_budget":result_body::active()}),
                    )
                } else {
                    Response::error_with_data(
                        "same-id",
                        "path_outside_root",
                        "outside root",
                        json!({"producer_budget":result_body::active()}),
                    )
                }
            },
            Some(test_support::bind_config_without_indexes(&module_root)),
            false,
            usize::MAX,
            None,
            &module_root.join("storage"),
            QuiescedLsp::ReapInBackground,
            None,
            None,
        ))
    });
    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("legacy route response timed out")
    }
    let hello = receive(&mut daemon, 0, HELLO_CORR).await;
    assert_eq!(hello.header.ty, FrameType::Hello);
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
    let mut replies = Vec::new();
    for (channel, capabilities) in [(1, None), (2, Some(vec!["content/v1".to_owned()]))] {
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(&root, "opencode", "legacy-result-bytes"),
            principal: Some(Principal::Direct),
            consumer_capabilities: capabilities,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        };
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                channel as u64,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ack = receive(&mut daemon, 0, channel as u64).await;
        assert_eq!(
            ack.header.ty,
            FrameType::Response,
            "bind refused for channel {channel}: {}",
            String::from_utf8_lossy(&ack.body)
        );
        assert!(matches!(
            serde_json::from_slice::<ModuleControlResponse>(&ack.body).unwrap(),
            ModuleControlResponse::RouteBindAck {}
        ));
        for (corr, path) in [(10, "success.txt"), (11, "error.txt")] {
            write_frame(
                &mut daemon,
                &Frame::build(
                    FrameType::Request,
                    control_flags(),
                    channel,
                    1,
                    corr,
                    serde_json::to_vec(&json!({"name":"read", "arguments":{"path":path}})).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            replies.push((corr, receive(&mut daemon, channel, corr).await));
        }
    }
    drop(daemon);
    module_thread.join().unwrap().unwrap();
    for (corr, frame) in replies {
        assert_eq!(frame.header.ty, FrameType::Response);
        let expected: &[u8] = if corr == 10 {
            br#"{"content":[{"type":"text","text":"route success"}],"isError":false,"structuredContent":{"id":"same-id","success":true,"content":"route success","producer_budget":false,"text":"route success"}}"#
        } else {
            br#"{"content":[{"type":"text","text":"outside root"}],"isError":true,"structuredContent":{"id":"same-id","success":false,"code":"path_outside_root","message":"outside root","producer_budget":false,"text":"outside root"}}"#
        };
        assert_eq!(frame.body, expected, "legacy bytes changed for call {corr}");
    }
}

#[test]
fn tool_result_retained_legacy_replies_use_the_current_route() {
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let result = ToolCallResult {
            text: "outside root".into(),
            response: Response::error("old-record", "path_outside_root", "outside root"),
        };
        let legacy = wire::tests::legacy_tool_response_body(&result, trust != BindTrust::Untrusted);
        let stored = crate::db::call_ledger::RecordedFrame {
            ty: FrameType::Response,
            body: legacy.clone(),
        };
        for enabled in [false, true] {
            let frame =
                Frame::build(stored.ty, control_flags(), 4, 2, 19, stored.body.clone()).unwrap();
            let outgoing = wire::WriterFrame::retained_tool_response(
                frame,
                wire::ToolResponsePolicy {
                    trust,
                    tool_result_body: enabled,
                },
                None,
            );
            let frame = project_frame(outgoing);
            if enabled {
                let body: ToolResult = serde_json::from_slice(&frame.body).unwrap();
                validate(&body).unwrap();
                assert_eq!(
                    body.error_code.as_deref(),
                    Some(if trust == BindTrust::Untrusted {
                        "tool_error"
                    } else {
                        "path_outside_root"
                    })
                );
            } else {
                assert_eq!(
                    frame.body, legacy,
                    "pre-switch record must replay byte for byte on a legacy route"
                );
            }
        }
        assert_eq!(
            stored.body, legacy,
            "projection must not mutate retained bytes"
        );
    }
    let oversized = ToolCallResult {
        text: "large sidecar".into(),
        response: Response::success(
            "old-large",
            json!({"data":"x".repeat(result_body::MAX_RESULT_BYTES)}),
        ),
    };
    let legacy = wire::tests::legacy_tool_response_body(&oversized, true);
    let outgoing = wire::WriterFrame::retained_tool_response(
        Frame::build(FrameType::Response, control_flags(), 1, 1, 1, legacy).unwrap(),
        opted_in(BindTrust::FirstParty),
        None,
    );
    let frame = project_frame(outgoing);
    let body: ToolResult = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.error_code.as_deref(), Some("response_too_large"));
    validate(&body).unwrap();

    use base64::Engine;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 7))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let github_image = ToolCallResult {
        text: "read image".into(),
        response: Response::success(
            "old-github-image",
            json!({"attachments":[{"kind":"image","mime":"image/png","data":base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()),"source_url":"https://github.com/user-attachments/files/7/image.png"}]}),
        ),
    };
    let legacy = wire::tests::legacy_tool_response_body(&github_image, true);
    let outgoing = wire::WriterFrame::retained_tool_response(
        Frame::build(
            FrameType::Response,
            control_flags(),
            1,
            1,
            1,
            legacy.clone(),
        )
        .unwrap(),
        opted_in(BindTrust::FirstParty),
        Some("issue://7"),
    );
    let frame = project_frame(outgoing);
    let body: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body["content"][1]["width"], 8);
    assert_eq!(body["content"][1]["height"], 7);
    assert_eq!(body["content"][1]["alt"], "issue://7");
    validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
    assert!(
        !serde_json::from_slice::<Value>(&legacy).unwrap()["structuredContent"]["attachments"][0]
            .as_object()
            .unwrap()
            .contains_key("width")
    );
}

#[tokio::test]
async fn tool_result_ledger_records_before_projection_and_replays_by_route() {
    use crate::db::call_ledger as ledger;
    let (dir, root) = test_support::test_root("result-projection-ledger");
    let ctx = test_support::test_ctx();
    let path = dir.path().join("aft.db");
    ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&path).unwrap())));
    let executor = Arc::new(Executor::new());
    let _actors = FixtureActors(executor.clone());
    executor.register_actor(root.clone(), ctx);
    let identity = |enabled| {
        let mut identity = test_support::route_identity(&root, "result-projection");
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.role = tool_provider::RouteRole::ToolProviderV1;
        data.tool_result_body = enabled;
        identity
    };
    let key = tool_provider::LedgerEdge::key(&identity(true), "retained-result");
    let (edge, mut ready) = tool_provider::LedgerEdge::new();
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (actual, receiver) = mpsc::channel(10);
    let (pong, pong_receiver) = mpsc::channel(1);
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let output = spawn_writer_task(module, receiver, pong_receiver, metrics.clone());
    let (writer, proxy) = edge.proxy(actual, executor.clone(), metrics.clone());
    let decode = |corr| {
        DecodedFrame {
        parsed_body: Default::default(), confinement: None, launch_restore: None, codemode_policies: None,
        frame: Frame::build(FrameType::Request, control_flags(), 42, 1, corr,
            serde_json::to_vec(&json!({"name":"read","arguments":{"path":"missing.txt"},"call_key":"retained-result"})).unwrap()).unwrap(),
        phase_trace: PhaseTrace::new(Instant::now()),
    }
    };
    assert!(edge
        .defer(
            decode(7),
            &HashMap::from([(route_key(42, 1), identity(true))]),
            &executor,
            &writer,
            &metrics
        )
        .is_ok());
    tokio::time::timeout(Duration::from_secs(5), ready.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(edge
        .defer(
            decode(10),
            &HashMap::from([(route_key(42, 1), identity(true))]),
            &executor,
            &writer,
            &metrics
        )
        .is_ok());
    // The actor barrier makes the duplicate observe the pending row before the
    // original reply settles it, rather than accidentally testing a settled replay.
    executor
        .submit_async(
            root.clone(),
            Lane::Mutating,
            "admission-barrier".into(),
            Box::new(|_| Response::success("admission-barrier", json!({}))),
        )
        .await
        .unwrap();
    let result = ToolCallResult {
        text: "outside root".into(),
        response: Response::error("retained-id", "path_outside_root", "outside root"),
    };
    let legacy = wire::tests::legacy_tool_response_body(&result, true);
    let outgoing = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(42, 1),
        7,
        control_flags(),
        &result,
        opted_in(BindTrust::FirstParty),
    )
    .unwrap();
    send_reliable_writer_frame(&writer, &metrics, outgoing, "recording-order test")
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&first.body).unwrap()["errorCode"],
        "path_outside_root"
    );
    let attached = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::collections::BTreeSet::from([first.header.corr, attached.header.corr]),
        std::collections::BTreeSet::from([7, 10])
    );
    let attached_body: ToolResult = serde_json::from_slice(&attached.body).unwrap();
    assert_eq!(
        attached_body.error_code.as_deref(),
        Some("path_outside_root")
    );
    validate(&attached_body).unwrap();
    let conn = crate::db::open(&path).unwrap();
    let stored = ledger::get(&conn, &key).unwrap().unwrap().frame.unwrap();
    assert_eq!(
        stored.body, legacy,
        "the ledger must retain pre-projection legacy bytes"
    );
    drop(conn);
    for (enabled, corr) in [(false, 8), (true, 9)] {
        assert!(edge
            .defer(
                decode(corr),
                &HashMap::from([(route_key(42, 1), identity(enabled))]),
                &executor,
                &writer,
                &metrics
            )
            .is_ok());
        let reply = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut daemon))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.corr, corr);
        if enabled {
            let decoded: ToolResult = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(decoded.error_code.as_deref(), Some("path_outside_root"));
            validate(&decoded).unwrap();
        } else {
            assert_eq!(
                reply.body, legacy,
                "legacy replay must use the stored bytes verbatim"
            );
        }
    }
    drop(writer);
    proxy.await.unwrap();
    drop(pong);
    output.await.unwrap().unwrap();
}

async fn assert_keyed_projection_replay_is_exact(result: ToolCallResult, trust: BindTrust) {
    use crate::db::call_ledger as ledger;
    let (dir, root) = test_support::test_root("exact-projection-replay");
    let ctx = test_support::test_ctx();
    let path = dir.path().join("aft.db");
    ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&path).unwrap())));
    let executor = Arc::new(Executor::new());
    let _actors = FixtureActors(executor.clone());
    executor.register_actor(root.clone(), ctx.clone());
    let identity = |enabled| {
        let mut identity = test_support::route_identity(&root, "exact-projection");
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.role = tool_provider::RouteRole::ToolProviderV1;
        data.trust = trust;
        data.tool_result_body = enabled;
        identity
    };
    let key = tool_provider::LedgerEdge::key(&identity(true), "exact-result");
    let (edge, mut ready) = tool_provider::LedgerEdge::new();
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (actual, receiver) = mpsc::channel(10);
    let (pong, pong_receiver) = mpsc::channel(1);
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let output = spawn_writer_task(module, receiver, pong_receiver, metrics.clone());
    let (writer, proxy) = edge.proxy(actual, executor.clone(), metrics.clone());
    let decode = |corr| {
        DecodedFrame {
        parsed_body: Default::default(), confinement: None, launch_restore: None, codemode_policies: None,
        frame: Frame::build(FrameType::Request, control_flags(), 42, 1, corr,
            serde_json::to_vec(&json!({"name":"read","arguments":{"path":"fixture.png"},"call_key":"exact-result"})).unwrap()).unwrap(),
        phase_trace: PhaseTrace::new(Instant::now()),
    }
    };
    let routes = HashMap::from([(route_key(42, 1), identity(true))]);
    assert!(edge
        .defer(decode(7), &routes, &executor, &writer, &metrics)
        .is_ok());
    tokio::time::timeout(Duration::from_secs(5), ready.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(edge
        .defer(decode(10), &routes, &executor, &writer, &metrics)
        .is_ok());
    executor
        .submit_async(
            root.clone(),
            Lane::Mutating,
            "replay-admission-barrier".into(),
            Box::new(|_| Response::success("replay-admission-barrier", json!({}))),
        )
        .await
        .unwrap();
    let mut legacy_data = result.response.data.clone();
    if let Some(parts) = legacy_data
        .get_mut("attachments")
        .and_then(Value::as_array_mut)
    {
        for part in parts {
            part.as_object_mut().unwrap().remove("alt");
        }
    }
    let legacy = wire::tests::legacy_tool_response_body(
        &ToolCallResult {
            text: result.text.clone(),
            response: Response {
                id: result.response.id.clone(),
                success: result.response.success,
                data: legacy_data,
            },
        },
        trust != BindTrust::Untrusted,
    );
    let outgoing = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(42, 1),
        7,
        control_flags(),
        &result,
        opted_in(trust),
    )
    .unwrap();
    send_reliable_writer_frame(&writer, &metrics, outgoing, "exact-replay test")
        .await
        .unwrap();
    let receive = async |daemon: &mut tokio::io::DuplexStream| {
        tokio::time::timeout(Duration::from_secs(5), read_frame(daemon))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    };
    let first = receive(&mut daemon).await;
    let attached = receive(&mut daemon).await;
    assert_eq!(
        std::collections::BTreeSet::from([first.header.corr, attached.header.corr]),
        std::collections::BTreeSet::from([7, 10])
    );
    let decoded: ToolResult = serde_json::from_slice(&first.body).unwrap();
    validate(&decoded).unwrap();
    let first_value: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(
        first_value.get("structuredContent").is_some(),
        trust != BindTrust::Untrusted
    );
    if result.response.success {
        assert_eq!(
            first_value["content"][1]["data"],
            result.response.data["attachments"][0]["data"]
        );
    } else {
        assert_eq!(first_value["errorCode"], "path_outside_root");
    }
    assert_eq!(
        first.body, attached.body,
        "attached duplicate must match the first {trust:?} reply"
    );
    let conn = crate::db::open(&path).unwrap();
    assert_eq!(
        ledger::get(&conn, &key)
            .unwrap()
            .unwrap()
            .frame
            .unwrap()
            .body,
        legacy
    );
    let retained = ledger::get_for_replay(&conn, &key)
        .unwrap()
        .unwrap()
        .tool_result_source
        .unwrap();
    assert!(retained.len() < ledger::MAX_TOOL_RESULT_SOURCE_BYTES);
    let rendered = retained_result::render(
        &retained,
        &legacy,
        trust != BindTrust::Untrusted,
        result_body::MAX_RESULT_BYTES,
    )
    .unwrap();
    assert_eq!(rendered.bytes, first.body);
    drop(conn);
    // A new ledger edge and database connection rule out an in-memory replay cache.
    let (restarted, _ready) = tool_provider::LedgerEdge::new();
    ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&path).unwrap())));
    for (enabled, corr) in [(true, 11), (false, 12)] {
        assert!(restarted
            .defer(
                decode(corr),
                &HashMap::from([(route_key(42, 1), identity(enabled))]),
                &executor,
                &writer,
                &metrics
            )
            .is_ok());
        let replay = receive(&mut daemon).await;
        assert_eq!(replay.header.corr, corr);
        assert_eq!(
            replay.body,
            if enabled {
                first.body.clone()
            } else {
                legacy.clone()
            },
            "settled replay must use the receiving route's format"
        );
    }
    drop(writer);
    proxy.await.unwrap();
    drop(pong);
    output.await.unwrap().unwrap();
}

#[tokio::test]
async fn tool_result_keyed_image_replay_is_exact_on_untrusted_and_trusted_routes() {
    use base64::Engine;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 7))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let data = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
    for trust in [BindTrust::Untrusted, BindTrust::FirstParty] {
        assert_keyed_projection_replay_is_exact(ToolCallResult { text: "read image".into(), response: Response::success("exact-id", json!({"attachments":[{"kind":"image","mime":"image/png","data":data,"width":8,"height":7,"alt":"native image description, not the call path"}]})) }, trust).await;
    }
}

#[tokio::test]
async fn tool_result_keyed_error_replay_is_exact_on_untrusted_and_trusted_routes() {
    for trust in [BindTrust::Untrusted, BindTrust::FirstParty] {
        assert_keyed_projection_replay_is_exact(
            ToolCallResult {
                text: "outside root".into(),
                response: Response::error("exact-id", "path_outside_root", "outside root"),
            },
            trust,
        )
        .await;
    }
}

#[test]
fn tool_result_retention_is_bounded_trust_independent_and_keeps_legacy_view() {
    use base64::Engine;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 7))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let image = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
    let result = ToolCallResult {
        text: "a string containing ,\"structuredContent\": is not a field".into(),
        response: Response::error_with_data(
            "id",
            "path_outside_root",
            "outside",
            json!({"attachments":[{"kind":"image","mime":"image/png","data":image,"width":8,"height":7,"alt":"native image"}],"score":1.2345678901234567,"structuredContent":{"private":"not model input"}}),
        ),
    };
    let mut outgoing = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &result,
        opted_in(BindTrust::Untrusted),
    )
    .unwrap();
    let legacy = outgoing.frame.body.clone();
    let source = outgoing
        .tool_result_source_for_retention()
        .unwrap()
        .unwrap();
    assert_eq!(outgoing.frame.body, legacy);
    assert!(source.len() < crate::db::call_ledger::MAX_TOOL_RESULT_SOURCE_BYTES);
    assert_eq!(
        source
            .windows(image.len())
            .filter(|bytes| *bytes == image.as_bytes())
            .count(),
        1,
        "the source retains only one copy of each image payload"
    );
    let first = project_frame(outgoing);
    for trust in [BindTrust::Untrusted, BindTrust::FirstParty] {
        let frame = Frame::build(
            FrameType::Response,
            control_flags(),
            1,
            1,
            1,
            legacy.clone(),
        )
        .unwrap();
        let replay = project_frame(wire::WriterFrame::retained_tool_response_with_source(
            frame,
            opted_in(trust),
            None,
            Some(source.clone()),
        ));
        let expected = project_frame(
            wire::build_tool_response_frame(
                PROTOCOL_VERSION,
                route_key(1, 1),
                1,
                control_flags(),
                &result,
                opted_in(trust),
            )
            .unwrap(),
        );
        assert_eq!(
            replay.body, expected.body,
            "the original untrusted view must not discard data needed by a trusted replay"
        );
        if trust == BindTrust::Untrusted {
            assert_eq!(replay.body, first.body);
        }
    }
    let frame = Frame::build(
        FrameType::Response,
        control_flags(),
        1,
        1,
        1,
        legacy.clone(),
    )
    .unwrap();
    assert_eq!(
        project_frame(wire::WriterFrame::retained_tool_response_with_source(
            frame,
            wire::ToolResponsePolicy {
                trust: BindTrust::FirstParty,
                tool_result_body: false
            },
            None,
            Some(vec![0xff])
        ))
        .body,
        legacy
    );

    let oversized = ToolCallResult {
        text: "large hidden result".into(),
        response: Response::success(
            "id",
            json!({"opaque":"x".repeat(result_body::MAX_RESULT_BYTES)}),
        ),
    };
    let mut outgoing = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &oversized,
        opted_in(BindTrust::Untrusted),
    )
    .unwrap();
    let legacy = outgoing.frame.body.clone();
    let source = outgoing
        .tool_result_source_for_retention()
        .unwrap()
        .unwrap();
    assert!(
        source.len() < 4096,
        "an unretentable result becomes one bounded named refusal"
    );
    let first = project_frame(outgoing);
    assert_eq!(
        serde_json::from_slice::<Value>(&first.body).unwrap()["errorCode"],
        "response_too_large"
    );
    let frame = Frame::build(FrameType::Response, control_flags(), 1, 1, 1, legacy).unwrap();
    let replay = project_frame(wire::WriterFrame::retained_tool_response_with_source(
        frame,
        opted_in(BindTrust::Untrusted),
        None,
        Some(source),
    ));
    assert_eq!(first.body, replay.body);

    let view_limited = ToolCallResult {
        text: "t".repeat(1024 * 1024),
        response: Response::success("id", json!({"opaque":"x".repeat(2 * 1024 * 1024)})),
    };
    let mut outgoing = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &view_limited,
        opted_in(BindTrust::Untrusted),
    )
    .unwrap();
    let legacy = outgoing.frame.body.clone();
    let source = outgoing
        .tool_result_source_for_retention()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&project_frame(outgoing).body).unwrap()["isError"],
        false
    );
    let frame = Frame::build(FrameType::Response, control_flags(), 1, 1, 1, legacy).unwrap();
    let replay = project_frame(wire::WriterFrame::retained_tool_response_with_source(
        frame,
        opted_in(BindTrust::FirstParty),
        None,
        Some(source),
    ));
    let expected = project_frame(
        wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &view_limited,
            opted_in(BindTrust::FirstParty),
        )
        .unwrap(),
    );
    assert_eq!(
        replay.body, expected.body,
        "each receiving view still applies the same serialized reply cap"
    );
}

#[cfg(target_os = "linux")]
fn fixture_descriptor_targets() -> std::collections::BTreeMap<String, usize> {
    let mut targets = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir("/proc/self/fd").expect("read process descriptors") {
        let entry = entry.unwrap();
        if let Ok(target) = std::fs::read_link(entry.path()) {
            let target = target.to_string_lossy().into_owned();
            // The directory iterator itself contributes one transient descriptor.
            if target == format!("/proc/{}/fd", std::process::id()) {
                continue;
            }
            *targets.entry(target).or_default() += 1;
        }
    }
    targets
}

#[cfg(target_os = "linux")]
#[test]
fn tool_result_fixtures_release_descriptors() {
    const CHILD: &str = "AFT_TOOL_RESULT_FD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // A fresh process excludes unrelated parallel tests from the descriptor
        // census, while still exercising the real fixture setup and teardown.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "subc::tool_result_tests::tool_result_fixtures_release_descriptors",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run isolated fixture descriptor census");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stdout.lines().filter(|line| line.starts_with("FD_CENSUS")) {
            println!("{line}");
        }
        assert!(
            output.status.success(),
            "fixture descriptor census failed:\n{stdout}\n{stderr}"
        );
        return;
    }
    let cases: &[(&str, fn())] = &[
        (
            "error_code",
            tool_result_error_code_is_preserved_or_falls_back,
        ),
        ("wording", tool_result_too_large_instructions_are_complete),
        (
            "normalization",
            tool_result_error_code_normalization_and_success_omission,
        ),
        (
            "size_cap",
            tool_result_serialized_size_is_strictly_below_three_mib,
        ),
        (
            "image_parts",
            tool_result_images_are_ordered_and_kept_in_structured_content,
        ),
        (
            "fitted_image_and_ast",
            tool_result_builders_fit_large_images_and_bound_ast_matches,
        ),
        (
            "catalog",
            tool_result_catalog_switch_preserves_schemas_digests_and_pins,
        ),
        (
            "route_binding",
            tool_result_capability_is_selected_per_bind_not_per_call,
        ),
        (
            "ast_t6_route",
            tool_result_ast_search_t6_complete_and_budget_cut_on_negotiated_route,
        ),
        (
            "legacy_replay",
            tool_result_retained_legacy_replies_use_the_current_route,
        ),
        (
            "ledger_recording",
            tool_result_ledger_records_before_projection_and_replays_by_route,
        ),
        (
            "image_replay",
            tool_result_keyed_image_replay_is_exact_on_untrusted_and_trusted_routes,
        ),
        (
            "error_replay",
            tool_result_keyed_error_replay_is_exact_on_untrusted_and_trusted_routes,
        ),
        (
            "retained_source",
            tool_result_retention_is_bounded_trust_independent_and_keeps_legacy_view,
        ),
        (
            "real_reply_corpus",
            tool_result_real_reply_validation_report,
        ),
    ];
    // Measure every fixture after warming its caches. Nightly stress adds
    // repetitions to expose intermittent or accumulating descriptor leaks.
    let rounds = if std::env::var("AFT_STRESS").as_deref() == Ok("1") {
        20
    } else {
        2
    };
    let mut leaks = Vec::new();
    for (name, fixture) in cases {
        fixture(); // Initialize legitimate process-wide caches before measuring.
        let initial = fixture_descriptor_targets();
        let before: usize = initial.values().sum();
        for round in 0..rounds {
            let prior: usize = fixture_descriptor_targets().values().sum();
            fixture();
            let current: usize = fixture_descriptor_targets().values().sum();
            println!("FD_CENSUS {name} round={round} before={prior} after={current}");
        }
        let final_targets = fixture_descriptor_targets();
        let after: usize = final_targets.values().sum();
        let retained: Vec<_> = final_targets
            .iter()
            .filter_map(|(target, count)| {
                let extra = count.saturating_sub(*initial.get(target).unwrap_or(&0));
                (extra > 0).then(|| format!("{extra} x {target}"))
            })
            .collect();
        println!("FD_CENSUS SUMMARY {name} before={before} after={after} retained={retained:?}");
        if after > before {
            let mut threads: Vec<_> = std::fs::read_dir("/proc/self/task")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    std::fs::read_to_string(entry.path().join("comm"))
                        .ok()
                        .map(|name| {
                            format!("{}:{}", entry.file_name().to_string_lossy(), name.trim())
                        })
                })
                .collect();
            threads.sort();
            println!("FD_CENSUS_THREADS {name} {threads:?}");
            leaks.push(format!("{name}: {before} -> {after}; {retained:?}"));
        }
    }
    assert!(
        leaks.is_empty(),
        "fixtures retained descriptors after {rounds} runs: {leaks:#?}"
    );
}

thread_local! {
    static REPLIES: Cell<usize> = const { Cell::new(0) };
    static CODES: std::cell::RefCell<std::collections::BTreeSet<String>> = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

fn context(root: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(root.join(".storage"));
    config.harness = Some(crate::harness::Harness::Opencode);
    config.experimental_bash_background = true;
    config.restrict_to_project_root = true;
    config.disabled_tools.clear();
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx
}

fn run(
    ctx: &AppContext,
    root: &Path,
    tool: &str,
    args: Value,
    dispatch: &crate::run_tool_call::DispatchFn<'_>,
) -> ToolCallResult {
    let format = crate::subc_format::FormatContext::from_tool_call(tool, &args, root);
    let call = ToolCallContext {
        project_root: root.to_path_buf(),
        session_id: Some("tool-result-audit".into()),
        request_id: "tool-result-audit".into(),
        diagnostics_on_edit: false,
        preview: false,
        edit_slot_survives: None,
        report_registration_downgrade: false,
        standard_edit_grammar: true,
        disabled_tools: Some(Arc::new(Vec::new())),
        worker_session: true,
        host_tool_names: None,
    };
    let ToolCallOutcome::Unary(result) = run_tool_call(
        tool,
        args,
        &format,
        &call,
        ctx,
        dispatch,
        Some(&|response, text| finalize(ctx, tool, response, text)),
        None,
    );
    result
}

fn finalize(ctx: &AppContext, tool: &str, response: &mut Response, text: &mut String) {
    crate::response_finalize::finalize_tool_response(
        response,
        text,
        ctx,
        "tool-result-audit",
        tool,
        false,
    );
}

fn record(tool: &str, shape: &str, result: &ToolCallResult) {
    for (label, trust) in [
        ("trusted", BindTrust::FirstParty),
        ("untrusted", BindTrust::Untrusted),
    ] {
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            Flags::new(false, Priority::Interactive, false),
            result,
            trust,
        )
        .unwrap();
        let frame = project_frame(frame);
        assert_eq!(
            frame.body,
            wire::tests::legacy_tool_response_body(result, trust != BindTrust::Untrusted),
            "legacy bytes: {tool}/{shape}/{label}"
        );
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(2, 1),
            1,
            control_flags(),
            result,
            opted_in(trust),
        )
        .unwrap();
        let frame = project_frame(frame);
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        validate(&decoded).unwrap_or_else(|problem| panic!("{tool}/{shape}/{label}: {problem}"));
        assert!(frame.body.len() < result_body::MAX_RESULT_BYTES);
        REPLIES.set(REPLIES.get() + 1);
        if let Some(code) = result.response.data["code"].as_str() {
            CODES.with_borrow_mut(|codes| {
                codes.insert(code.to_owned());
            });
        }
        println!(
            "| {tool} | {shape} | {label} | {} | {} | validates |",
            frame.body.len(),
            result
                .response
                .data
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("—"),
        );
    }
}

fn expand(value: Value, root: &Path) -> Value {
    match value {
        Value::String(text) => {
            Value::String(text.replace("<PROJECT_ROOT>", &root.to_string_lossy()))
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| expand(item, root)).collect())
        }
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key, expand(value, root)))
                .collect(),
        ),
        other => other,
    }
}

fn native_response(mut flat: Value) -> Response {
    let object = flat.as_object_mut().unwrap();
    let id = object
        .remove("id")
        .and_then(|id| id.as_str().map(str::to_owned))
        .unwrap();
    let success = object
        .remove("success")
        .and_then(|success| success.as_bool())
        .unwrap();
    Response {
        id,
        success,
        data: flat,
    }
}

#[test]
fn tool_result_real_reply_validation_report() {
    REPLIES.set(0);
    CODES.with_borrow_mut(|codes| codes.clear());
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let project = tempfile::tempdir_in(&scratch).unwrap();
    let root = project.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.ts"), "const value = 1;\n").unwrap();
    let ctx = context(root);
    println!("| Tool | Shape/source | Bind | Wire bytes | Native code | Validator result |");
    println!("| --- | --- | --- | ---: | --- | --- |");

    // Replay captured native responses at the dispatch boundary, not handwritten envelopes.
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/subc_parity/format");
    let mut fixtures: Vec<_> = std::fs::read_dir(fixture_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    fixtures.sort();
    let mut covered = std::collections::BTreeSet::new();
    for fixture in &fixtures {
        let input: Value =
            serde_json::from_slice(&std::fs::read(fixture.join("input.json")).unwrap()).unwrap();
        let tool = input["tool_name"].as_str().unwrap();
        let args = expand(input["ctx"]["agent_args"].clone(), root);
        let native = expand(input["native_response_json"].clone(), root);
        let result = if tool == "inspect" {
            // run_tool_call hard-codes inspect dispatch. Replay its captured responses through
            // finish_tool_call_response, the shared renderer used after execution.
            let format = crate::subc_format::FormatContext::from_tool_call(tool, &args, root);
            finish_tool_call_response(
                tool,
                &format,
                native_response(native),
                false,
                Some(&|response, text| finalize(&ctx, tool, response, text)),
                None,
            )
        } else {
            let dispatched = Cell::new(false);
            let result = run(&ctx, root, tool, args, &|_, _| {
                dispatched.set(true);
                native_response(native.clone())
            });
            assert!(
                dispatched.get(),
                "fixture {fixture:?} failed before native response replay: {:?}",
                result.response
            );
            result
        };
        covered.insert(tool.to_string());
        record(
            tool,
            fixture.file_name().unwrap().to_str().unwrap(),
            &result,
        );
    }
    assert!(
        fixtures.len() >= 90,
        "audit must not silently miss the parity corpus"
    );

    let catalog = tool_provider::tools_for_session(
        tool_provider::CatalogPreset::Worker,
        &[],
        true,
        true,
        true,
    );
    for tool in catalog {
        let result = run(&ctx, root, &tool.name, json!([]), &|_, _| {
            panic!("invalid arguments dispatched")
        });
        assert!(
            !result.response.success,
            "{} failed to exercise an error",
            tool.name
        );
        record(&tool.name, "live invalid arguments", &result);
    }
    println!(
        "Parity corpus: {} fixtures, {} tool names",
        fixtures.len(),
        covered.len()
    );
    live_cases(Arc::new(ctx), root, &scratch);
    #[cfg(unix)]
    assert_eq!(REPLIES.get(), 264, "the real-reply corpus must not shrink");
    println!(
        "Validated {} opted-in replies and {} byte-identical legacy replies; machine codes: {:?}",
        REPLIES.get(),
        REPLIES.get(),
        CODES.with_borrow(|codes| codes.clone())
    );
}

#[cfg(unix)]
fn resolve(ctx: &AppContext, outcome: crate::response_finalize::DispatchOutcome) -> Response {
    match outcome {
        crate::response_finalize::DispatchOutcome::Immediate(response) => response,
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(response) = (pending.poll)(ctx) {
                    break response;
                }
                assert!(Instant::now() < deadline, "shell outcome did not resolve");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(unix)]
fn live_cases(ctx: Arc<AppContext>, root: &Path, scratch: &Path) {
    struct Shutdown(Arc<AppContext>);
    impl Drop for Shutdown {
        fn drop(&mut self) {
            self.0.bash_background().shutdown();
        }
    }
    let _shutdown = Shutdown(Arc::clone(&ctx));
    let dispatch = |request: RawRequest, ctx: &AppContext| match request.command.as_str() {
        "read" => crate::commands::read::handle_read(&request, ctx),
        "bash" => resolve(
            ctx,
            crate::commands::bash_orchestrate::build_bash_outcome(
                &request,
                ctx,
                crate::commands::bash::handle(&request, ctx),
            ),
        ),
        "bash_status" => crate::commands::bash_status::handle(&request, ctx),
        "bash_kill" => crate::commands::bash_kill::handle(&request, ctx),
        "bash_write" => crate::commands::bash_write::handle(&request, ctx),
        "ast_search" => crate::commands::ast_search::handle_ast_search(&request, ctx),
        other => panic!("unexpected live command: {other}"),
    };
    let external = tempfile::tempdir_in(scratch).unwrap();
    let outside = external.path().join("outside.txt");
    std::fs::write(&outside, "external content").unwrap();
    let result = run(&ctx, root, "read", json!({"path": outside}), &dispatch);
    assert_eq!(result.response.data["code"], "path_outside_root");
    record("read", "live path_outside_root", &result);

    ctx.update_config(|config| config.restrict_to_project_root = false);
    let result = {
        let owner = Arc::new(());
        let _ask =
            crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Plugin {
                scope: crate::access_policy::plugin::Scope::new(&owner, "tool-result-audit"),
                once: Vec::new(),
            }));
        run(&ctx, root, "read", json!({"path": outside}), &dispatch)
    };
    assert_eq!(result.response.data["code"], "permission_required");
    record("read", "live permission_required", &result);

    let denied = root.join("denied.txt");
    std::fs::write(&denied, "denied content").unwrap();
    ctx.update_config(|config| {
        config.restrict_to_project_root = true;
        config.sandbox.read_deny = vec![denied.clone()];
    });
    let result = run(&ctx, root, "read", json!({"path": denied}), &dispatch);
    assert_eq!(result.response.data["code"], "denied_floor");
    record("read", "live denied_floor", &result);
    ctx.update_config(|config| config.sandbox.read_deny.clear());

    let foreground = run(
        &ctx,
        root,
        "bash",
        json!({"command":"printf foreground-ok"}),
        &dispatch,
    );
    assert!(foreground.response.success, "{:?}", foreground.response);
    assert!(
        foreground.text.contains("foreground-ok"),
        "foreground must include completed stdout"
    );
    record("bash", "live foreground", &foreground);
    let failed = run(
        &ctx,
        root,
        "bash",
        json!({"command":"printf command-failed; exit 7"}),
        &dispatch,
    );
    assert_eq!(failed.response.data["exit_code"], 7);
    record("bash", "live foreground nonzero exit", &failed);
    let powershell = run(
        &ctx,
        root,
        "powershell",
        json!({"command":"Write-Output foreground-ok"}),
        &|_, _| native_response(serde_json::to_value(&foreground.response).unwrap()),
    );
    record(
        "powershell",
        "foreground native shell response replay",
        &powershell,
    );

    let background = run(
        &ctx,
        root,
        "bash",
        json!({"command":"printf ready; sleep 30", "background":true}),
        &dispatch,
    );
    assert!(background.response.success, "{:?}", background.response);
    record("bash", "live background launch", &background);
    let task = background.response.data["task_id"].as_str().unwrap();
    record(
        "bash_status",
        "live running",
        &run(&ctx, root, "bash_status", json!({"taskId":task}), &dispatch),
    );
    let watched = run(
        &ctx,
        root,
        "bash_watch",
        json!({"taskId":task,"pattern":"ready","timeoutMs":5000}),
        &|request, _| match crate::commands::bash_watch::handle_deferred(&request, Arc::clone(&ctx))
        {
            crate::response_finalize::DispatchOutcome::Immediate(response) => response,
            crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(response) = (pending.poll)(&ctx) {
                        break response;
                    }
                    assert!(Instant::now() < deadline, "bash_watch did not resolve");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        },
    );
    assert!(watched.response.success, "{:?}", watched.response);
    record("bash_watch", "live pattern match", &watched);
    record(
        "bash_kill",
        "live kill",
        &run(&ctx, root, "bash_kill", json!({"taskId":task}), &dispatch),
    );

    let pty = run(
        &ctx,
        root,
        "bash",
        json!({"command":"cat","pty":true}),
        &dispatch,
    );
    assert!(pty.response.success, "{:?}", pty.response);
    let task = pty.response.data["task_id"].as_str().unwrap();
    let written = run(
        &ctx,
        root,
        "bash_write",
        json!({"taskId":task,"input":"hello\n"}),
        &dispatch,
    );
    assert!(written.response.success, "{:?}", written.response);
    record("bash_write", "live PTY write", &written);
    let _ = run(&ctx, root, "bash_kill", json!({"taskId":task}), &dispatch);

    let large_bash = run(
        &ctx,
        root,
        "bash",
        json!({"command":"python3 -c \"print('x' * 4000000)\"","compressed":false}),
        &dispatch,
    );
    assert!(large_bash.response.success, "{:?}", large_bash.response);
    assert_eq!(large_bash.response.data["exit_code"], 0);
    assert!(
        large_bash.text.contains("xxxx"),
        "large output must have been captured"
    );
    record("bash", "live 4MB stdout", &large_bash);

    let mut state = 0x12345678u32;
    let image = image::RgbImage::from_fn(1024, 1024, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    let image_path = root.join("large.png");
    image.save(&image_path).unwrap();
    let result = run(&ctx, root, "read", json!({"path":image_path}), &dispatch);
    assert!(result.response.success, "{:?}", result.response);
    assert!(
        result.response.data["attachments"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "large image must be attached, not omitted"
    );
    record("read", "live large image", &result);

    let ast_path = root.join("large.js");
    std::fs::write(&ast_path, "console.log(1);\n".repeat(24000)).unwrap();
    let result = run(
        &ctx,
        root,
        "ast_search",
        json!({"pattern":"console.log($X)","lang":"javascript","paths":[ast_path]}),
        &dispatch,
    );
    assert!(result.response.success, "{:?}", result.response);
    record("ast_search", "live 24000 matches", &result);
    assert_eq!(
        result.response.data["matches"].as_array().unwrap().len(),
        24000
    );
}

#[cfg(not(unix))]
fn live_cases(_: Arc<AppContext>, _: &Path, _: &Path) {
    println!("Live shell cases require Unix; replay cases remain available on this host.");
}
