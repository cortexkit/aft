use super::*;

impl LspManager {
    pub(crate) fn recheck_unproven_rust_file(&mut self, key: &ServerKey, file: &Path) -> bool {
        let lookup = (key.clone(), normalize_lookup_path(file));
        let Some(hash) = self
            .documents
            .get(key)
            .and_then(|store| store.entry(&lookup.1))
            .and_then(|document| document.content_hash)
        else {
            return false;
        };
        let Some(already_requested) = self.unproven_rust_compiler_reports.get_mut(&lookup) else {
            return false;
        };
        // A temporary close and reopen advances the wire version even when
        // the same file contents still need the result of the same check.
        if *already_requested == Some(hash) {
            return false;
        }
        *already_requested = Some(hash);
        if !self.request_rust_check(key, &lookup.1) {
            return false;
        }
        if let Some(client) = self.clients.get_mut(key) {
            client.make_rust_save_one_shot();
        }
        true
    }

    /// Whether rust-analyzer's latest `cargo check` push for `file` carried
    /// compiler diagnostics that could not be matched to known file content
    /// (see `unproven_rust_compiler_reports`). A pulled report for the file
    /// then lacks those rows and cannot stand for its compiler result.
    pub(crate) fn rust_compiler_report_unproven(
        &self,
        server_key: &ServerKey,
        file_path: &Path,
    ) -> bool {
        self.unproven_rust_compiler_reports
            .contains_key(&(server_key.clone(), normalize_lookup_path(file_path)))
    }

    /// Ask rust-analyzer for a new `cargo check` by telling it that `file`,
    /// a document open in it, was saved. The check publishes its results for
    /// open documents with their versions, which AFT can match to their
    /// content. Until the check begins, [`Self::rust_check_state`] reports
    /// it as running. Returns whether the save was sent.
    pub(crate) fn request_rust_check(&mut self, server_key: &ServerKey, file_path: &Path) -> bool {
        let Ok(content) = read_file_content(file_path) else {
            return false;
        };
        let Ok(uri) = uri_for_path(file_path) else {
            return false;
        };
        let Some(client) = self.clients.get_mut(server_key) else {
            return false;
        };
        if client.save_notification().is_none() {
            return false;
        }
        match send_did_save(client, &uri, &content) {
            Ok(()) => true,
            Err(error) => {
                crate::slog_warn!("didSave for {} failed: {error}", uri.as_str());
                false
            }
        }
    }
}
