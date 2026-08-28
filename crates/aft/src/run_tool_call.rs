use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub type DispatchFn<'a> = dyn Fn(RawRequest, &AppContext) -> Response + 'a;
pub type FinalizeFn<'a> = dyn Fn(&mut Response) + 'a;

/// Monotonic timestamps for one subc tool call. The recorder stays on the
/// request path and only takes an `Instant::now()` at each phase boundary.
#[derive(Debug)]
pub struct PhaseTrace {
    frame_decoded: Instant,
    executor_submitted: Option<Instant>,
    job_admitted: Option<Instant>,
    translate_done: Option<Instant>,
    execute_done: Option<Instant>,
    format_done: Option<Instant>,
    finalize_done: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolCallEgressTiming {
    pub enqueued: Instant,
    pub dequeued: Instant,
    pub write_started: Instant,
    pub write_finished: Instant,
    pub frame_bytes: usize,
    pub queue_depth: usize,
    pub writer_active_at_enqueue: bool,
    pub writer_queue_was_full: bool,
    pub reserve_timeouts: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolCallPhaseDurations {
    pub queue: Duration,
    pub translate: Duration,
    pub execute: Duration,
    pub format: Duration,
    pub finalize: Duration,
    pub egress_enqueue: Duration,
    pub egress_queue: Duration,
    pub egress_prepare: Duration,
    pub egress_write: Duration,
    pub egress: Duration,
    pub frame_bytes: usize,
    pub writer_queue_depth: usize,
    pub writer_active_at_enqueue: bool,
    pub writer_queue_was_full: bool,
    pub writer_reserve_timeouts: u32,
    pub total: Duration,
}

impl PhaseTrace {
    pub fn new(frame_decoded: Instant) -> Self {
        Self {
            frame_decoded,
            executor_submitted: None,
            job_admitted: None,
            translate_done: None,
            execute_done: None,
            format_done: None,
            finalize_done: None,
        }
    }

    pub fn mark_executor_submitted(&mut self) {
        self.executor_submitted = Some(Instant::now());
    }

    pub fn mark_job_admitted(&mut self) {
        self.job_admitted = Some(Instant::now());
    }

    fn mark_translate_done(&mut self) {
        self.translate_done = Some(Instant::now());
    }

    pub(crate) fn mark_execute_done(&mut self) {
        self.execute_done = Some(Instant::now());
    }

    fn mark_format_done(&mut self) {
        self.format_done = Some(Instant::now());
    }

    fn mark_finalize_done(&mut self) {
        self.finalize_done = Some(Instant::now());
    }

    pub fn finish(self, egress: ToolCallEgressTiming) -> Option<ToolCallPhaseDurations> {
        let executor_submitted = self.executor_submitted?;
        let job_admitted = self.job_admitted?;
        let translate_done = self.translate_done?;
        let execute_done = self.execute_done?;
        let format_done = self.format_done?;
        let finalize_done = self.finalize_done?;
        Some(ToolCallPhaseDurations {
            queue: job_admitted.duration_since(executor_submitted),
            translate: translate_done.duration_since(job_admitted),
            execute: execute_done.duration_since(translate_done),
            format: format_done.duration_since(execute_done),
            finalize: finalize_done.duration_since(format_done),
            egress_enqueue: egress.enqueued.duration_since(finalize_done),
            egress_queue: egress.dequeued.duration_since(egress.enqueued),
            egress_prepare: egress.write_started.duration_since(egress.dequeued),
            egress_write: egress.write_finished.duration_since(egress.write_started),
            egress: egress.write_finished.duration_since(finalize_done),
            frame_bytes: egress.frame_bytes,
            writer_queue_depth: egress.queue_depth,
            writer_active_at_enqueue: egress.writer_active_at_enqueue,
            writer_queue_was_full: egress.writer_queue_was_full,
            writer_reserve_timeouts: egress.reserve_timeouts,
            total: egress.write_finished.duration_since(self.frame_decoded),
        })
    }
}

/// The full result of a tool call: the COMPLETE dispatch Response carried VERBATIM,
/// plus the server-rendered agent-facing text (what the deleted TS formatters used to produce).
/// Oracle #1: carry the WHOLE Response — promote nothing, drop nothing (preview_diff, attachments,
/// status_bar, bg_completions, lsp_diagnostics, code, message, candidates, … all ride inside `response`).
#[derive(Debug)]
pub struct ToolCallResult {
    pub text: String,
    pub response: crate::protocol::Response,
}

/// Reserve a discriminated seam so bash/PTY/streaming (P3) doesn't force a signature rewrite.
/// Only `Unary` is constructed today. Do NOT build `Stream`.
#[derive(Debug)]
pub enum ToolCallOutcome {
    Unary(ToolCallResult),
}

/// Server-owned settings for a single `tool_call` request.
/// These fields cannot be supplied through the agent's arguments object.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub project_root: PathBuf,
    pub session_id: Option<String>,
    pub request_id: String,
    pub diagnostics_on_edit: bool,
    pub preview: bool,
    /// Plugin-computed registration fact carried by transports whose bind
    /// protocol cannot include configure-time process state.
    pub edit_slot_survives: Option<bool>,
    /// Whether configure's immediate registration-downgrade warning was discarded
    /// by this transport and must be reported on the first tool call instead.
    pub report_registration_downgrade: bool,
}

pub(crate) fn ensure_hashline_registration(
    app_ctx: &AppContext,
    project_root: &std::path::Path,
    session: &str,
    edit_slot_survives: Option<bool>,
    report_registration_downgrade: bool,
) -> bool {
    let binding_root = app_ctx
        .canonical_cache_root_opt()
        .unwrap_or_else(|| project_root.to_path_buf());
    let edit_slot_survives = match edit_slot_survives {
        Some(value) => value,
        None if app_ctx.harness_opt().is_some_and(|harness| {
            matches!(
                harness,
                crate::harness::Harness::Opencode | crate::harness::Harness::Pi
            )
        }) && app_ctx
            .hashline_bindings()
            .peek(&binding_root, session)
            .is_none() =>
        {
            false
        }
        None => return false,
    };
    let registration = app_ctx.hashline_bindings().register(
        &binding_root,
        session.to_string(),
        crate::hashline::integration::RegistrationRequest {
            configured_enabled: app_ctx.config().hashline_enabled,
            edit_slot_survives,
        },
    );
    report_registration_downgrade
        && registration.downgrade.is_some()
        && !registration.stores_preserved
}

fn attach_hashline_downgrade(response: &mut Response) {
    let warning = crate::commands::configure::hashline_downgrade_warning();
    if let Some(data) = response.data.as_object_mut() {
        match data.get_mut("warnings").and_then(Value::as_array_mut) {
            Some(warnings) => warnings.push(warning),
            None => {
                data.insert("warnings".to_string(), json!([warning]));
            }
        }
    }
}

fn append_hashline_downgrade_text(text: &mut String) {
    text.push_str("\n\n");
    text.push_str(crate::commands::configure::HASHLINE_DOWNGRADE_MESSAGE);
}

pub(crate) struct PreparedToolCall {
    pub(crate) request: RawRequest,
    pub(crate) surface_downgraded: bool,
}

pub(crate) fn prepare_tool_call(
    bare_name: &str,
    args: Value,
    format_context: &crate::subc_format::FormatContext,
    ctx: &ToolCallContext,
    app_ctx: &AppContext,
    mut phase_trace: Option<&mut PhaseTrace>,
) -> Result<PreparedToolCall, ToolCallResult> {
    let sanitized_args = strip_agent_preview_arg_owned(args);
    let binding_root = app_ctx
        .canonical_cache_root_opt()
        .unwrap_or_else(|| ctx.project_root.clone());
    let session = ctx
        .session_id
        .as_deref()
        .unwrap_or(crate::protocol::DEFAULT_SESSION_ID);
    let surface_downgraded = ensure_hashline_registration(
        app_ctx,
        &ctx.project_root,
        session,
        ctx.edit_slot_survives,
        ctx.report_registration_downgrade,
    );
    let binding_guard = app_ctx.hashline_bindings().capture(binding_root, session);
    let translate_context = crate::subc_translate::TranslateContext {
        diagnostics_on_edit: ctx.diagnostics_on_edit,
        preview: ctx.preview,
        effective_hashline: crate::hashline::integration::effective_for_capture(
            binding_guard.as_ref(),
        ),
    };
    let (command, translated_args) = if crate::subc_translate::supports_tool(bare_name) {
        match crate::subc_translate::subc_translate_owned_with_context(
            bare_name,
            sanitized_args,
            ctx.project_root.as_path(),
            translate_context,
        ) {
            Ok(translated) => (translated.command, translated.args),
            Err(err) => {
                if let Some(trace) = phase_trace.as_mut() {
                    trace.mark_translate_done();
                    trace.mark_execute_done();
                }
                let response = Response::error(ctx.request_id.clone(), err.code, err.message);
                let result = tool_call_result_from_response(
                    bare_name,
                    format_context,
                    response,
                    surface_downgraded,
                );
                if let Some(trace) = phase_trace.as_mut() {
                    trace.mark_format_done();
                    trace.mark_finalize_done();
                }
                return Err(result);
            }
        }
    } else {
        let map = match sanitized_args {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        (bare_name.to_string(), map)
    };

    let request = match raw_request_from_translated(command, translated_args, ctx) {
        Ok(req) => req,
        Err(error) => {
            if let Some(trace) = phase_trace.as_mut() {
                trace.mark_translate_done();
                trace.mark_execute_done();
            }
            let response = Response::error(
                ctx.request_id.clone(),
                "invalid_request",
                format!("failed to build request from tool call: {error}"),
            );
            let result = tool_call_result_from_response(
                bare_name,
                format_context,
                response,
                surface_downgraded,
            );
            if let Some(trace) = phase_trace.as_mut() {
                trace.mark_format_done();
                trace.mark_finalize_done();
            }
            return Err(result);
        }
    };
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_translate_done();
    }

    Ok(PreparedToolCall {
        request,
        surface_downgraded,
    })
}

pub(crate) fn finish_tool_call_response(
    bare_name: &str,
    format_context: &crate::subc_format::FormatContext,
    mut response: Response,
    surface_downgraded: bool,
    finalizer: Option<&FinalizeFn<'_>>,
    mut phase_trace: Option<&mut PhaseTrace>,
) -> ToolCallResult {
    if surface_downgraded {
        attach_hashline_downgrade(&mut response);
    }
    let mut text =
        crate::subc_format::format_response_with_context(bare_name, &response, format_context);
    if surface_downgraded {
        append_hashline_downgrade_text(&mut text);
    }
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_format_done();
    }
    if let Some(finalizer) = finalizer {
        finalizer(&mut response);
    }
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_finalize_done();
    }
    ToolCallResult { text, response }
}

