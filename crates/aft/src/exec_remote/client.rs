use super::{types::*, *};
use serde::Serialize;
use serde_json::json;
use std::{path::Path, sync::Arc, time::Instant};
use subc_client_rs::{CallOptions, ConsumerOptions, SubcConsumer, SubscribeOptions, Subscription};
use subc_protocol::{BindIdentity, RouteTarget};

/// Capability ck-motor advertises when it accepts Windows jobs. Capability
/// names must have the `<name>/v<N>` shape the subc daemon validates.
pub(crate) const WINDOWS_CAPABILITY: &str = "exec-remote-windows/v1";

pub const CAPABILITY: &str = "exec-remote/v1";

// Agent-visible text, approved by Ufuk; change it only with his approval.
const MEMORY_UNSUPPORTED_REFUSAL: &str =
    "This build server can't take a memory size yet; run the job without one.";

/// Local, monotonic launch timings. Phase labels are static so diagnostics
/// cannot accidentally include command text, paths, or environment values.
pub(crate) struct LaunchTimings {
    started: Instant,
    entered: Instant,
    phase: &'static str,
    phases: Vec<(&'static str, std::time::Duration)>,
}

impl LaunchTimings {
    pub(crate) fn new(started: Instant, phase: &'static str) -> Self {
        Self {
            started,
            entered: started,
            phase,
            phases: Vec::new(),
        }
    }

    pub(crate) fn enter(&mut self, phase: &'static str) {
        let now = Instant::now();
        self.phases
            .push((self.phase, now.duration_since(self.entered)));
        self.entered = now;
        self.phase = phase;
    }

    fn sent(mut self, task_id: &str, session: &str, sent_at: Instant) {
        use std::fmt::Write as _;

        self.phases
            .push((self.phase, sent_at.duration_since(self.entered)));
        let mut phases = String::new();
        for (name, elapsed) in self.phases {
            if !phases.is_empty() {
                phases.push(',');
            }
            let _ = write!(phases, "{name}:{}", elapsed.as_millis());
        }
        crate::slog_info!(
            "runon: exec.run sent task_id={} session={} pre_send_ms={} phases={}",
            task_id,
            session,
            sent_at.duration_since(self.started).as_millis(),
            phases,
        );
    }
}

pub(crate) struct RunDiagnostics<'a> {
    pub task_id: &'a str,
    pub session: &'a str,
    pub timings: LaunchTimings,
}

/// An authenticated daemon route to the catalog's sole capability provider.
/// No module ID fallback, SSH transport, or snapshotting. Only a typed cap
/// refusal before job creation permits one automatic return-cap resubmit.
#[derive(Clone)]
pub struct ExecRemoteClient {
    consumer: Arc<SubcConsumer>,
    route: subc_client_rs::RouteHandle,
    module_id: String,
    /// The capabilities the daemon catalog lists for `module_id`, read by the
    /// first capability check and reused by every later check on this route
    /// (one submission makes several). Recovery after a failed call opens a
    /// new route ([`Self::reopen_route`]) or connection, whose first check
    /// reads the catalog again.
    provided: Arc<std::sync::OnceLock<Arc<[String]>>>,
}

impl ExecRemoteClient {
    pub(crate) async fn return_max_bytes(&self) -> Result<u64, Error> {
        Ok(
            if self
                .advertises(RETURN_LARGE_CAPABILITY)
                .await
                .map_err(Error::Transport)?
            {
                RETURN_LARGE_MAX_BYTES_CAP
            } else {
                super::return_max_bytes()
            },
        )
    }

    pub async fn connect(connection_file: &Path, identity: BindIdentity) -> Result<Self, Error> {
        let consumer =
            crate::fleet_status::connect_subc_consumer(connection_file, ConsumerOptions::default())
                .await
                .map_err(Error::Connection)?;
        Self::from_consumer(Arc::new(consumer), identity).await
    }

    /// Reuse AFT's existing authenticated connection and daemon route machinery.
    pub async fn from_consumer(
        consumer: Arc<SubcConsumer>,
        identity: BindIdentity,
    ) -> Result<Self, Error> {
        let module_id = consumer
            .resolve_provider(CAPABILITY)
            .await
            .map_err(Error::Transport)?;
        let route = consumer
            .open_route(
                RouteTarget::ManagementSurface {
                    module_id: module_id.clone(),
                },
                identity,
                CallOptions {
                    consumer_identity: crate::launch_nonce::consumer_identity(),
                    ..CallOptions::default()
                },
            )
            .await
            .map_err(Error::Transport)?;
        Ok(Self {
            consumer,
            route,
            module_id,
            provided: Arc::default(),
        })
    }

    pub async fn run(&self, request: &RunRequest) -> Result<RemoteStream, Error> {
        self.run_with_diagnostics(request, None).await
    }

