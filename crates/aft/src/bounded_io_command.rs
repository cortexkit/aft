//! Tool-request preflight for bounded filesystem access.

use std::io;
use std::path::{Path, PathBuf};

use crate::bounded_io::{
    enter_if_absent, finish_response, name_walk_root, probe_target, FALLBACK_WALK_BUDGET, IO_BUDGET,
};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[cfg(test)]
thread_local! {
    /// Inputs [`command`] probed before running its handler, so tests can
    /// check that a long `files` list is not probed past what is examined.
    pub(crate) static COMMAND_INPUT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn command(
    req: &RawRequest,
    ctx: &AppContext,
    work: impl FnOnce(&RawRequest, &AppContext) -> Response,
) -> Response {
    let duration = if req.command == "grep" {
        FALLBACK_WALK_BUDGET
    } else {
        IO_BUDGET
    };
    let _scope = enter_if_absent(duration);
    // Probe explicit target roots before canonicalization/stat can cross a
    // protected location. Files themselves are never opened by this probe.
    let root = ctx.config().project_root.clone().unwrap_or_default();
    let mut inputs = Vec::new();
    for key in ["path", "file", "directory", "target", "files"] {
        match req.params.get(key) {
            Some(serde_json::Value::String(path)) => inputs.push(path.as_str()),
            // An outline reads at most the first entries of its `files` list,
            // and none of them for a directory, files-mode or revision
            // outline; probing the rest would cost one access check each
            // for entries the handler never opens.
            Some(serde_json::Value::Array(_)) if key == "files" && req.command == "outline" => {
                inputs.extend(crate::commands::outline::outline_examined_files(req));
            }
            Some(serde_json::Value::Array(paths)) => {
                inputs.extend(paths.iter().filter_map(|p| p.as_str()))
            }
            _ => {}
        }
    }
    if !root.as_os_str().is_empty() {
        inputs.push(root.to_str().unwrap_or_default());
    }
    inputs.sort_unstable();
    inputs.dedup();
    let mut probe_error = None;
    for input in inputs {
        if input.starts_with("http://") || input.starts_with("https://") || input.contains("://") {
            continue;
        }
        let path = if Path::new(input).is_absolute() {
            PathBuf::from(input)
        } else {
            root.join(input)
        };
        name_walk_root(&path, Path::new(input));
        #[cfg(test)]
        COMMAND_INPUT_PROBES.with(|probes| probes.set(probes.get() + 1));
        if let Err(error) = probe_target(&path) {
            // Missing paths still get the handler's normal validation/error.
            if error.kind() == io::ErrorKind::TimedOut {
                probe_error = Some(error);
                break;
            }
        }
    }
    let response = match probe_error {
        Some(error) => Response::error(&req.id, "read_deadline_exceeded", error.to_string()),
        None => work(req, ctx),
    };
    finish_response(response)
}

#[cfg(test)]
fn test_helper_scope() -> Box<dyn std::any::Any> {
    Box::new((
        crate::commands::read::helper_state_scope(),
        crate::hashline::snapshot::helper_state_scope(),
        crate::search_index::helper_state_scope(),
    ))
}

#[cfg(test)]
#[path = "bounded_io_command_tests.rs"]
mod tests;
