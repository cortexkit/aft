use super::AppContext;
use std::{path::Path, sync::Arc};

impl AppContext {
    /// Post-write LSP hook: notify server and optionally collect diagnostics.
    ///
    /// This is the single call site for all command handlers after `write_format_validate`.
    /// Behavior:
    /// - When `diagnostics: true` is in `params`, notifies the server, waits
    ///   until matching diagnostics arrive or the timeout expires, and returns
    ///   `Some(outcome)` with the verified-fresh diagnostics + per-server
    ///   status.
    /// - When `diagnostics: false` (or absent), just notifies (fire-and-forget)
    ///   and returns `None`. Callers must NOT wrap this in `Some(...)`; the
    ///   `None` is what tells the response builder to omit the LSP fields
    ///   entirely (preserves the no-diagnostics-requested response shape).
    ///
    /// v0.17.3: default `wait_ms` raised from 1500 to 3000 because real-world
    /// tsserver re-analysis on monorepo files routinely takes 2-5s. Still
    /// capped at 10000ms.
    pub fn lsp_post_write(
        &self,
        file_path: &Path,
        content: &str,
        params: &serde_json::Value,
    ) -> Option<Arc<dyn crate::edit::ports::PostWriteDiagnostics + Send + Sync>> {
        let _phase = crate::logging::time_edit_phase(crate::logging::EditPhase::Lsp);
        let wants_diagnostics = params
            .get("diagnostics")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let custom_markers = self.custom_lsp_root_markers();
        if let Some(file_paths) = Self::multi_file_write_paths(params) {
            self.add_pending_tier2_paths(file_paths);
        } else {
            self.add_pending_tier2_paths([file_path.to_path_buf()]);
        }

        if !wants_diagnostics {
            if let Some(file_paths) = Self::multi_file_write_paths(params) {
                self.notify_watched_config_files(&file_paths);
            } else if let Some(config_events) =
                Self::watched_file_events_from_params(params, &custom_markers)
            {
                self.notify_watched_config_events(&config_events);
            }
            self.lsp_notify_file_changed(file_path, content);
            return None;
        }

        let wait_ms = params
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000)
            .min(10_000); // Cap at 10 seconds to prevent hangs from adversarial input

        if let Some(file_paths) = Self::multi_file_write_paths(params) {
            return self
                .lsp_post_multi_file_write(file_path, content, &file_paths, params)
                .map(
                    |outcome| -> Arc<dyn crate::edit::ports::PostWriteDiagnostics + Send + Sync> {
                        Arc::new(outcome)
                    },
                );
        }

        if let Some(config_events) = Self::watched_file_events_from_params(params, &custom_markers)
        {
            self.notify_watched_config_events(&config_events);
        }

        Some(Arc::new(self.lsp_notify_and_collect_diagnostics(
            file_path,
            content,
            std::time::Duration::from_millis(wait_ms),
        )))
    }
}
