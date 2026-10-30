//! Request-scoped certification for typescript-language-server's push-only protocol.
//!
//! Its publications merge independently arriving syntax, semantic and suggestion
//! events. An empty syntax publication is not evidence that semantic analysis is
//! complete. The synchronous tsserver commands answer all three categories for an
//! open file; keep their result separate from the continuously updated push store.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lsp_types::{request::ExecuteCommand, ExecuteCommandParams, NumberOrString};
use serde::Deserialize;

use super::client::{LspClient, PendingLspRequest};
use super::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use super::roots::ServerKey;
use super::LspError;

const COMMANDS: [&str; 3] = [
    "syntacticDiagnosticsSync",
    "semanticDiagnosticsSync",
    "suggestionDiagnosticsSync",
];

#[derive(Debug)]
pub(crate) enum CertificationFailure {
    Unsupported(String),
    Timeout(String),
    Failed(String),
    InvalidResponse(String),
    DocumentChanged,
}

impl std::fmt::Display for CertificationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, detail) = match self {
            Self::Unsupported(detail) => ("unsupported", detail.as_str()),
            Self::Timeout(detail) => ("timeout", detail.as_str()),
            Self::Failed(detail) => ("failed", detail.as_str()),
            Self::InvalidResponse(detail) => ("invalid_response", detail.as_str()),
            Self::DocumentChanged => ("document_changed", "the file changed during analysis"),
        };
        write!(f, "typescript_sync_{code}: {detail}; retry aft_inspect")
    }
}

impl From<LspError> for CertificationFailure {
    fn from(error: LspError) -> Self {
        match error {
            LspError::Timeout(detail) => Self::Timeout(detail),
            LspError::ServerError {
                code: -32601 | -32602,
                message,
            } => Self::Unsupported(message),
            error => Self::Failed(error.to_string()),
        }
    }
}

pub(crate) struct TypescriptDiagnosticsRequest {
    key: ServerKey,
    file: PathBuf,
    version: Option<i32>,
    deadline: Instant,
    requests: Vec<(&'static str, Result<PendingLspRequest, LspError>)>,
}

pub(crate) struct TypescriptDiagnosticsReply {
    pub(crate) key: ServerKey,
    pub(crate) file: PathBuf,
    pub(crate) version: Option<i32>,
    pub(crate) result: Result<Vec<StoredDiagnostic>, CertificationFailure>,
}

impl TypescriptDiagnosticsRequest {
    pub(crate) fn start(
        client: &mut LspClient,
        key: ServerKey,
        file: PathBuf,
        version: Option<i32>,
        uri: &lsp_types::Uri,
        deadline: Instant,
    ) -> Self {
        let requests = COMMANDS
            .into_iter()
            .map(|command| {
                let params = ExecuteCommandParams {
                    command: "typescript.tsserverRequest".into(),
                    arguments: vec![
                        serde_json::json!(command),
                        serde_json::json!({ "file": uri.as_str(), "includeLinePosition": true }),
                    // The TypeScript language server routes this enum value to
                    // its semantic server, not its separate, faster syntax server.
                        serde_json::json!({ "executionTarget": 0 }),
                    ],
                    work_done_progress_params: Default::default(),
                };
                crate::slog_info!(
                    "lsp_protocol server=typescript root={} method=workspace/executeCommand event=sent command={} file={} document_version={:?}",
                    key.root.display(), command, file.display(), version
                );
                (command, client.start_request::<ExecuteCommand>(params))
            })
            .collect();
        Self {
            key,
            file,
            version,
            deadline: deadline.min(Instant::now() + Duration::from_secs(10)),
            requests,
        }
    }