    pub(crate) async fn run_with_diagnostics(
        &self,
        request: &RunRequest,
        diagnostics: Option<RunDiagnostics<'_>>,
    ) -> Result<RemoteStream, Error> {
        // Submitting is when the job's inputs are captured, so the
        // allocation tracer names that step for whatever it allocates.
        crate::alloc_trace::future(
            crate::alloc_trace::Label::REMOTE_INPUT_CAPTURE,
            None,
            self.submit_run(request, diagnostics),
        )
        .await
    }

    async fn submit_run(
        &self,
        request: &RunRequest,
        mut diagnostics: Option<RunDiagnostics<'_>>,
    ) -> Result<RemoteStream, Error> {
        if request.submission_key().is_some() || request.resume_from_seq().is_some() {
            if let Some(diagnostics) = diagnostics.as_mut() {
                diagnostics.timings.enter("run_supports_submission_key");
            }
            if !self.supports_submission_key().await? {
                return Err(Error::Protocol(
                    "executor does not advertise submission keys".into(),
                ));
            }
        }
        if request.memory_mib.is_some() {
            if let Some(diagnostics) = diagnostics.as_mut() {
                diagnostics.timings.enter("advertises_memory");
            }
            if !self.advertises(MEMORY_CAPABILITY).await.map_err(|error| {
                Error::Protocol(format!("{MEMORY_UNSUPPORTED_REFUSAL} ({error})"))
            })? {
                return Err(Error::Protocol(MEMORY_UNSUPPORTED_REFUSAL.into()));
            }
        }
        if request.platform == Some(Platform::Windows) {
            if let Some(diagnostics) = diagnostics.as_mut() {
                diagnostics.timings.enter("advertises_windows");
            }
            if !self.advertises(WINDOWS_CAPABILITY).await.map_err(|error| {
                Error::Protocol(format!(
                    "runon \"windows\" capability discovery failed before launch: {error}"
                ))
            })? {
                return Err(Error::Protocol(format!("runon \"windows\" is unavailable: the build server doesn't offer Windows jobs yet (missing {WINDOWS_CAPABILITY})")));
            }
        }
        // A runner without chunked return would run the command and drop the
        // requested files, so refuse before submitting anything.
        if !request.return_paths.is_empty() {
            if let Some(diagnostics) = diagnostics.as_mut() {
                diagnostics.timings.enter("advertises_return_chunked");
            }
            if !self
                .advertises(RETURN_CHUNKED_CAPABILITY)
                .await
                .map_err(|error| {
                    Error::Protocol(format!(
                        "return_paths capability discovery failed before launch: {error}"
                    ))
                })?
            {
                return Err(Error::MissingCapability {
                    feature: "return_paths",
                    capability: RETURN_CHUNKED_CAPABILITY,
                });
            }
        }
        // Progress is optional display data. Older executors receive no opt-in,
        // even if a saved request was created against a newer executor.
        let mut request = request
            .clone()
            .with_progress(self.advertises(PROGRESS_CAPABILITY).await.unwrap_or(false));
        if !request.return_paths.is_empty() {
            let maximum = self.return_max_bytes().await?;
            request.return_max_bytes =
                Some(request.return_max_bytes.unwrap_or(maximum).min(maximum));
        }
        let mut stream = self
            .stream(
                "exec.run",
                &request,
                StreamConsumer::keyed_run(request.resume_from_seq()),
                diagnostics,
            )
            .await?;
        if !request.return_paths.is_empty() {
            stream.return_request = Some(request.clone());
            stream.return_retry = Some((self.clone(), request));
        }
        Ok(stream)
    }

    pub(crate) async fn supports_submission_key(&self) -> Result<bool, Error> {
        self.advertises(SUBMISSION_KEY_CAPABILITY)
            .await
            .map_err(Error::Transport)
    }

    #[cfg(unix)]
    pub(crate) async fn reopen_route(&self, identity: BindIdentity) -> Result<Self, Error> {
        Self::from_consumer(Arc::clone(&self.consumer), identity).await
    }

    /// Whether the module serving this route advertises `capability` in the
    /// daemon catalog.
    async fn advertises(&self, capability: &str) -> Result<bool, subc_client_rs::CallError> {
        Ok(self
            .provided()
            .await?
            .iter()
            .any(|provided| provided == capability))
    }

