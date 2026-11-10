//! Command routing over the child checkouts of a parent-folder session.

mod engine;
pub(crate) mod inspect;

use crate::commands::callgraph_store_adapter as adapter;
use crate::context::AppContext;
use crate::data_layer::parent_session::query::{discovered, Gaps};
use crate::data_layer::parent_session::{display_relative, Child, ParentSession, Plane};
use crate::protocol::{RawRequest, Response};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Commands a parent session answers itself from its children.
const ROUTED_COMMANDS: &[&str] = &[
    "semantic_search",
    "callers",
    "call_tree",
    "impact",
    "trace_to",
    "trace_to_symbol",
    "trace_data",
    "inspect",
];

/// Answers `req` from the children when `ctx` is a parent folder session and
/// the command reads an index. `None` lets the ordinary handler run.
pub fn route(req: &RawRequest, ctx: &AppContext) -> Option<Response> {
    if !ROUTED_COMMANDS.contains(&req.command.as_str()) {
        return None;
    }
    let session = crate::data_layer::parent_session::session_for(ctx)?;
    Some(match req.command.as_str() {
        "semantic_search" => engine::search(req, ctx, &session),
        "inspect" => inspect(req, ctx, &session),
        operation => callgraph(req, ctx, &session, operation),
    })
}

fn string_param<'a>(req: &'a RawRequest, name: &str) -> Option<&'a str> {
    req.params.get(name).and_then(Value::as_str)
}

fn missing(req: &RawRequest, operation: &str, name: &str) -> Response {
    Response::error(
        &req.id,
        "invalid_request",
        format!("{operation}: missing required param '{name}'"),
    )
}

fn depth_param(req: &RawRequest, default: u64, max: u64) -> usize {
    req.params
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .min(max) as usize
}

