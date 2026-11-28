use super::*;

impl LspManager {
    /// Issue a `workspace/diagnostic` request to a specific server. Cancels
    /// internally if `timeout` elapses before the server responds. Cached
    /// entries from the response are stored so directory-mode queries pick
    /// them up. Waits with `self` borrowed; callers holding the manager lock
    /// should use [`pull_workspace_diagnostics_unlocked`].
    pub fn pull_workspace_diagnostics(
        &mut self,
        server_key: &ServerKey,
        timeout: Option<std::time::Duration>,
    ) -> Result<PullWorkspaceResult, LspError> {
        let timeout = timeout.unwrap_or(Self::PULL_WORKSPACE_TIMEOUT);
        match self.begin_workspace_pull(server_key)? {
            None => Ok(unsupported_workspace_pull(server_key)),
            Some(request) => {
                let reply = request.wait(timeout);
                self.finish_workspace_pull(server_key, reply)
            }
        }
    }

    /// Send a `workspace/diagnostic` request; `None` when the server does not
    /// support it.
    pub(crate) fn begin_workspace_pull(
        &mut self,
        server_key: &ServerKey,
    ) -> Result<Option<PendingWorkspacePull>, LspError> {
        self.deliver_pending_watched_backlog();
        if self.starting.contains_key(server_key) {
            return Err(LspError::ServerNotReady(START_IN_PROGRESS_REASON.into()));
        }
        let supports_workspace = self
            .clients
            .get(server_key)
            .and_then(|c| c.diagnostic_capabilities())
            .is_some_and(|caps| caps.workspace_diagnostics);
        if !supports_workspace {
            return Ok(None);
        }

        let identifier = self
            .clients
            .get(server_key)
            .and_then(|c| c.diagnostic_capabilities())
            .and_then(|caps| caps.identifier.clone());

        // Report the result ids of current reports so the server can answer
        // "unchanged" for those files instead of resending every report;
        // an unchanged item leaves the stored report as it is.
        let previous_result_ids: Vec<_> = self
            .diagnostics
            .result_ids_for_server(server_key)
            .into_iter()
            .filter_map(|(file, value)| {
                Some(lsp_types::PreviousResultId {
                    uri: uri_for_path(&file).ok()?,
                    value,
                })
            })
            .collect();
        let sent_result_ids = previous_result_ids
            .iter()
            .filter_map(|previous| Some((uri_to_path(&previous.uri)?, previous.value.clone())))
            .collect();
        let params = AftWorkspaceDiagnosticParams {
            identifier,
            previous_result_ids,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let client = self
            .clients
            .get_mut(server_key)
            .ok_or_else(|| LspError::ServerNotReady("server not found".into()))?;
        let watched_epoch = (client.instance, client.watched_epoch);
        client
            .start_request::<AftWorkspaceDiagnosticRequest>(params)
            .map(|request| {
                Some(PendingWorkspacePull {
                    watched_epoch,
                    request,
                    previous_result_ids: sent_result_ids,
                })
            })
    }

    /// Store the reply to a request sent by [`Self::begin_workspace_pull`].
    pub(crate) fn finish_workspace_pull(
        &mut self,
        server_key: &ServerKey,
        reply: WorkspacePullReply,
    ) -> Result<PullWorkspaceResult, LspError> {
        self.deliver_pending_watched_backlog();
        let WorkspacePullReply {
            watched_epoch,
            reply,
            previous_result_ids,
        } = reply;
        if watched_epoch.is_some_and(|epoch| self.watched_epoch(server_key) != Some(epoch)) {
            return Err(LspError::ServerNotReady("server not found".into()));
        }
        let result = match reply.and_then(|value| {
            serde_json::from_value::<lsp_types::WorkspaceDiagnosticReportResult>(value)
                .map_err(Into::into)
        }) {
            Ok(result) => result,
            Err(LspError::Timeout(_)) => {
                return Ok(PullWorkspaceResult {
                    server_key: server_key.clone(),
                    files_reported: Vec::new(),
                    complete: false,
                    cancelled: true,
                    supports_workspace: true,
                });
            }
            Err(err) => {
                if let Some(result) = self.cache_post_initialize_exit(server_key, &err) {
                    return Err(LspError::ServerNotReady(server_attempt_result_reason(
                        &result,
                    )));
                }
                return Err(err);
            }
        };

        // Extract the items list. Partial responses are not a complete
        // workspace view, but the partial payload can still contain useful
        // document reports; ingest those while surfacing complete=false.
        let (items, mut complete) = match result {
            lsp_types::WorkspaceDiagnosticReportResult::Report(report) => (report.items, true),
            lsp_types::WorkspaceDiagnosticReportResult::Partial(partial) => (partial.items, false),
        };

        // Workspace and document pulls share version/authority rules. Only a
        // document request completes the per-document analysis wait below.
        let mut files_reported = Vec::with_capacity(items.len());
        for item in items {
            let (uri, version, report) = match item {
                lsp_types::WorkspaceDocumentDiagnosticReport::Full(full) => (
                    full.uri,
                    full.version,
                    lsp_types::DocumentDiagnosticReportKind::Full(
                        full.full_document_diagnostic_report,
                    ),
                ),
                lsp_types::WorkspaceDocumentDiagnosticReport::Unchanged(unchanged) => (
                    unchanged.uri,
                    unchanged.version,
                    lsp_types::DocumentDiagnosticReportKind::Unchanged(
                        unchanged.unchanged_document_diagnostic_report,
                    ),
                ),
            };
            if let Some(file) = uri_to_path(&uri) {
                let previous = self
                    .diagnostics
                    .entries_for_file(&file)
                    .into_iter()
                    .find(|(stored, _)| **stored == *server_key)
                    .map(|(_, entry)| entry.clone());
                if matches!(
                    &report,
                    lsp_types::DocumentDiagnosticReportKind::Unchanged(_)
                ) && !previous_result_ids.get(&file).is_some_and(|sent| {
                    previous.as_ref().and_then(|entry| entry.result_id.as_ref()) == Some(sent)
                }) {
                    // An unchanged verdict concerns the report sent with the
                    // request, not a replacement published while it was in flight.
                    complete = false;
                    continue;
                }
                self.ingest_document_report_kind(
                    server_key,
                    &file,
                    version.and_then(|version| i32::try_from(version).ok()),
                    previous,
                    false,
                    report,
                );
                files_reported.push(file);
            }
        }

        Ok(PullWorkspaceResult {
            server_key: server_key.clone(),
            files_reported,
            complete,
            cancelled: false,
            supports_workspace: true,
        })
    }

    pub(super) fn cache_post_initialize_exit(
        &mut self,
        key: &ServerKey,
        err: &LspError,
    ) -> Option<ServerAttemptResult> {
        let binary = self
            .server_binaries
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.kind.id_str().to_string());
        let (instance, report) = {
            let client = self.clients.get_mut(key)?;
            let status = client.wait_for_exit(Duration::from_millis(100))?;
            (
                client.instance,
                client.exit_report(client.phase(), Some(status), false),
            )
        };
        let reason = format_post_initialize_exit_reason(&binary, &report, err);
        let durability = exit_durability(&reason, &report.stderr_tail);
        let result = ServerAttemptResult::SpawnFailed { binary, reason };
        self.clients.remove(key);
        self.remember_exit_report(instance, report);
        self.forget_server_state(Some(key));
        Some(self.record_failed_spawn(key, result, durability))
    }
}
