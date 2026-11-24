use super::*;
use crate::lsp::client::PendingLspRequest;
use crate::lsp::diagnostics::{ProviderResults, ProviderStamp};
use lsp_types::{DocumentDiagnosticReport, DocumentDiagnosticReportResult};

pub(super) struct RegisteredPulls {
    instance: ServerInstanceId,
    wait_until: Instant,
    pulls: Vec<(u64, ProviderPull)>,
}

enum ProviderPull {
    InFlight(PendingLspRequest),
    Replied(Result<serde_json::Value, LspError>),
}

pub(super) fn request_deadline(deadline: Option<Instant>, timeout: Duration) -> Instant {
    let limit = Instant::now() + timeout;
    deadline.map_or(limit, |deadline| deadline.min(limit))
}

impl RegisteredPulls {
    pub(super) fn new(
        instance: ServerInstanceId,
        requests: Vec<(u64, Result<PendingLspRequest, LspError>)>,
        wait_until: Instant,
    ) -> Self {
        let pulls = requests
            .into_iter()
            .map(|(id, request)| {
                (
                    id,
                    match request {
                        Ok(request) => ProviderPull::InFlight(request),
                        Err(error) => ProviderPull::Replied(Err(error)),
                    },
                )
            })
            .collect();
        Self {
            instance,
            wait_until,
            pulls,
        }
    }

    pub(super) fn wait(mut self) -> Self {
        self.pulls = self
            .pulls
            .into_iter()
            .map(|(id, pull)| {
                (
                    id,
                    match pull {
                        ProviderPull::InFlight(request) => ProviderPull::Replied(
                            request.wait(self.wait_until.saturating_duration_since(Instant::now())),
                        ),
                        settled => settled,
                    },
                )
            })
            .collect();
        self
    }
}

