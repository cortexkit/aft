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
/// No module ID fallback, SSH transport, snapshotting, or automatic run retry.
pub struct ExecRemoteClient {
    consumer: Arc<SubcConsumer>,
    route: subc_client_rs::RouteHandle,
    module_id: String,
}

impl ExecRemoteClient {
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
        })
    }

    pub async fn run(&self, request: &RunRequest) -> Result<RemoteStream, Error> {
        self.run_with_diagnostics(request, None).await
    }

    pub(crate) async fn run_with_diagnostics(
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
        let request = request
            .clone()
            .with_progress(self.advertises(PROGRESS_CAPABILITY).await.unwrap_or(false));
        self.stream(
            "exec.run",
            &request,
            StreamConsumer::keyed_run(request.resume_from_seq()),
            diagnostics,
        )
        .await
    }

    pub(crate) async fn supports_submission_key(&self) -> Result<bool, Error> {
        let catalog = self
            .consumer
            .catalog_list()
            .await
            .map_err(Error::Transport)?;
        Ok(catalog.modules.iter().any(|module| {
            module.module_id == self.module_id
                && module.capabilities.as_ref().is_some_and(|capabilities| {
                    capabilities
                        .provides
                        .iter()
                        .any(|capability| capability == SUBMISSION_KEY_CAPABILITY)
                })
        }))
    }

    #[cfg(unix)]
    pub(crate) async fn reopen_route(&self, identity: BindIdentity) -> Result<Self, Error> {
        Self::from_consumer(Arc::clone(&self.consumer), identity).await
    }

    /// Whether the module serving this route advertises `capability` in the
    /// daemon catalog.
    async fn advertises(&self, capability: &str) -> Result<bool, subc_client_rs::CallError> {
        let catalog = self.consumer.catalog_list().await?;
        Ok(catalog.modules.iter().any(|module| {
            module.module_id == self.module_id
                && module.capabilities.as_ref().is_some_and(|capabilities| {
                    capabilities
                        .provides
                        .iter()
                        .any(|provided| provided == capability)
                })
        }))
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
            sent_at: (method == "exec.run").then_some(sent_at),
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

    /// Attempt call-key cancellation even before Accepted assigns a job ID.
    /// Unsubscribe closes the local receiver and sends Cancel best-effort;
    /// a subsequent status round trip keeps the transport alive to send it
    /// before the task's runtime shuts down. Neither sending Cancel nor receiving
    /// the status reply proves termination.
    /// Without a job ID, terminal recovery still has to report unknown.
    #[cfg(unix)]
    pub(crate) async fn cancel_submission(&self, stream: &RemoteStream) -> Result<(), Error> {
        stream
            .subscription
            .unsubscribe()
            .map_err(Error::Transport)?;
        self.status().await.map(|_| ())
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

/// One held-open run/attach call. Dropping it sends Cancel (SDK semantics), not
/// a detach. Persist its resume point before dropping when recovery is needed.
pub struct RemoteStream {
    subscription: Subscription,
    consumer: StreamConsumer,
    finished: bool,
    received_frame: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    sent_at: Option<Instant>,
}

impl RemoteStream {
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

    /// Read one record into a durable sink. Outside output-gap recovery, a known
    /// terminal followed by clean StreamEnd is required to complete a run;
    /// loss/decode errors require status/attach. During output-gap recovery, the
    /// saved executor terminal still supplies the outcome even if a later attach
    /// closes with an error or does not repeat that terminal.
    /// There is no response deadline here: request timeout bounds remote run
    /// time only, and the executor owns queue waiting and reconnects to runner.
    pub async fn next<S: OutputSink>(&mut self, sink: &mut S) -> Result<StreamProgress, Error> {
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
        self.consumer
            .finish_recovery(sink)
            .map(StreamProgress::Complete)
    }
}

#[cfg(test)]
mod capability_tests {
    use super::{CAPABILITY, PROGRESS_CAPABILITY, RETURN_CHUNKED_CAPABILITY, WINDOWS_CAPABILITY};

    /// The daemon refuses a HELLO whose manifest names a capability outside the
    /// `<name>/v<N>` grammar, so AFT must check for names that can exist on the wire.
    #[test]
    fn checked_capabilities_pass_the_daemon_grammar() {
        for capability in [
            CAPABILITY,
            WINDOWS_CAPABILITY,
            RETURN_CHUNKED_CAPABILITY,
            PROGRESS_CAPABILITY,
        ] {
            assert!(
                subc_protocol::manifest::is_valid_capability_identifier(capability),
                "{capability} is not a capability name the daemon admits"
            );
        }
    }
}