    /// The capabilities the catalog lists for this route's module, read once
    /// per route (see the `provided` field).
    async fn provided(&self) -> Result<Arc<[String]>, subc_client_rs::CallError> {
        if let Some(provided) = self.provided.get() {
            return Ok(Arc::clone(provided));
        }
        let catalog = self.consumer.catalog_list().await?;
        let mut listed = false;
        let provided: Arc<[String]> = catalog
            .modules
            .iter()
            .filter(|module| module.module_id == self.module_id)
            .inspect(|_| listed = true)
            .filter_map(|module| module.capabilities.as_ref())
            .flat_map(|capabilities| capabilities.provides.iter().cloned())
            .collect();
        if !listed {
            // The module is missing from this listing (it may be registering
            // again): it advertises nothing now, and the next check looks again.
            return Ok(provided);
        }
        Ok(Arc::clone(self.provided.get_or_init(|| provided)))
    }

    /// Resume from the last durably delivered seq, not from the last received
    /// packet. `from_seq` on the wire is inclusive, so this sends last_seq + 1.
    pub async fn attach(&self, point: ResumePoint) -> Result<RemoteStream, Error> {
        let request = point.attach_request()?;
        self.stream("exec.attach", &request, StreamConsumer::resume(point), None)
            .await
    }

    async fn stream<T: Serialize>(
        &self,
        method: &str,
        params: &T,
        consumer: StreamConsumer,
        mut diagnostics: Option<RunDiagnostics<'_>>,
    ) -> Result<RemoteStream, Error> {
        if let Some(diagnostics) = diagnostics.as_mut() {
            diagnostics.timings.enter("send");
        }
        let subscription = self
            .consumer
            .subscribe_route(
                &self.route,
                envelope(method, params)?,
                SubscribeOptions::default(),
            )
            .await
            .map_err(Error::Transport)?;
        let sent_at = Instant::now();
        if let Some(diagnostics) = diagnostics {
            diagnostics
                .timings
                .sent(diagnostics.task_id, diagnostics.session, sent_at);
        }
        Ok(RemoteStream {
            subscription,
            consumer,
            finished: false,
            received_frame: false,
            attached: method == "exec.attach",
            sent_at: (method == "exec.run").then_some(sent_at),
            return_request: None,
            return_retry: None,
        })
    }

    /// Cancel on the active call's key, then attach to observe the terminal.
    /// The SDK's unsubscribe sends Cancel but closes its local receiver, so the
    /// cancellation is never graded from that closure or from an acknowledgement.
    pub async fn cancel(&self, stream: &RemoteStream) -> Result<RemoteStream, Error> {
        stream
            .subscription
            .unsubscribe()
            .map_err(Error::Transport)?;
        let point = stream.resume_point().ok_or_else(|| {
            stream.consumer.recovery(
                "cancel sent before the job ID was received; the job cannot be re-attached",
            )
        })?;
        self.attach(point).await
    }

    /// Idempotent cancellation by job ID after a caller restart or detach.
    /// The acknowledgement is not a terminal; follow it with `attach`.
    pub async fn cancel_job(&self, job_id: Uuid) -> Result<CancelReply, Error> {
        match self
            .unary("exec.cancel", &CancelRequest::new(job_id))
            .await?
        {
            Reply::Cancel(reply) => Ok(reply),
            _ => unreachable!(),
        }
    }
    pub async fn status(&self) -> Result<StatusReply, Error> {
        match self.unary("exec.status", &StatusRequest::new()).await? {
            Reply::Status(reply) => Ok(reply),
            _ => unreachable!(),
        }
    }
    pub async fn prepare(&self, request: &PrepareRequest) -> Result<PrepareReply, Error> {
        match self.unary("workspace.prepare", request).await? {
            Reply::Prepare(reply) => Ok(reply),
            _ => unreachable!(),
        }
    }
    pub async fn drop_workspace(&self, request: &DropRequest) -> Result<DropReply, Error> {
        match self.unary("workspace.drop", request).await? {
            Reply::Drop(reply) => Ok(reply),
            _ => unreachable!(),
        }
    }
    async fn unary<T: Serialize>(&self, method: &str, params: &T) -> Result<Reply, Error> {
        let bytes = self
            .consumer
            .request(
                &self.route,
                envelope(method, params)?,
                CallOptions::default(),
            )
            .await
            .map_err(Error::Transport)?;
        decode_reply(method, &bytes)
    }
}