pub fn run_tool_call(
    bare_name: &str,
    args: Value,
    format_context: &crate::subc_format::FormatContext,
    ctx: &ToolCallContext,
    app_ctx: &AppContext,
    dispatch: &DispatchFn<'_>,
    finalizer: Option<&FinalizeFn<'_>>,
    mut phase_trace: Option<&mut PhaseTrace>,
) -> ToolCallOutcome {
    let prepared = match prepare_tool_call(
        bare_name,
        args,
        format_context,
        ctx,
        app_ctx,
        phase_trace.as_deref_mut(),
    ) {
        Ok(prepared) => prepared,
        Err(result) => return ToolCallOutcome::Unary(result),
    };

    let response = if prepared.request.command == "inspect" {
        crate::commands::inspect::handle_inspect_tool_call(&prepared.request, app_ctx)
    } else {
        dispatch(prepared.request, app_ctx)
    };
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_execute_done();
    }
    let result = finish_tool_call_response(
        bare_name,
        format_context,
        response,
        prepared.surface_downgraded,
        finalizer,
        phase_trace,
    );
    ToolCallOutcome::Unary(result)
}

fn raw_request_from_translated(
    command: String,
    mut params: serde_json::Map<String, Value>,
    ctx: &ToolCallContext,
) -> Result<RawRequest, &'static str> {
    if params.contains_key("method") {
        return Err("duplicate field `command`");
    }

    if ctx.preview {
        params.insert("preview".to_string(), json!(true));
    }

    params.remove("id");
    if command != "bash" {
        params.remove("command");
    }
    params.remove("session_id");
    let lsp_hints = params.remove("lsp_hints").filter(|value| !value.is_null());

    Ok(RawRequest {
        id: ctx.request_id.clone(),
        command,
        lsp_hints,
        session_id: ctx.session_id.clone(),
        params: Value::Object(params),
    })
}