impl LspManager {
    pub(super) fn begin_open_document_pull(
        &mut self,
        key: ServerKey,
        canonical_path: &Path,
        uri: &lsp_types::Uri,
        deadline: Option<Instant>,
    ) -> DocumentPull {
        let canonical_path = canonical_path.to_path_buf();
        self.background_pull_pending
            .remove(&(key.clone(), canonical_path.clone()));
        let document_version = self
            .documents
            .get(&key)
            .and_then(|store| store.version(&canonical_path));
        let mut pull = DocumentPull {
            document_version,
            document_snapshot: self
                .documents
                .get(&key)
                .and_then(|store| store.entry(&canonical_path))
                .cloned(),
            server_pid: self.clients.get(&key).map(LspClient::child_pid),
            previous_report: self
                .diagnostics
                .entries_for_file(&canonical_path)
                .into_iter()
                .find(|(stored, entry)| {
                    **stored == key
                        && entry.from_document_pull
                        && (entry.result_id.is_some() || entry.provider_results.is_some())
                })
                .map(|(_, entry)| entry.clone()),
            key,
            canonical_path,
            opened_for_pull: None,
            state: DocumentPullState::Done(PullFileOutcome::PullNotSupported),
        };
        if self.starting.contains_key(&pull.key) {
            pull.state = DocumentPullState::Replied(Err(LspError::ServerNotReady(
                START_IN_PROGRESS_REASON.into(),
            )));
            return pull;
        }
        if let Some(client) = self.clients.get_mut(&pull.key) {
            let requests = client.start_registered_document_pulls(
                &pull.canonical_path,
                uri,
                pull.previous_report
                    .as_ref()
                    .and_then(|entry| entry.provider_results.as_ref()),
            );
            if !requests.is_empty() {
                let wait_until = request_deadline(deadline, Self::PULL_FILE_TIMEOUT);
                pull.state = DocumentPullState::Registered(RegisteredPulls::new(
                    client.instance,
                    requests,
                    wait_until,
                ));
                return pull;
            }
        }
        let supports_pull = self
            .clients
            .get(&pull.key)
            .and_then(|c| c.diagnostic_capabilities_for_file(&pull.canonical_path))
            .is_some_and(|caps| caps.pull_diagnostics);
        if !supports_pull {
            return pull;
        }

        // Look up previous resultId for incremental requests.
        let previous_result_id = pull
            .previous_report
            .as_ref()
            .and_then(|entry| entry.result_id.clone());
        let identifier = self
            .clients
            .get(&pull.key)
            .and_then(|c| c.diagnostic_capabilities_for_file(&pull.canonical_path))
            .and_then(|caps| caps.identifier.clone());
        let params = AftDocumentDiagnosticParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            identifier,
            previous_result_id,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        // Capped below the global request timeout so a stalled pull server
        // cannot consume the entire inspect or post-edit budget.
        let timeout = deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Self::PULL_FILE_TIMEOUT)
            })
            .unwrap_or(Self::PULL_FILE_TIMEOUT);
        slog_info!(
            "lsp_protocol server={} root={} method=textDocument/diagnostic event=sent file={} document_version={}",
            pull.key.kind.id_str(),
            pull.key.root.display(),
            pull.canonical_path.display(),
            pull.document_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "none".to_string())
        );
        let started = match self.clients.get_mut(&pull.key) {
            Some(client) => client.start_document_diagnostic_request(params),
            None => Err(LspError::ServerNotReady("server not found".into())),
        };
        pull.state = match started {
            Ok((request, used_previous)) => {
                if !used_previous {
                    pull.previous_report = None;
                }
                DocumentPullState::InFlight {
                    request,
                    wait_until: Instant::now() + timeout,
                }
            }
            Err(err) => DocumentPullState::Replied(Err(err)),
        };
        pull
    }

    pub(super) fn invalidate_changed_providers(&mut self, key: &ServerKey) {
        if let Some(client) = self.clients.get(key) {
            if !client.uses_document_pull(None) {
                self.diagnostics.invalidate_document_pulls(key);
            } else {
                self.diagnostics
                    .invalidate_changed_provider_results(key, |file| {
                        client.registered_document_provider_generations(file)
                    });
            }
        }
    }

    pub(super) fn finish_registered_document_pulls(
        &mut self,
        key: &ServerKey,
        file: &Path,
        document: Option<&DocumentEntry>,
        previous: Option<&DiagnosticEntry>,
        pulls: RegisteredPulls,
    ) -> PullFileOutcome {
        // Apply queued pushes first. They must not overwrite a complete pull
        // for the same open document just because they arrived earlier.
        let drained = self.drain_events_inner(usize::MAX, true);
        self.pending_diagnostic_observations
            .extend(drained.accepted_snapshots);
        let Some(client) = self
            .clients
            .get(key)
            .filter(|client| client.instance == pulls.instance)
        else {
            return failed("document_changed_while_pulling");
        };
        if self.documents.get(key).and_then(|store| store.entry(file)) != document {
            return failed("document_changed_while_pulling");
        }
        let generations = client.registered_document_provider_generations(file);
        if !generations
            .into_iter()
            .eq(pulls.pulls.iter().map(|(generation, _)| *generation))
        {
            self.diagnostics.mark_stale_for_server_file(key, file);
            return failed("document_diagnostics_not_registered");
        }
        let previous = previous
            .zip(previous.and_then(|p| p.provider_results.as_ref()))
            .filter(|(_, results)| results.instance == pulls.instance);
        let provisional = client.diagnostics_are_provisional() && !client.uses_analysis_progress();
        let mut items = Vec::new();
        let mut stamps = Vec::new();
        let mut unchanged = true;
        for (generation, pull) in pulls.pulls {
            let ProviderPull::Replied(reply) = pull else {
                unreachable!("pull was waited")
            };
            let report = reply.and_then(|value| {
                serde_json::from_value::<DocumentDiagnosticReportResult>(value).map_err(Into::into)
            });
            let start = items.len();
            let result_id = match report {
                Ok(DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
                    report,
                ))) => {
                    unchanged = false;
                    let full = report.full_document_diagnostic_report;
                    items.extend(from_lsp_diagnostics(
                        file.to_path_buf(),
                        full.items,
                        &key.kind,
                    ));
                    full.result_id
                }
                Ok(DocumentDiagnosticReportResult::Report(
                    DocumentDiagnosticReport::Unchanged(report),
                )) => {
                    let Some(saved) = previous.and_then(|(entry, results)| {
                        results
                            .stamps
                            .iter()
                            .find(|stamp| {
                                stamp.generation == generation && stamp.result_id.is_some()
                            })
                            .and_then(|stamp| entry.diagnostics.get(stamp.items.clone()))
                    }) else {
                        self.diagnostics.mark_stale_for_server_file(key, file);
                        return failed("no_cache_for_unchanged");
                    };
                    items.extend_from_slice(saved);
                    Some(report.unchanged_document_diagnostic_report.result_id)
                }
                Ok(DocumentDiagnosticReportResult::Partial(_)) => {
                    self.diagnostics.mark_stale_for_server_file(key, file);
                    return PullFileOutcome::PartialNotSupported;
                }
                Err(err) => {
                    self.diagnostics.mark_stale_for_server_file(key, file);
                    return self.cache_post_initialize_exit(key, &err).map_or_else(
                        || failed(err.to_string()),
                        |result| failed(server_attempt_result_reason(&result)),
                    );
                }
            };
            stamps.push(ProviderStamp {
                generation,
                result_id,
                items: start..items.len(),
            });
        }
        let diagnostic_count = items.len();
        let document_version = document.map(|entry| entry.version);
        self.diagnostics.publish_provider_results(
            key.clone(),
            file.to_path_buf(),
            items,
            document_version,
            provisional,
            ProviderResults {
                instance: pulls.instance,
                stamps,
            },
        );
        self.note_document_pull_completed(key, file);
        self.certify_report_content(key, file, document_version);
        if unchanged {
            PullFileOutcome::Unchanged
        } else {
            PullFileOutcome::Full { diagnostic_count }
        }
    }
}

fn failed(reason: impl Into<String>) -> PullFileOutcome {
    PullFileOutcome::RequestFailed {
        reason: reason.into(),
    }
}
