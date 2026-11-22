#![cfg(test)]

use super::*;
use cortexkit_role_tool_provider::call::ToolCallRequest as ProviderCall;
use serde::Deserialize;
use serde_json::value::RawValue;

const OPENCODE: &str =
    include_str!("../../../../../aft/tests/fixtures/fork-tools-v1/opencode2.json");
const PI: &str = include_str!("../../../../../aft/tests/fixtures/fork-tools-v1/pi.json");
const CLAUDE: &str =
    include_str!("../../../../../aft/tests/fixtures/fork-tools-v1/claude-code.json");
const IN_ROOT_TEXT: &str = "read from the fork's project";
static DISPATCHES: AtomicUsize = AtomicUsize::new(0);

fn request_body(sample: &str) -> &RawValue {
    #[derive(Deserialize)]
    struct Sample<'a> {
        #[serde(rename = "tool.call", borrow)]
        call: CapturedCall<'a>,
    }

    #[derive(Deserialize)]
    struct CapturedCall<'a> {
        #[serde(borrow)]
        request: &'a RawValue,
    }

    serde_json::from_str::<Sample<'_>>(sample)
        .unwrap()
        .call
        .request
}

fn read_tool(revision_reads: bool) -> CatalogTool {
    tools_for_session(CatalogPreset::Worker, &[], false, false, revision_reads)
        .into_iter()
        .find(|tool| tool.name == "read")
        .unwrap()
}

fn dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    DISPATCHES.fetch_add(1, Ordering::SeqCst);
    assert_eq!(request.command, "read");
    crate::commands::read::handle_read(&request, ctx)
}

async fn exchange_sample(
    label: &str,
    request: &[u8],
    identity: &RouteIdentity,
    ctx: &Arc<AppContext>,
) -> (FrameType, Value) {
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        request.to_vec(),
    )
    .unwrap();
    let reply = exchange_frame_with_actor(
        frame,
        identity.clone(),
        ctx.clone(),
        &Arc::new(DispatchPathMetrics::new()),
        dispatch,
    )
    .await;
    let body = String::from_utf8_lossy(&reply.body);
    eprintln!("{label}: {:?} {body}", reply.header.ty);
    (reply.header.ty, serde_json::from_str(&body).unwrap())
}

fn fork_refused(tool: &str, reason: &str) -> Value {
    serde_json::to_value(cortexkit_role_tool_provider::errors::fork_call_refused(
        tool, reason,
    ))
    .unwrap()
}

#[tokio::test]
async fn thalamus_fork_samples_decode_and_obey_fork_admission() {
    // An ordinary outside read must ask. Files under the system temporary
    // directory are exempt from that access check.
    let dir = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
    let base = dir.path().canonicalize().unwrap();
    let project = base.join("project");
    let outside = base.join("outside.txt");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("inside.txt"), IN_ROOT_TEXT).unwrap();
    std::fs::write(&outside, "protected content").unwrap();
    let root = ProjectRootId::from_path(&project).unwrap();
    let mut identity = plugin_prompt_v1_identity(&root);
    Arc::get_mut(&mut identity.0).unwrap().scope = Some(stamp("worker"));
    let ctx = Arc::new(crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project),
            storage_dir: Some(base.join("storage")),
            ..Default::default()
        },
    ));

    for (label, sample, name, expected) in [
        (
            "OpenCode 2.3.2",
            OPENCODE,
            "fork_read",
            fork_refused("fork_read", "not_read_only"),
        ),
        (
            "Pi 1.5.2",
            PI,
            "read",
            json!({
                "code": "tool_schema_changed",
                "message": "schema pin is stale",
                "detail": {"tool": "read", "expected": "a".repeat(64), "current": read_tool(true).schema_digest},
            }),
        ),
        (
            "Claude Code 2.1.327",
            CLAUDE,
            "echo",
            fork_refused("echo", "not_read_only"),
        ),
    ] {
        let raw = request_body(sample).get();
        // The handler can refuse a tool that is not marked read-only before
        // decoding it as a ProviderCall. Check that those bodies decode too.
        let decoded: ProviderCall = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.name, name, "{label}");
        let (ty, reply) = exchange_sample(label, raw.as_bytes(), &identity, &ctx).await;
        assert_eq!(ty, FrameType::Error, "{label}: {reply}");
        assert_eq!(reply, expected, "{label}");

        let mut renamed: Value = serde_json::from_str(raw).unwrap();
        renamed["name"] = json!("edit");
        let (ty, reply) = exchange_sample(
            &format!("{label}, renamed to edit"),
            &serde_json::to_vec(&renamed).unwrap(),
            &identity,
            &ctx,
        )
        .await;
        assert_eq!(ty, FrameType::Error, "{label}: {reply}");
        assert_eq!(reply, fork_refused("edit", "not_read_only"), "{label}");
        assert_eq!(DISPATCHES.load(Ordering::SeqCst), 0, "{label} dispatched");
    }

    // The published Pi call names an AFT tool but was pinned to a stand-in
    // schema. Use a live pin and an actual input only in the controls.
    let mut control: Value = serde_json::from_str(request_body(PI).get()).unwrap();
    let tool = read_tool(false);
    control["schema_pin"] = json!(
        SchemaPin::new(&tool.name, &tool.schema_digest, tool.semantics)
            .encode()
            .unwrap()
    );
    control["arguments"] = json!({"path": "inside.txt"});
    let (ty, reply) = exchange_sample(
        "Pi control, in-root read",
        &serde_json::to_vec(&control).unwrap(),
        &identity,
        &ctx,
    )
    .await;
    assert_eq!(ty, FrameType::Response, "{reply}");
    assert_eq!(reply["isError"], false, "{reply}");
    assert!(reply["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(IN_ROOT_TEXT));
    assert_eq!(DISPATCHES.load(Ordering::SeqCst), 1);

    control["arguments"] = json!({"path": outside});
    let (ty, reply) = exchange_sample(
        "Pi control, outside-root fork read",
        &serde_json::to_vec(&control).unwrap(),
        &identity,
        &ctx,
    )
    .await;
    assert_eq!(ty, FrameType::Error, "{reply}");
    assert_eq!(reply, fork_refused("read", "approval_required"));
    assert_eq!(DISPATCHES.load(Ordering::SeqCst), 2);

    control.as_object_mut().unwrap().remove("fork_id");
    let (ty, reply) = exchange_sample(
        "Pi control, outside-root plain read",
        &serde_json::to_vec(&control).unwrap(),
        &identity,
        &ctx,
    )
    .await;
    assert_eq!(ty, FrameType::Response, "{reply}");
    assert_eq!(reply["structuredContent"]["code"], "permission_required");
    assert!(reply["structuredContent"]["asks"][0]["grant_id"].is_string());
    assert_eq!(DISPATCHES.load(Ordering::SeqCst), 3);
}
