impl crate::edit::ports::PostWriteDiagnostics for crate::lsp::manager::PostEditWaitOutcome {
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
