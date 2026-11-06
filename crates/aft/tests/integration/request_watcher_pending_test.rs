//! A bounded request drain can leave a ready index behind the live filesystem.
use aft::commands::{glob::handle_glob, grep::handle_grep, read::handle_read};
use aft::config::Config;
use aft::context::{default_language_provider_factory, AppContext};
use aft::protocol::RawRequest;
use aft::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome};
use aft::runtime_drain::{drain_watcher_events, WATCHER_PATH_DRAIN_BATCH_CAP};
use aft::search_index::SearchIndex;
use aft::subc_format::FormatContext;
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

fn call(ctx: &AppContext, name: &str, arguments: Value) -> aft::run_tool_call::ToolCallResult {
    call_with_drain(ctx, name, arguments, false)
}

fn call_with_drain(
    ctx: &AppContext,
    name: &str,
    arguments: Value,
    drain_after: bool,
) -> aft::run_tool_call::ToolCallResult {
    let root = ctx.config().project_root.clone().unwrap();
    let outcome = run_tool_call(
        name,
        arguments,
        &FormatContext::default(),
        &ToolCallContext {
            request_id: name.into(),
            project_root: root,
            standard_edit_grammar: true,
            session_id: None,
            diagnostics_on_edit: false,
            preview: false,
            edit_slot_survives: None,
            report_registration_downgrade: false,
            disabled_tools: None,
            worker_session: false,
            host_tool_names: None,
        },
        ctx,
        &|request: RawRequest, ctx| {
            let response = match request.command.as_str() {
                "grep" => handle_grep(&request, ctx),
                "glob" => handle_glob(&request, ctx),
                "read" => handle_read(&request, ctx),
                "outline" => aft::commands::outline::handle_outline(&request, ctx),
                "zoom" => aft::commands::zoom::handle_zoom(&request, ctx),
                "semantic_search" => {
                    aft::commands::semantic_search::handle_semantic_search(&request, ctx)
                }
                "callers" => aft::commands::callers::handle_callers(&request, ctx),
                other => panic!("unexpected command {other}"),
            };
            if drain_after {
                while ctx.watcher_drain_has_work() {
                    drain_watcher_events(ctx);
                }
            }
            response
        },
        None,
        None,
    );
    let ToolCallOutcome::Unary(result) = outcome;
    assert!(result.response.success, "{}", result.text);
    result
}

