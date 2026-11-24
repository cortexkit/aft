use super::*;

impl LspManager {
    pub(super) fn notify_file_changed_for_server_keys(
        &mut self,
        canonical_path: PathBuf,
        content: &str,
        server_keys: Vec<ServerKey>,
    ) -> Result<Vec<(ServerKey, i32)>, LspError> {
        if server_keys.is_empty() {
            return Ok(Vec::new());
        }
        self.end_inspect_close_exception_on_reopen(&canonical_path);

        let uri = uri_for_path(&canonical_path)?;
        let language_id = language_id_for_path(&canonical_path, Some(content)).to_string();

        let mut versions: Vec<(ServerKey, i32)> = Vec::with_capacity(server_keys.len());
        let mut first_error = None;
        // Read once for every server's store (see `ensure_file_open`).
        let disk = DiskSnapshot::for_content(&canonical_path, content);

        for key in server_keys {
            if self.starting.contains_key(&key) {
                match self.queue_startup_notification(&key, &canonical_path) {
                    Ok(version) => versions.push((key, version)),
                    Err(err) => {
                        first_error.get_or_insert(err);
                    }
                }
                continue;
            }
            // Both pushed and pulled reports lose authority before text sync.
            self.invalidate_diagnostic_content(&key, &canonical_path);
            let current_version = self
                .documents
                .get(&key)
                .and_then(|store| store.version(&canonical_path));

            if let Some(version) = current_version {
                let next_version = version + 1;
                let mut sync_order = None;
                if let Some(client) = self.clients.get_mut(&key) {
                    let send = client.send_full_did_change_ordered(&uri, next_version, content)
                    // AFT has written this content to disk: tell the server
                    // it was saved. rust-analyzer re-runs `cargo check` only
                    // on `didSave`, so without it the compiler errors it
                    // reports keep describing the file before the edit.
                        .and_then(|order| send_did_save(client, &uri, content).map(|()| order));
                    sync_order = send.as_ref().ok().copied();
                    if let Err(err) = send {
                        if let Some(store) = self.documents.get_mut(&key) {
                            store.bump_version_with(&canonical_path, &DiskSnapshot::default());
                        }
                        first_error.get_or_insert(err);
                        continue;
                    }
                }
                if let Some(store) = self.documents.get_mut(&key) {
                    store.bump_version_with(
                        &canonical_path,
                        &disk.clone().with_sync_order(sync_order),
                    );
                }
                self.queue_background_document_pull(&key, &canonical_path);
                versions.push((key, next_version));
                continue;
            }

            let version = self
                .documents
                .get(&key)
                .map_or(0, DocumentStore::next_open_version);
            let mut sync_order = None;
            if let Some(client) = self.clients.get_mut(&key) {
                let send = client.send_notification_ordered::<DidOpenTextDocument>(
                    DidOpenTextDocumentParams {
                        text_document: TextDocumentItem::new(
                            uri.clone(),
                            language_id.clone(),
                            version,
                            content.to_string(),
                        ),
                    },
                );
                sync_order = send.as_ref().ok().copied();
                if let Err(err) = send {
                    if matches!(err, LspError::Timeout(_)) {
                        self.documents
                            .entry(key.clone())
                            .or_default()
                            .open_with(canonical_path.clone(), &DiskSnapshot::default());
                    }
                    first_error.get_or_insert(err);
                    continue;
                }
                log_did_open_sent(&key, &canonical_path, &language_id, version);
                // The content was just written to disk: tell the server it
                // was saved, so rust-analyzer re-runs `cargo check`.
                if let Err(err) = send_did_save(client, &uri, content) {
                    self.documents
                        .entry(key.clone())
                        .or_default()
                        .open_with(canonical_path.clone(), &DiskSnapshot::default());
                    first_error.get_or_insert(err);
                    continue;
                }
            }
            self.documents.entry(key.clone()).or_default().open_with(
                canonical_path.clone(),
                &disk.clone().with_sync_order(sync_order),
            );
            self.queue_background_document_pull(&key, &canonical_path);
            versions.push((key, version));
        }

        first_error.map_or(Ok(versions), Err)
    }
}