fn include_tests(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Applies the session's read-path authorization before resolving the path
/// to an absolute path within the parent folder.
fn resolve_path(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    raw: &str,
) -> Result<PathBuf, Response> {
    let validated = ctx.validate_read_path(&req.id, req.session(), Path::new(raw))?;
    let absolute = if validated.is_relative() {
        session.root().join(validated)
    } else {
        validated
    };
    Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

/// A response naming why the parent could not answer from one child.
fn gap_response(req: &RawRequest, operation: &str, gaps: Gaps) -> Response {
    let gaps = gaps.into_values();
    let reasons = gaps
        .iter()
        .map(|gap| {
            format!(
                "{}: {}",
                gap["path"].as_str().unwrap_or("."),
                gap["reason"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Response::error_with_data(
        &req.id,
        "parent_child_unavailable",
        format!("{operation}: not answered from this parent folder: {reasons}"),
        json!({"complete": false, "gaps": gaps}),
    )
}

/// Rewrites the call graph's checkout-relative paths to parent-relative ones.
fn prefix_paths(value: &mut Value, prefix: &Path) {
    const PATH_KEYS: &[&str] = &["file", "caller_file", "target_file", "to_file"];
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(text) if PATH_KEYS.contains(&key.as_str()) => {
                        if !Path::new(text.as_str()).is_absolute() {
                            *text = prefix
                                .join(text.as_str())
                                .to_string_lossy()
                                .replace('\\', "/");
                        }
                    }
                    other => prefix_paths(other, prefix),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| prefix_paths(item, prefix)),
        _ => {}
    }
}

fn callgraph(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    operation: &str,
) -> Response {
    let Some(file) = string_param(req, "file") else {
        return missing(req, operation, "file");
    };
    let Some(symbol) = string_param(req, "symbol") else {
        return missing(req, operation, "symbol");
    };
    let file = match resolve_path(req, ctx, session, file) {
        Ok(file) => file,
        Err(response) => return response,
    };
    let mut gaps = Gaps::default();
    let Some(child) = session.child_for(&file) else {
        if !discovered(session, &mut gaps) {
            return gap_response(req, operation, gaps);
        }
        gaps.push(
            "outside_child_repositories",
            display_relative(session.root(), &file),
            "outside every child repository; a parent folder has no call graph of its own".into(),
        );
        return gap_response(req, operation, gaps);
    };
    if !session.planes.callgraph {
        gaps.child(&child, "call graph: the call graph is off".into());
        return gap_response(req, operation, gaps);
    }
    data_layer_callgraph(req, ctx, session, operation, &child, &file, symbol)
}

/// [`callgraph`] answered from the child's own checkout in the index store.
/// The parent keeps that checkout current itself and waits for its
/// recorded edits as a single checkout's query does, so the answer is
/// current.
fn data_layer_callgraph(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    operation: &str,
    child: &Child,
    file: &Path,
    symbol: &str,
) -> Response {
    let plane = crate::data_layer::parent::callgraph_plane(ctx, session.root(), &child.root);
    let Plane::Ready(view) = plane else {
        let mut gaps = Gaps::default();
        if let Some(reason) = plane.gap_reason("call graph") {
            gaps.child(child, reason);
        }
        return gap_response(req, operation, gaps);
    };
    let reader = view.callgraph();
    let answer = run_operation(req, ctx, session, operation, &reader, child, file, symbol);
    // A file whose part of the call graph could not be read was seen as
    // absent, so the answer cannot be trusted.
    if let Some(response) =
        crate::commands::data_callgraph::failed_reads(&req.id, operation, Some(&view))
    {
        return response;
    }
    match answer {
        Ok(mut value) => {
            prefix_paths(&mut value, &child.relative);
            Response::success(&req.id, value)
        }
        Err(response) => response,
    }
}

/// Runs call-graph `operation` for `symbol` in `file` over `store`, the
/// call graph of `child`: the reply's value, with paths relative to the
/// child, or the response to send instead.
#[allow(clippy::too_many_arguments)]
fn run_operation(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
    operation: &str,
    store: &impl crate::callgraph_store::CallGraphRead,
    child: &Child,
    file: &Path,
    symbol: &str,
) -> Result<Value, Response> {
    let tests = include_tests(req);
    let value = match operation {
        "callers" => adapter::callers_result(store, file, symbol, depth_param(req, 1, 100), tests)
            .map(|result| adapter::serialized_value(&req.id, operation, &result)),
        "call_tree" => {
            adapter::call_tree_result(store, file, symbol, depth_param(req, 5, 100), tests)
                .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        "impact" => adapter::impact_result(store, file, symbol, depth_param(req, 5, 100), tests)
            .map(|result| adapter::serialized_value(&req.id, operation, &result)),
        "trace_to" => {
            adapter::trace_to_result(store, file, symbol, depth_param(req, 10, 100), tests)
                .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        "trace_to_symbol" => {
            let Some(to_symbol) = string_param(req, "toSymbol") else {
                return Err(missing(req, operation, "toSymbol"));
            };
            let to_file = match string_param(req, "toFile") {
                Some(raw) => Some(resolve_path(req, ctx, session, raw)?),
                None => None,
            };
            if let Some(to_file) = to_file.as_deref() {
                if !to_file.starts_with(&child.root) {
                    let mut gaps = Gaps::default();
                    gaps.push(
                        "parent_cross_repository",
                        display_relative(session.root(), to_file),
                        format!(
                            "in a different repository than {}; each repository's call graph covers only its own files",
                            child.display()
                        ),
                    );
                    return Err(gap_response(req, operation, gaps));
                }
            }
            adapter::trace_to_symbol_result(
                store,
                file,
                symbol,
                to_symbol,
                to_file.as_deref(),
                depth_param(req, 10, 16),
                tests,
            )
            .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        "trace_data" => {
            let Some(expression) = string_param(req, "expression") else {
                return Err(missing(req, operation, "expression"));
            };
            adapter::trace_data_result(
                store,
                file,
                symbol,
                expression,
                depth_param(req, 5, 100),
                ctx.symbol_cache(),
            )
            .map(|result| adapter::serialized_value(&req.id, operation, &result))
        }
        _ => {
            return Err(Response::error(
                &req.id,
                "unknown_command",
                operation.to_string(),
            ))
        }
    };
    match value {
        Ok(value) => value,
        Err(error) => Err(adapter::store_error_response(&req.id, operation, error)),
    }
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn inspect(req: &RawRequest, ctx: &AppContext, session: &ParentSession) -> Response {
    inspect::answer(req, ctx, session)
}

#[cfg(test)]
mod disk_reserve_tests {}
