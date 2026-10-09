use super::{types::*, *};
use serde::Serialize;
use serde_json::json;
use std::{path::Path, sync::Arc};
use subc_client_rs::{CallOptions, ConsumerOptions, SubcConsumer, SubscribeOptions, Subscription};
use subc_protocol::{BindIdentity, RouteTarget};

pub const CAPABILITY: &str = "exec-remote/v1";

/// An authenticated daemon route to the catalog's sole capability provider.
/// No module ID fallback, SSH transport, snapshotting, or automatic run retry.
pub struct ExecRemoteClient {
    consumer: Arc<SubcConsumer>,
    route: subc_client_rs::RouteHandle,
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
                RouteTarget::ManagementSurface { module_id },
                identity,
                CallOptions {
                    consumer_identity: crate::launch_nonce::consumer_identity(),
                    ..CallOptions::default()
                },
            )
            .await
            .map_err(Error::Transport)?;
        Ok(Self { consumer, route })
    }

    pub async fn run(&self, request: &RunRequest) -> Result<RemoteStream, Error> {
        self.stream("exec.run", request, StreamConsumer::new())
            .await
    }

    /// Resume from the last durably delivered seq, not from the last received
    /// packet. `from_seq` on the wire is inclusive, so this sends last_seq + 1.
    pub async fn attach(&self, point: ResumePoint) -> Result<RemoteStream, Error> {
        let request = point.attach_request()?;
        self.stream("exec.attach", &request, StreamConsumer::resume(point))
            .await
    }

    async fn stream<T: Serialize>(
        &self,
        method: &str,
        params: &T,
        consumer: StreamConsumer,
    ) -> Result<RemoteStream, Error> {
        let subscription = self
            .consumer
            .subscribe_route(
                &self.route,
                envelope(method, params)?,
                SubscribeOptions::default(),
            )
            .await
            .map_err(Error::Transport)?;
        Ok(RemoteStream {
            subscription,
            consumer,
            finished: false,
            received_frame: false,
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
            stream
                .consumer
                .recovery("cancel sent before job ID was received; query exec.status")
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
    Complete(Verdict),
}

/// One held-open run/attach call. Dropping it sends Cancel (SDK semantics), not
/// a detach. Persist its resume point before dropping when recovery is needed.
pub struct RemoteStream {
    subscription: Subscription,
    consumer: StreamConsumer,
    finished: bool,
    received_frame: bool,
}

impl RemoteStream {
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
            self.received_frame = true;
            if let Err(error) = self.consumer.consume_bytes(&bytes, sink) {
                return Err(self.consumer.recovery(error.to_string()));
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
