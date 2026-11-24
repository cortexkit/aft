impl aft_edit::edit::ports::PostWriteDiagnostics for crate::lsp::manager::PostEditWaitOutcome {
    fn append_to(&self, result: &mut serde_json::Value) {
        let outcome = self;
        result["lsp_diagnostics"] = serde_json::json!(outcome
            .diagnostics
            .iter()
            .map(|d| {
                serde_json::json!({
                    "file": d.file.display().to_string(),
                    "line": d.line,
                    "column": d.column,
                    "end_line": d.end_line,
                    "end_column": d.end_column,
                    "severity": d.severity.as_str(),
                    "message": d.message,
                    "code": d.code,
                    "source": d.source,
                })
            })
            .collect::<Vec<_>>());

        result["lsp_complete"] = serde_json::Value::Bool(outcome.complete());

        if !outcome.pending_servers.is_empty() {
            result["lsp_status"] = serde_json::json!(if outcome.unresponsive_servers.is_empty() {
                "diagnostics unknown (no authoritative response within the diagnostics budget)"
            } else {
                "diagnostics unknown (server not responding)"
            });
            result["lsp_pending_servers"] = serde_json::json!(outcome
                .pending_servers
                .iter()
                .map(|key| key.kind.id_str().to_string())
                .collect::<Vec<_>>());
        }
        if !outcome.exited_servers.is_empty() {
            result["lsp_exited_servers"] = serde_json::json!(outcome
                .exited_servers
                .iter()
                .map(|key| key.kind.id_str().to_string())
                .collect::<Vec<_>>());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
    use crate::lsp::manager::PostEditWaitOutcome;
    use crate::lsp::registry::ServerKind;
    use crate::lsp::roots::ServerKey;
    use aft_edit::edit::WriteResult;
    use std::sync::Arc;

    #[test]
    fn populated_edit_diagnostics_keep_the_existing_wire_shape() {
        let pending = ServerKey {
            kind: ServerKind::TypeScript(None),
            root: "root".into(),
        };
        let outcome = PostEditWaitOutcome {
            diagnostics: vec![StoredDiagnostic {
                file: "file.ts".into(),
                line: 2,
                column: 3,
                end_line: 2,
                end_column: 7,
                severity: DiagnosticSeverity::Warning,
                message: "example".into(),
                code: Some("example-code".into()),
                source: None,
            }],
            pending_servers: vec![pending.clone()],
            unresponsive_servers: vec![pending],
            exited_servers: vec![ServerKey {
                kind: ServerKind::TypeScriptNative("tsc".into()),
                root: "root".into(),
            }],
            ..Default::default()
        };
        let write = WriteResult {
            markdown_table_advisory: None,
            syntax_valid: None,
            syntax_skipped_reason: None,
            syntax_error_line: None,
            formatted: false,
            format_skipped_reason: None,
            validate_requested: false,
            validation_errors: Vec::new(),
            validate_skipped_reason: None,
            rolled_back: false,
            lsp_outcome: Some(Arc::new(outcome)),
            reformatted_excerpt: None,
            final_text: None,
            write_warnings: Vec::new(),
        };
        let mut reply = serde_json::json!({});
        write.append_lsp_diagnostics_to(&mut reply);
        assert_eq!(serde_json::to_string(&reply).unwrap(), concat!(
                "{\"rolled_back\":false,\"lsp_diagnostics\":[{\"file\":\"file.ts\",\"line\":2,",
                "\"column\":3,\"end_line\":2,\"end_column\":7,\"severity\":\"warning\",",
                "\"message\":\"example\",\"code\":\"example-code\",\"source\":null}],",
                "\"lsp_complete\":false,\"lsp_status\":\"diagnostics unknown (server not responding)\",",
                "\"lsp_pending_servers\":[\"typescript\"],\"lsp_exited_servers\":[\"typescript-native\"]}"
        ));
    }
}