fn envelope<T: Serialize>(method: &str, params: &T) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(&json!({ "method": method, "params": params }))
        .map_err(|e| Error::Protocol(e.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamProgress {
    Record,
    AdmissionProgress,
    Complete(Verdict),
}

/// One held-open run/attach observation. Dropping it sends request-level Cancel
/// (SDK semantics), which detaches at the executor, unlike `exec.cancel` by job
/// ID. Persist its resume point before dropping when recovery is needed.
pub struct RemoteStream {
    subscription: Subscription,
    consumer: StreamConsumer,
    finished: bool,
    received_frame: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    attached: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    sent_at: Option<Instant>,
    return_request: Option<RunRequest>,
    return_retry: Option<(ExecRemoteClient, RunRequest)>,
}

impl RemoteStream {
    #[cfg(unix)]
    pub(crate) fn is_attach(&self) -> bool {
        self.attached
    }

    #[cfg(unix)]
    pub(crate) fn sent_at(&self) -> Option<Instant> {
        self.sent_at
    }

    pub fn resume_point(&self) -> Option<ResumePoint> {
        self.consumer.resume_point()
    }

    /// StreamEnd and transport errors alone do not prove the job still exists.
    // The background remote worker currently runs only on Unix.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn received_frame(&self) -> bool {
        self.received_frame
    }

    /// Read one record into a sink; admission progress is display-only. Outside
    /// output-gap recovery, a known terminal followed by clean StreamEnd is
    /// required to complete a run; loss/decode errors require status/attach.
    /// During output-gap recovery, the
    /// saved executor terminal still supplies the outcome even if a later attach
    /// closes with an error or does not repeat that terminal.
    /// There is no response deadline here: request timeout bounds remote run
    /// time only, and the executor owns queue waiting and reconnects to runner.
    pub async fn next<S: OutputSink>(&mut self, sink: &mut S) -> Result<StreamProgress, Error> {
        loop {
            if let Some(request) = self.return_request.take() {
                sink.return_request(&request, false)?;
            }
            if self.finished {
                return self.consumer.finish().map(StreamProgress::Complete);
            }
            if let Some(bytes) = self.subscription.events().recv().await {
                match self.consumer.consume_frame(&bytes, sink) {
                    Ok(false) => return Ok(StreamProgress::AdmissionProgress),
                    Ok(true) => self.received_frame = true,
                    Err(error) => {
                        self.received_frame = true;
                        return Err(self.consumer.recovery(error.to_string()));
                    }
                }
                return Ok(StreamProgress::Record);
            }
            if let Err(error) = self.subscription.closed().await {
                // Only this typed submit refusal proves no job was created. A
                // record (including acceptance) rules out resubmitting the call.
                if !self.received_frame && self.consumer.known_job_id().is_none() {
                    if let Some((client, request)) = self.return_retry.take() {
                        if let Some(maximum) = refused_return_cap(&error, &request) {
                            let request = request.with_return_max_bytes(maximum);
                            if sink.return_request(&request, true)? {
                                self.subscription = client
                                    .consumer
                                    .subscribe_route(
                                        &client.route,
                                        envelope("exec.run", &request)?,
                                        SubscribeOptions::default(),
                                    )
                                    .await
                                    .map_err(Error::Transport)?;
                                self.sent_at = Some(Instant::now());
                                continue;
                            }
                        }
                    }
                    if matches!(&error, subc_client_rs::CallError::Module(body) if body.code == "invalid_params")
                    {
                        return Err(Error::Transport(error));
                    }
                }
                if self
                    .consumer
                    .resume_point()
                    .is_some_and(|p| p.gap_recovery.is_some())
                {
                    return self
                        .consumer
                        .finish_recovery(sink)
                        .map(StreamProgress::Complete);
                }
                return Err(self.consumer.recovery(error.to_string()));
            }
            self.finished = true;
            return self
                .consumer
                .finish_recovery(sink)
                .map(StreamProgress::Complete);
        }
    }
}

fn refused_return_cap(error: &subc_client_rs::CallError, request: &RunRequest) -> Option<u64> {
    let subc_client_rs::CallError::Module(body) = error else {
        return None;
    };
    if body.code != "invalid_params" || request.return_paths.is_empty() {
        return None;
    }
    let detail: ReturnCapRefusal = serde_json::from_value(body.detail.clone()?).ok()?;
    let maximum = detail.max_bytes()?;
    (detail.field() == "return_max_bytes" && maximum < request.return_max_bytes?).then_some(maximum)
}

#[cfg(test)]
mod capability_tests {
    use super::{
        CAPABILITY, MEMORY_CAPABILITY, PROGRESS_CAPABILITY, RETURN_CHUNKED_CAPABILITY,
        RETURN_LARGE_CAPABILITY, WINDOWS_CAPABILITY,
    };

    /// The daemon refuses a HELLO whose manifest names a capability outside the
    /// `<name>/v<N>` grammar, so AFT must check for names that can exist on the wire.
    #[test]
    fn checked_capabilities_pass_the_daemon_grammar() {
        for capability in [
            CAPABILITY,
            MEMORY_CAPABILITY,
            WINDOWS_CAPABILITY,
            RETURN_CHUNKED_CAPABILITY,
            RETURN_LARGE_CAPABILITY,
            PROGRESS_CAPABILITY,
        ] {
            assert!(
                subc_protocol::manifest::is_valid_capability_identifier(capability),
                "{capability} is not a capability name the daemon admits"
            );
        }
    }
}
