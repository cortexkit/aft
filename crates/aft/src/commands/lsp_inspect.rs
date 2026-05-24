use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::context::AppContext;
use crate::lsp::diagnostics::StoredDiagnostic;
use crate::lsp::manager::{PullFileOutcome, ServerAttempt, ServerAttemptResult};
use crate::lsp::registry::{resolve_lsp_binary, servers_for_file, ServerDef};
use crate::lsp::roots::find_workspace_root;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct LspInspectParams {
    file: String,
}

pub fn handle_lsp_inspect(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<LspInspectParams>(req.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("lsp_inspect: invalid params: {err}"),
            );
        }
    };

    let file = Path::new(&params.file);
    let validated = match ctx.validate_path(&req.id, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let canonical = normalize_query_path(&validated);
    let extension = canonical
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_string();

    let config = ctx.config().clone();
    let matching_defs = servers_for_file(&canonical, &config);
    let outcomes = {
        let mut lsp = ctx.lsp();
        lsp.ensure_server_for_file_detailed(&canonical, &config)
    };

    let mut pull_results_json = Vec::new();
    if !outcomes.successful.is_empty() {
        let pull_results = {
            let mut lsp = ctx.lsp();
            match lsp.pull_file_diagnostics(&canonical, &config) {
                Ok(results) => results,
                Err(err) => {
                    crate::slog_warn!("[lsp_inspect] pull_file_diagnostics failed: {err}");
                    Vec::new()
                }
            }
        };
        pull_results_json = pull_results
            .iter()
            .map(|result| {
                serde_json::json!({
                    "server_id": result.server_key.kind.id_str(),
                    "workspace_root": result.server_key.root.display().to_string(),
                    "status": pull_status(&result.outcome),
                })
            })
            .collect();
    }

    let diagnostics = collect_file_diagnostics(ctx, &canonical);
    let matching_servers: Vec<serde_json::Value> = matching_defs
        .iter()
        .map(|def| {
            inspect_server(
                def,
                outcomes
                    .attempts
                    .iter()
                    .find(|a| a.server_id == def.kind.id_str()),
                &canonical,
                &config,
            )
        })
        .collect();

    Response::success(
        &req.id,
        serde_json::json!({
            "file": canonical.display().to_string(),
            "extension": extension,
            "project_root": config.project_root.as_ref().map(|root| root.display().to_string()),
            "experimental_lsp_ty": config.experimental_lsp_ty,
            "disabled_lsp": sorted_disabled_lsp(&config.disabled_lsp),
            "lsp_paths_extra": config
                .lsp_paths_extra
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
            "matching_servers": matching_servers,
            "pull_results": pull_results_json,
            "diagnostics_count": diagnostics.len(),
            "diagnostics": diagnostics_to_json(&diagnostics),
        }),
    )
}

fn inspect_server(
    def: &ServerDef,
    attempt: Option<&ServerAttempt>,
    canonical: &Path,
    config: &crate::config::Config,
) -> serde_json::Value {
    let binary_path = resolve_lsp_binary(
        &def.binary,
        config.project_root.as_deref(),
        &config.lsp_paths_extra,
    );
    let binary_source = classify_binary_source(
        &binary_path,
        config.project_root.as_deref(),
        &config.lsp_paths_extra,
    );
    let workspace_root = match attempt.map(|attempt| &attempt.result) {
        Some(ServerAttemptResult::Ok { server_key }) => Some(server_key.root.clone()),
        _ => find_workspace_root(canonical, &def.root_markers),
    };
    let spawn_status = attempt
        .map(|attempt| spawn_status(&attempt.result))
        .unwrap_or_else(|| "not_attempted".to_string());

    serde_json::json!({
        "id": def.kind.id_str(),
        "name": def.name,
        "kind": def.kind.id_str(),
        "extensions": def.extensions,
        "root_markers": def.root_markers,
        "binary_name": def.binary,
        "binary_path": binary_path.as_ref().map(|path| path.display().to_string()),
        "binary_source": binary_source,
        "workspace_root": workspace_root.as_ref().map(|path| path.display().to_string()),
        "spawn_status": spawn_status,
        "args": def.args,
    })
}

fn classify_binary_source(
    binary_path: &Option<PathBuf>,
    project_root: Option<&Path>,
    extra_paths: &[PathBuf],
) -> &'static str {
    let Some(path) = binary_path else {
        return "not_found";
    };

    if let Some(root) = project_root {
        if path.starts_with(root.join("node_modules").join(".bin")) {
            return "project_node_modules";
        }
    }
    if extra_paths.iter().any(|extra| path.starts_with(extra)) {
        return "lsp_paths_extra";
    }
    "path"
}

fn spawn_status(result: &ServerAttemptResult) -> String {
    match result {
        ServerAttemptResult::Ok { .. } => "ok".to_string(),
        ServerAttemptResult::NoRootMarker { .. } => "no_root_marker".to_string(),
        ServerAttemptResult::BinaryNotInstalled { .. } => "binary_not_installed".to_string(),
        ServerAttemptResult::SpawnFailed { reason, .. } => format!("spawn_failed: {reason}"),
    }
}

fn pull_status(outcome: &PullFileOutcome) -> String {
    match outcome {
        PullFileOutcome::Full { diagnostic_count } => {
            format!("full ({diagnostic_count} diagnostics)")
        }
        PullFileOutcome::Unchanged => "unchanged".to_string(),
        PullFileOutcome::PartialNotSupported => "partial_not_supported".to_string(),
        PullFileOutcome::PullNotSupported => "pull_not_supported".to_string(),
        PullFileOutcome::RequestFailed { reason } => format!("request_failed: {reason}"),
    }
}

fn collect_file_diagnostics(ctx: &AppContext, canonical: &Path) -> Vec<StoredDiagnostic> {
    let lsp = ctx.lsp();
    lsp.get_diagnostics_for_file(canonical)
        .into_iter()
        .cloned()
        .collect()
}

fn diagnostics_to_json(diagnostics: &[StoredDiagnostic]) -> Vec<serde_json::Value> {
    let mut sorted: Vec<&StoredDiagnostic> = diagnostics.iter().collect();
    sorted.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
            .then(left.end_line.cmp(&right.end_line))
            .then(left.end_column.cmp(&right.end_column))
            .then(left.message.cmp(&right.message))
    });

    sorted
        .into_iter()
        .map(|diagnostic| {
            serde_json::json!({
                "file": diagnostic.file.display().to_string(),
                "line": diagnostic.line,
                "column": diagnostic.column,
                "end_line": diagnostic.end_line,
                "end_column": diagnostic.end_column,
                "severity": diagnostic.severity.as_str(),
                "message": diagnostic.message,
                "code": diagnostic.code,
                "source": diagnostic.source,
            })
        })
        .collect()
}

fn sorted_disabled_lsp(disabled: &std::collections::HashSet<String>) -> Vec<String> {
    let mut values: Vec<String> = disabled.iter().cloned().collect();
    values.sort();
    values
}

fn normalize_query_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