pub(crate) fn strip_agent_preview_arg_owned(mut args: Value) -> Value {
    if let Some(map) = args.as_object_mut() {
        map.remove("preview");
    }
    args
}

fn tool_call_result_from_response(
    bare_name: &str,
    format_context: &crate::subc_format::FormatContext,
    mut response: Response,
    surface_downgraded: bool,
) -> ToolCallResult {
    if surface_downgraded {
        attach_hashline_downgrade(&mut response);
    }
    let mut text =
        crate::subc_format::format_response_with_context(bare_name, &response, format_context);
    if surface_downgraded {
        append_hashline_downgrade_text(&mut text);
    }
    ToolCallResult { text, response }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_trace_reports_execution_and_writer_egress_subphases() {
        let t0 = Instant::now();
        let trace = PhaseTrace {
            frame_decoded: t0,
            executor_submitted: Some(t0 + Duration::from_millis(1)),
            job_admitted: Some(t0 + Duration::from_millis(3)),
            translate_done: Some(t0 + Duration::from_millis(6)),
            execute_done: Some(t0 + Duration::from_millis(10)),
            format_done: Some(t0 + Duration::from_millis(15)),
            finalize_done: Some(t0 + Duration::from_millis(21)),
        };

        let phases = trace
            .finish(ToolCallEgressTiming {
                enqueued: t0 + Duration::from_millis(28),
                dequeued: t0 + Duration::from_millis(35),
                write_started: t0 + Duration::from_millis(37),
                write_finished: t0 + Duration::from_millis(48),
                frame_bytes: 262_144,
                queue_depth: 17,
                writer_active_at_enqueue: true,
                writer_queue_was_full: true,
                reserve_timeouts: 2,
            })
            .unwrap();

        assert_eq!(phases.queue, Duration::from_millis(2));
        assert_eq!(phases.translate, Duration::from_millis(3));
        assert_eq!(phases.execute, Duration::from_millis(4));
        assert_eq!(phases.format, Duration::from_millis(5));
        assert_eq!(phases.finalize, Duration::from_millis(6));
        assert_eq!(phases.egress_enqueue, Duration::from_millis(7));
        assert_eq!(phases.egress_queue, Duration::from_millis(7));
        assert_eq!(phases.egress_prepare, Duration::from_millis(2));
        assert_eq!(phases.egress_write, Duration::from_millis(11));
        assert_eq!(phases.egress, Duration::from_millis(27));
        assert_eq!(phases.frame_bytes, 262_144);
        assert_eq!(phases.writer_queue_depth, 17);
        assert!(phases.writer_active_at_enqueue);
        assert!(phases.writer_queue_was_full);
        assert_eq!(phases.writer_reserve_timeouts, 2);
        assert_eq!(phases.total, Duration::from_millis(48));
    }

    mod raw_request_construction {
        use std::hint::black_box;

        use super::*;
        use crate::test_allocations::count as count_allocations;

        fn context(preview: bool) -> ToolCallContext {
            ToolCallContext {
                project_root: PathBuf::from("/workspace"),
                session_id: Some("session-realistic".to_string()),
                request_id: "subc-7-42".to_string(),
                diagnostics_on_edit: true,
                preview,
                edit_slot_survives: None,
                report_registration_downgrade: false,
            }
        }

        fn object(value: Value) -> serde_json::Map<String, Value> {
            value.as_object().cloned().expect("test input is an object")
        }

        fn legacy_raw_request(
            command: String,
            mut params: serde_json::Map<String, Value>,
            ctx: &ToolCallContext,
        ) -> Result<RawRequest, String> {
            if ctx.preview {
                params.insert("preview".to_string(), json!(true));
            }
            params.insert("id".to_string(), json!(ctx.request_id.clone()));
            params.insert("command".to_string(), json!(command));
            params.insert("session_id".to_string(), json!(ctx.session_id.clone()));
            serde_json::from_value(Value::Object(params)).map_err(|error| error.to_string())
        }

        fn dispatch_result_bytes(request: RawRequest) -> Vec<u8> {
            let response = Response::success(
                request.id.clone(),
                json!({
                    "received_command": request.command,
                    "received_lsp_hints": request.lsp_hints,
                    "received_session_id": request.session_id,
                    "received_params": request.params,
                }),
            );
            serde_json::to_vec(&response).expect("serialize recording dispatch response")
        }

        #[test]
        fn direct_raw_request_construction_avoids_flatten_rematerialization() {
            let direct_params = object(json!({
                "file": "/workspace/src/main.rs",
                "start_line": 150,
                "end_line": 229,
            }));
            let legacy_params = direct_params.clone();
            let ctx = context(false);
            let direct_command = "read".to_string();
            let legacy_command = direct_command.clone();

            let (direct, direct_allocations) = count_allocations(|| {
                raw_request_from_translated(direct_command, direct_params, &ctx)
                    .expect("direct request")
            });
            let (legacy, legacy_allocations) = count_allocations(|| {
                legacy_raw_request(legacy_command, legacy_params, &ctx).expect("legacy request")
            });
            black_box((&direct, &legacy));

            assert_eq!(direct_allocations, 2);
            assert!(
                legacy_allocations >= 20,
                "legacy flatten path unexpectedly used only {legacy_allocations} allocations"
            );
            assert!(
                legacy_allocations >= direct_allocations + 18,
                "direct={direct_allocations}, legacy={legacy_allocations}"
            );
        }

        #[test]
        fn direct_raw_request_matches_legacy_dispatch_bytes() {
            let edits = (0..100)
                .map(|index| {
                    json!({
                        "match": format!("old declaration {index}"),
                        "replacement": format!("new declaration {index}"),
                        "replace_all": false,
                    })
                })
                .collect::<Vec<_>>();
            let cases = [
                (
                    "read",
                    "read",
                    object(json!({
                        "file": "/workspace/src/main.rs",
                        "start_line": 1,
                        "end_line": 80,
                    })),
                    false,
                ),
                (
                    "write",
                    "write",
                    object(json!({
                        "file": "/workspace/src/new.rs",
                        "content": "fn created() {}\n",
                        "create_dirs": true,
                    })),
                    false,
                ),
                (
                    "batch-edit-100",
                    "batch",
                    object(json!({
                        "file": "/workspace/src/large.rs",
                        "edits": edits,
                    })),
                    false,
                ),
                (
                    "preview",
                    "read",
                    object(json!({"file": "/workspace/src/main.rs"})),
                    true,
                ),
                (
                    "lsp-hints",
                    "move_symbol",
                    object(json!({
                        "file": "/workspace/src/main.rs",
                        "symbol": "run",
                        "destination": "/workspace/src/moved.rs",
                        "lsp_hints": {
                            "symbols": [{
                                "name": "run",
                                "file": "/workspace/src/main.rs",
                                "line": 12,
                                "kind": "function",
                            }],
                        },
                    })),
                    false,
                ),
                (
                    "null-lsp-hints",
                    "move_symbol",
                    object(json!({
                        "file": "/workspace/src/main.rs",
                        "symbol": "run",
                        "destination": "/workspace/src/moved.rs",
                        "lsp_hints": null,
                    })),
                    false,
                ),
            ];

            for (label, command, params, preview) in cases {
                let ctx = context(preview);
                let direct = raw_request_from_translated(command.to_string(), params.clone(), &ctx)
                    .expect("direct request");
                let legacy =
                    legacy_raw_request(command.to_string(), params, &ctx).expect("legacy request");

                assert_eq!(
                    dispatch_result_bytes(direct),
                    dispatch_result_bytes(legacy),
                    "recording dispatch response differed for {label}"
                );
            }
        }

        #[test]
        fn direct_raw_request_preserves_method_alias_rejection() {
            let params = object(json!({"method": "agent-supplied-command"}));
            let ctx = context(false);
            let direct_error =
                raw_request_from_translated("read".to_string(), params.clone(), &ctx)
                    .expect_err("method alias must conflict with server-owned command");
            let legacy_error = legacy_raw_request("read".to_string(), params, &ctx)
                .expect_err("legacy path rejects the duplicate alias");

            assert_eq!(direct_error, legacy_error);
        }

        #[test]
        fn direct_raw_request_preserves_bash_command_param() {
            let params = object(json!({
                "command": "echo standalone-tool-call-ok",
                "background": false,
            }));
            let ctx = context(false);
            let request = raw_request_from_translated("bash".to_string(), params, &ctx)
                .expect("bash request");

            assert_eq!(request.command, "bash");
            assert_eq!(
                request.params.get("command").and_then(Value::as_str),
                Some("echo standalone-tool-call-ok")
            );
        }

        #[test]
        fn direct_raw_request_still_strips_command_param_for_non_bash_tools() {
            let params = object(json!({"command": "agent-supplied-command"}));
            let ctx = context(false);
            let request = raw_request_from_translated("read".to_string(), params, &ctx)
                .expect("read request");

            assert_eq!(request.command, "read");
            assert!(request.params.get("command").is_none());
        }
    }
}
