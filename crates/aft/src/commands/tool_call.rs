use serde_json::{json, Value};
use std::sync::OnceLock;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::run_tool_call::{
    run_tool_call, strip_agent_preview_arg_owned, DispatchFn, ToolCallContext, ToolCallOutcome,
};

type StandaloneDispatch = fn(RawRequest, &AppContext) -> Response;

static STANDALONE_DISPATCH: OnceLock<StandaloneDispatch> = OnceLock::new();

pub fn register_dispatch(dispatch: StandaloneDispatch) {
    let _ = STANDALONE_DISPATCH.set(dispatch);
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let Some(dispatch) = STANDALONE_DISPATCH.get().copied() else {
        return Response::error(
            &req.id,
            "internal_error",
            "tool_call: standalone dispatcher is not registered",
        );
    };
    handle_with_dispatch(req, ctx, &dispatch)
}

#[doc(hidden)]
pub fn handle_with_dispatch(
    req: &RawRequest,
    ctx: &AppContext,
    dispatch: &DispatchFn<'_>,
) -> Response {
    crate::access_policy::with_native_route(req.clone(), ctx, |req, ctx| {
        handle_on_route(&req, ctx, dispatch)
    })
}

fn handle_on_route(req: &RawRequest, ctx: &AppContext, dispatch: &DispatchFn<'_>) -> Response {
    let Some(name) = req
        .params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return Response::error(
            &req.id,
            "invalid_request",
            "tool_call: missing or invalid required string field 'name'",
        );
    };

    if name == "tool_call" {
        return Response::error(
            &req.id,
            "invalid_request",
            "tool_call: recursive tool_call requests are not supported",
        );
    }

    // Standalone callers historically send `args`; silently ignoring that
    // field turns a scoped inspect into a whole-project inspect.
    if req.params.get("args").is_some() && req.params.get("arguments").is_some() {
        return Response::error(
            &req.id,
            "invalid_request",
            "tool_call: pass either 'args' or 'arguments', not both",
        );
    }
    let arguments = req
        .params
        .get("arguments")
        .or_else(|| req.params.get("args"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let preview = req
        .params
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let edit_slot_survives = match req.params.get("edit_slot_survives") {
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "tool_call: edit_slot_survives must be a boolean",
            );
        }
        None => None,
    };
    if ctx.claim_database_runtime_retry(name) {
        ctx.retry_database_runtime();
    }
    let report_registration_downgrade = edit_slot_survives.is_some();
    let config = ctx.config();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let tool_ctx = ToolCallContext {
        project_root,
        session_id: Some(req.session().to_string()),
        request_id: req.id.clone(),
        diagnostics_on_edit: config.diagnostics_on_edit,
        preview,
        edit_slot_survives,
        // Configure emits the warning for legacy callers that omit this flag.
        // A later session that explicitly disables hashline has no configure
        // response, so its first tool call emits that session's one-shot warning.
        report_registration_downgrade,
        standard_edit_grammar: false,
        // Standalone tool calls use the `disabled_tools` list configure resolved
        // when the session connected (the root config).
        disabled_tools: None,
        worker_session: req.worker_session(),
        host_tool_names: req.sent_host_tool_names(),
    };

    let sanitized_arguments = strip_agent_preview_arg_owned(arguments);
    let agent_visible =
        !preview && !crate::subc::is_native_plumbing_call(name, &sanitized_arguments);
    let format_context = crate::subc_format::FormatContext::from_tool_call(
        name,
        &sanitized_arguments,
        tool_ctx.project_root.as_path(),
    );
    let finalizer = |response: &mut Response, text: &mut String| {
        if agent_visible {
            crate::response_finalize::append_cancelled_call_notices(
                response,
                text,
                ctx,
                req.session(),
            );
        }
    };

    match run_tool_call(
        name,
        sanitized_arguments,
        &format_context,
        &tool_ctx,
        ctx,
        dispatch,
        Some(&finalizer),
        None,
    ) {
        ToolCallOutcome::Unary(result) => response_with_text(result.response, result.text),
    }
}

fn response_with_text(mut response: Response, text: String) -> Response {
    if let Some(data) = response.data.as_object_mut() {
        data.insert("text".to_string(), Value::String(text));
    } else {
        response.data = json!({ "text": text });
    }
    response
}