    /// Wait without holding the manager lock. One deadline covers all categories.
    pub(crate) fn wait(self) -> TypescriptDiagnosticsReply {
        let mut diagnostics = Vec::new();
        let mut failure = None;
        // Reap every outstanding category request even after one fails, so
        // timeout cleanup removes every pending response handler.
        for (command, request) in self.requests {
            let result = request
                .and_then(|request| {
                    request.wait(self.deadline.saturating_duration_since(Instant::now()))
                })
                .map_err(CertificationFailure::from)
                .and_then(|value| parse_response(&self.file, command, value));
            match result {
                Ok(rows) => {
                    crate::slog_info!(
                        "lsp_protocol server=typescript root={} method=workspace/executeCommand event=received command={} file={} diagnostics={}",
                        self.key.root.display(), command, self.file.display(), rows.len()
                    );
                    diagnostics.extend(rows);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        let result = failure.map_or(Ok(diagnostics), Err);
        TypescriptDiagnosticsReply {
            key: self.key,
            file: self.file,
            version: self.version,
            result,
        }
    }
}

#[derive(Deserialize)]
struct SyncResponse {
    #[serde(rename = "type")]
    kind: String,
    command: String,
    success: bool,
    body: Vec<SyncDiagnostic>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncDiagnostic {
    start_location: Location,
    end_location: Location,
    message: String,
    category: String,
    code: Option<NumberOrString>,
    source: Option<String>,
}

#[derive(Deserialize)]
struct Location {
    line: u32,
    offset: u32,
}

fn parse_response(
    file: &std::path::Path,
    command: &str,
    value: serde_json::Value,
) -> Result<Vec<StoredDiagnostic>, CertificationFailure> {
    let response: SyncResponse = serde_json::from_value(value)
        .map_err(|error| CertificationFailure::InvalidResponse(format!("{command}: {error}")))?;
    if response.kind != "response" || response.command != command || !response.success {
        return Err(CertificationFailure::InvalidResponse(command.into()));
    }
    response
        .body
        .into_iter()
        .map(|diagnostic| {
            if diagnostic.start_location.line == 0
                || diagnostic.start_location.offset == 0
                || diagnostic.end_location.line == 0
                || diagnostic.end_location.offset == 0
                || (diagnostic.end_location.line, diagnostic.end_location.offset)
                    < (
                        diagnostic.start_location.line,
                        diagnostic.start_location.offset,
                    )
            {
                return Err(CertificationFailure::InvalidResponse(format!(
                    "{command}: invalid range"
                )));
            }
            Ok(StoredDiagnostic {
                file: file.to_path_buf(),
                line: diagnostic.start_location.line,
                column: diagnostic.start_location.offset,
                end_line: diagnostic.end_location.line,
                end_column: diagnostic.end_location.offset,
                severity: match diagnostic.category.as_str() {
                    "warning" => DiagnosticSeverity::Warning,
                    "suggestion" => DiagnosticSeverity::Hint,
                    _ => DiagnosticSeverity::Error,
                },
                message: diagnostic.message,
                code: diagnostic.code.map(|code| match code {
                    NumberOrString::Number(code) => code.to_string(),
                    NumberOrString::String(code) => code,
                }),
                source: Some(
                    diagnostic
                        .source
                        .filter(|source| !source.is_empty())
                        .unwrap_or_else(|| "typescript".into()),
                ),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::diagnostics::from_lsp_diagnostics;
    use crate::lsp::registry::ServerKind;
    use serde_json::json;

    #[test]
    fn typescript_sync_matches_push_severities_codes_and_ranges() {
        let file = PathBuf::from("/workspace/emoji.ts");
        for (category, severity) in [
            ("error", 1),
            ("warning", 2),
            ("suggestion", 4),
            ("message", 1),
        ] {
            let sync = parse_response(
                &file,
                COMMANDS[1],
                json!({
                    "type": "response", "command": COMMANDS[1], "success": true,
                    "body": [{
                        "startLocation": { "line": 2, "offset": 7 },
                        "endLocation": { "line": 3, "offset": 11 },
                        "message": "a diagnostic after a UTF-16 surrogate pair",
                        "category": category, "code": 2322
                    }]
                }),
            )
            .unwrap();
            let pushed: Vec<lsp_types::Diagnostic> = serde_json::from_value(json!([{
                "range": { "start": { "line": 1, "character": 6 }, "end": { "line": 2, "character": 10 } },
                "message": "a diagnostic after a UTF-16 surrogate pair",
                "severity": severity, "code": 2322, "source": "typescript"
            }])).unwrap();
            assert_eq!(
                sync,
                from_lsp_diagnostics(file.clone(), pushed, &ServerKind::TypeScript)
            );
            assert_eq!(
                (
                    sync[0].line,
                    sync[0].column,
                    sync[0].end_line,
                    sync[0].end_column
                ),
                (2, 7, 3, 11)
            );
            assert_eq!(sync[0].code.as_deref(), Some("2322"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn typescript_sync_real_server_matches_push_for_the_same_file() {
        use super::super::manager::LspManager;
        let Some(modules) = std::env::var_os("AFT_TEST_TS5_NODE_MODULES").map(PathBuf::from) else {
            eprintln!("skipping real TLS parity: AFT_TEST_TS5_NODE_MODULES unset");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("package.json"), r#"{"private":true}"#).unwrap();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"target":"es2022"},"include":["*.ts"]}"#,
        )
        .unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(
            modules.join("typescript"),
            root.join("node_modules/typescript"),
        )
        .unwrap();
        let file = root.join("parity.ts");
        std::fs::write(
            &file,
            "export const broken: number = \"wrong\";\nfunction unused() { const value = 1; }\n",
        )
        .unwrap();
        let mut manager = LspManager::new();
        manager.override_binary(
            ServerKind::TypeScript,
            modules.join(".bin/typescript-language-server"),
        );
        let opened = crate::lsp::manager::settled_lsp(&mut manager, |startup_manager| {
            startup_manager.ensure_file_open_default(&file)
        })
        .unwrap();
        let key = opened
            .server_keys
            .iter()
            .find(|key| key.kind == ServerKind::TypeScript)
            .unwrap();
        let request = manager
            .begin_scoped_typescript_diagnostics(
                key,
                &file,
                Instant::now() + Duration::from_secs(15),
            )
            .unwrap();
        let mut certified = manager
            .finish_scoped_typescript_diagnostics(request.wait())
            .unwrap();
        assert!(
            certified
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("2322")),
            "{certified:?}"
        );
        assert!(
            certified
                .iter()
                .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Hint),
            "{certified:?}"
        );
        certified.sort_by_key(|d| (d.line, d.column, d.code.clone()));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pushed;
        loop {
            pushed = manager.wait_for_diagnostics_default(
                &file,
                deadline.saturating_duration_since(Instant::now()),
            );
            pushed.sort_by_key(|d| (d.line, d.column, d.code.clone()));
            if pushed == certified || Instant::now() >= deadline {
                break;
            }
        }
        assert_eq!(
            certified, pushed,
            "synchronous and pushed diagnostics must preserve identical stored fields"
        );
    }

    #[test]
    fn typescript_sync_rejects_incomplete_or_malformed_responses() {
        let file = PathBuf::from("/workspace/file.ts");
        for response in [
            json!(null),
            json!({ "type": "noServer" }),
            json!({ "type": "response", "command": COMMANDS[1], "success": false, "body": [] }),
            json!({ "type": "response", "command": COMMANDS[0], "success": true, "body": [] }),
            json!({ "type": "response", "command": COMMANDS[1], "success": true, "body": [{}] }),
            json!({ "type": "response", "command": COMMANDS[1], "success": true, "body": [{
                "startLocation": { "line": 0, "offset": 1 }, "endLocation": { "line": 1, "offset": 1 },
                "message": "invalid range", "category": "error", "code": 1
            }] }),
        ] {
            assert!(matches!(
                parse_response(&file, COMMANDS[1], response),
                Err(CertificationFailure::InvalidResponse(_))
            ));
        }
    }
}
