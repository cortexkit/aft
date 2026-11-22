//! Fixtures shared by tools-side shell tests and daemon orchestration tests.

/// Run `params` through the bash command handler with `policy` installed as
/// the session's remote policy, as the subc tool-call path does.
pub fn handle_with_policy(
    ctx: &crate::context::AppContext,
    policy: Option<crate::bash_background::RemoteLaunch>,
    mut params: serde_json::Value,
) -> crate::protocol::Response {
    params
        .as_object_mut()
        .unwrap()
        .entry("workdir")
        .or_insert_with(|| serde_json::json!(ctx.config().project_root));
    let request = crate::protocol::RawRequest {
        id: "runon".into(),
        command: "bash".into(),
        session_id: Some("session".into()),
        lsp_hints: None,
        params,
    };
    crate::bash_background::with_remote_policy(policy, || {
        crate::commands::bash::handle(&request, ctx)
    })
}

/// bash_status for `task_id` in `ctx`, rendered as the agent sees it.
pub fn rendered_status(ctx: &crate::context::AppContext, task_id: &str) -> String {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "status", "command": "bash_status", "session_id": "session",
        "params": { "task_id": task_id },
    }))
    .unwrap();
    let response = crate::commands::bash_status::handle(&request, ctx);
    assert!(response.success, "{response:?}");
    crate::subc_format::format_response_with_context(
        "bash_status",
        &response,
        &crate::subc_format::FormatContext::default(),
    )
}
