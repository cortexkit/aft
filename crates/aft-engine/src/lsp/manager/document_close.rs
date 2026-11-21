use super::*;

impl LspManager {
    /// Close a document only in the specified servers.
    ///
    /// Scoped inspection uses this to release documents it opened without
    /// disturbing pre-existing editor documents in other server stores.
    pub(crate) fn close_file_for_servers(
        &mut self,
        file_path: &Path,
        server_keys: &[ServerKey],
    ) -> Result<(), LspError> {
        self.close_documents(file_path, server_keys, false)
    }

    fn close_documents(
        &mut self,
        file_path: &Path,
        server_keys: &[ServerKey],
        keep_diagnostics: bool,
    ) -> Result<(), LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let uri = uri_for_path(&canonical_path)?;
        let mut first_error = None;

        for key in server_keys {
            let was_open = self
                .documents
                .get(key)
                .is_some_and(|store| store.is_open(&canonical_path));
            if !was_open {
                continue;
            }

            if let Some(client) = self.clients.get_mut(key) {
                if let Err(err) =
                    client.send_notification::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                    })
                {
                    let was_queued = matches!(err, LspError::Timeout(_));
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                    if !was_queued {
                        // A pre-queue rejection leaves the server's document
                        // open. Keep local state so a later retry sends didClose
                        // rather than a duplicate didOpen.
                        continue;
                    }
                }
                if keep_diagnostics {
                    self.note_inspect_closed_document(key, &canonical_path);
                }
            }

            if let Some(store) = self.documents.get_mut(key) {
                store.close(&canonical_path);
            }
            if !keep_diagnostics {
                self.diagnostics.clear_for_server_file(key, &canonical_path);
                // Clear earlier scoped-close bookkeeping that would suppress
                // the empty report from this close.
                self.late_close_replies
                    .remove(&(key.clone(), canonical_path.clone()));
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Close documents a scoped inspect opened with
    /// [`Self::open_document_for_servers`]. The diagnostics collected while
    /// they were open stay in the store: they are the latest analysis of the
    /// file. A later change makes them stale: an AFT edit marks a pulled
    /// report stale when it sends the new text, the watcher marks every
    /// report stale on an external edit inside the project, and the next
    /// scoped inspect re-sends a document that changed on disk (see
    /// [`Self::resync_drifted_document`]). The server's reply to the close is
    /// ignored (see `inspect_closed_documents`).
    pub(crate) fn close_inspect_documents(
        &mut self,
        file_path: &Path,
        server_keys: &[ServerKey],
    ) -> Result<(), LspError> {
        self.close_documents(file_path, server_keys, true)
    }

    /// Close the documents a file pull opened only to ask about them (see
    /// [`PullFileResult::opened_for_pull`]), keeping the diagnostics they
    /// produced, as a scoped inspect does with
    /// [`Self::close_inspect_documents`]. A document AFT edits stays open:
    /// an edit opens it through `didOpen`/`didChange`, not through a pull.
    /// A document reopened or edited since the pull is left open.
    pub fn close_documents_opened_for_pulls(
        &mut self,
        file_path: &Path,
        results: &[PullFileResult],
    ) {
        let Ok(canonical) = canonicalize_for_lsp(file_path) else {
            return;
        };
        let keys: Vec<ServerKey> = results
            .iter()
            .filter_map(|result| {
                let (instance, version) = result.opened_for_pull?;
                let key = &result.server_key;
                let same_instance = self.clients.get(key)?.instance == instance;
                let same_version = self.documents.get(key)?.version(&canonical) == Some(version);
                (same_instance && same_version).then(|| key.clone())
            })
            .collect();
        if keys.is_empty() {
            return;
        }
        if let Err(error) = self.close_inspect_documents(file_path, &keys) {
            crate::slog_warn!(
                "closing {} after a diagnostics pull failed: {error}",
                file_path.display()
            );
        }
    }
}
