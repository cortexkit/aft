//! Bind publication and control replies do not wait for native tool calls to
//! record their outcomes in the call ledger.

use super::wire::WriterEnqueueOutcome;
use super::*;
use std::sync::atomic::AtomicU8;

const QUEUED: u8 = 0;
const WRITING: u8 = 1;
const WRITTEN: u8 = 2;
const CANCELLED: u8 = 3;
const RESERVED: u8 = 4;
pub(super) const BIND_BUSY_MESSAGE: &str =
    "too many requests are waiting for a route bind; retry shortly";

#[derive(Clone, Copy, Debug)]
struct BindMeta {
    route: RouteChannel,
    ver: u8,
    corr: u64,
    flags: Flags,
}

struct PublicationState {
    phase: AtomicU8,
    meta: Option<BindMeta>,
    error: StdMutex<ErrorBody>,
    error_claimed: AtomicBool,
    enqueued: AtomicBool,
    enqueue_failed: AtomicBool,
    enqueue_changed: Notify,
    terminal_written: AtomicBool,
    succeeded: AtomicBool,
}

#[derive(Clone)]
pub(super) struct AckPublication(Arc<PublicationState>);

impl fmt::Debug for AckPublication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AckPublication")
            .field("phase", &self.0.phase.load(Ordering::Acquire))
            .finish()
    }
}

impl Default for AckPublication {
    fn default() -> Self {
        Self::new(None, QUEUED)
    }
}

impl AckPublication {
    fn new(meta: Option<BindMeta>, phase: u8) -> Self {
        Self(Arc::new(PublicationState {
            phase: AtomicU8::new(phase),
            meta,
            error: StdMutex::new(ErrorBody::new("cancelled", "request cancelled")),
            error_claimed: AtomicBool::new(false),
            enqueued: AtomicBool::new(false),
            enqueue_failed: AtomicBool::new(false),
            enqueue_changed: Notify::new(),
            terminal_written: AtomicBool::new(false),
            succeeded: AtomicBool::new(false),
        }))
    }
    pub(super) fn begin_write(&self) -> bool {
        self.0
            .phase
            .compare_exchange(QUEUED, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(super) fn written(&self) {
        self.0.phase.store(WRITTEN, Ordering::Release);
        self.finished(true);
    }
    pub(super) fn finished(&self, succeeded: bool) {
        self.0.succeeded.store(succeeded, Ordering::Release);
        self.0.terminal_written.store(true, Ordering::Release);
    }
    pub(super) fn outcome(&self) -> Option<bool> {
        self.0
            .terminal_written
            .load(Ordering::Acquire)
            .then(|| self.0.succeeded.load(Ordering::Acquire))
    }
    pub(super) fn is_queued(&self) -> bool {
        self.0.phase.load(Ordering::Acquire) == QUEUED
    }
    pub(super) fn is_committed(&self) -> bool {
        matches!(self.0.phase.load(Ordering::Acquire), WRITING | WRITTEN)
    }
    pub(super) fn is_retracted(&self) -> bool {
        self.0.phase.load(Ordering::Acquire) == CANCELLED
    }
    #[cfg(test)]
    pub(super) fn cancel_before_write(&self) -> bool {
        self.retract(ErrorBody::new("cancelled", "request cancelled"))
    }
    pub(super) fn retract(&self, error: ErrorBody) -> bool {
        let mut reason = self.0.error.lock().unwrap();
        let phase = self.0.phase.load(Ordering::Acquire);
        if !matches!(phase, RESERVED | QUEUED)
            || self
                .0
                .phase
                .compare_exchange(phase, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        *reason = error;
        true
    }
    pub(super) fn error_frame(&self, ack: &Frame) -> Result<Option<Frame>, SubcError> {
        if !self.is_retracted() || self.0.error_claimed.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let error = self.0.error.lock().unwrap().clone();
        Frame::build_with_version(
            ack.header.ver,
            FrameType::Error,
            ack.header.flags,
            0,
            0,
            ack.header.corr,
            serde_json::to_vec(&error).map_err(SubcError::Json)?,
        )
        .map(Some)
        .map_err(SubcError::FrameBuild)
    }
    fn queued(&self) {
        self.0
            .phase
            .compare_exchange(RESERVED, QUEUED, Ordering::AcqRel, Ordering::Acquire)
            .ok();
    }
    fn mark_enqueued(&self, failed: bool) {
        if failed {
            self.0.enqueue_failed.store(true, Ordering::Release);
        } else {
            self.0.enqueued.store(true, Ordering::Release);
        }
        self.0.enqueue_changed.notify_waiters();
    }
    pub(super) async fn wait_enqueued(&self) -> Result<(), SubcError> {
        loop {
            let changed = self.0.enqueue_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.0.enqueue_failed.load(Ordering::Acquire) {
                return Err(SubcError::WriterClosed);
            }
            if self.0.enqueued.load(Ordering::Acquire) {
                return Ok(());
            }
            changed.await;
        }
    }
}

pub(super) enum ControlCompletion {
    ConfigureReady(RouteBindCompletion),
    AckWritten(RouteBindCompletion),
    AckCancelled(Box<ControlCompletion>),
    ManagementAckWritten {
        route: RouteChannel,
        identity: ManagementIdentity,
        corr: u64,
    },
    BindRetracted {
        route: RouteChannel,
        corr: u64,
    },
    WriteFailed,
}

impl ControlCompletion {
    pub(super) fn corr(&self) -> u64 {
        match self {
            Self::ConfigureReady(done) | Self::AckWritten(done) => done.corr,
            Self::ManagementAckWritten { corr, .. } | Self::BindRetracted { corr, .. } => *corr,
            Self::AckCancelled(done) => done.corr(),
            Self::WriteFailed => 0,
        }
    }
}

pub(super) struct AckCompletion {
    permit: mpsc::OwnedPermit<ControlCompletion>,
    completion: ControlCompletion,
    metrics: Arc<DispatchPathMetrics>,
}
impl AckCompletion {
    pub(super) fn publish(self, cancelled: bool) {
        let completion = if cancelled {
            ControlCompletion::AckCancelled(Box::new(self.completion))
        } else {
            self.completion
        };
        self.metrics
            .control_completion_queued
            .fetch_add(1, Ordering::Relaxed);
        let _ = self.permit.send(completion);
    }
}

struct PublicationBudget {
    slot: tokio::sync::OwnedSemaphorePermit,
    permit: mpsc::OwnedPermit<ControlCompletion>,
}
struct PublicationEntry {
    publication: AckPublication,
    budget: Option<PublicationBudget>,
    metrics: Arc<DispatchPathMetrics>,
}

pub(super) struct BindReservation<'a> {
    egress: &'a A0Egress,
    route: RouteChannel,
    publication: AckPublication,
    committed: bool,
}
impl BindReservation<'_> {
    pub(super) fn commit(mut self) -> AckPublication {
        self.committed = true;
        self.publication.clone()
    }
}
impl Drop for BindReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let mut entries = self.egress.publications.lock().unwrap();
            if entries.get(&self.route).is_some_and(|entry| {
                Arc::ptr_eq(&entry.publication.0, &self.publication.0) && entry.budget.is_some()
            }) {
                entries.remove(&self.route);
            }
        }
    }
}

pub(super) struct A0Egress {
    pub(super) writer: WriterSender,
    completions: mpsc::Sender<ControlCompletion>,
    publications: StdMutex<HashMap<RouteChannel, PublicationEntry>>,
    slots: Arc<tokio::sync::Semaphore>,
    closing: AtomicBool,
    probe: Option<SubcTestLifecycleProbe>,
    #[cfg(test)]
    waiter_gate: Option<Arc<WaiterGate>>,
}
#[cfg(test)]
pub(super) struct WaiterGate {
    pub(super) entered: tokio::sync::Semaphore,
    pub(super) release: tokio::sync::Semaphore,
}

impl A0Egress {
    pub(super) fn new(writer: WriterSender, completions: mpsc::Sender<ControlCompletion>) -> Self {
        Self {
            writer,
            completions,
            publications: StdMutex::new(HashMap::new()),
            slots: Arc::new(tokio::sync::Semaphore::new(WRITER_QUEUE_CAPACITY)),
            closing: AtomicBool::new(false),
            probe: None,
            #[cfg(test)]
            waiter_gate: None,
        }
    }
    pub(super) fn with_probe(mut self, probe: Option<SubcTestLifecycleProbe>) -> Self {
        self.probe = probe;
        self
    }
    pub(super) fn reserve(
        &self,
        route: RouteChannel,
        ver: u8,
        corr: u64,
        flags: Flags,
        metrics: &Arc<DispatchPathMetrics>,
    ) -> Result<BindReservation<'_>, ErrorBody> {
        let busy =
            || ErrorBody::new("busy", BIND_BUSY_MESSAGE).with_detail(json!({"retryable":true}));
        if self.closing.load(Ordering::Acquire) {
            return Err(busy());
        }
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| busy())?;
        let permit = self
            .completions
            .clone()
            .try_reserve_owned()
            .map_err(|_| busy())?;
        let publication = AckPublication::new(
            Some(BindMeta {
                route,
                ver,
                corr,
                flags,
            }),
            RESERVED,
        );
        let mut entries = self.publications.lock().unwrap();
        if entries.contains_key(&route) {
            return Err(busy());
        }
        entries.insert(
            route,
            PublicationEntry {
                publication: publication.clone(),
                budget: Some(PublicationBudget { slot, permit }),
                metrics: metrics.clone(),
            },
        );
        Ok(BindReservation {
            egress: self,
            route,
            publication,
            committed: false,
        })
    }
    pub(super) fn publication(&self, route: RouteChannel) -> Option<AckPublication> {
        self.publications
            .lock()
            .unwrap()
            .get(&route)
            .map(|entry| entry.publication.clone())
    }
    pub(super) fn stop(&self) {
        self.closing.store(true, Ordering::Release);
        let routes: Vec<_> = self.publications.lock().unwrap().keys().copied().collect();
        for route in routes {
            self.reject(
                route,
                ErrorBody::new(drain::MODULE_DRAINING_ERROR_CODE, "module is reloading")
                    .with_detail(json!({"retryable":true})),
            );
        }
    }
    pub(super) fn cancel(&self, route: RouteChannel) {
        self.reject(route, ErrorBody::new("cancelled", "request cancelled"));
    }
    pub(super) fn supersede(&self, route: RouteChannel) {
        self.reject(
            route,
            ErrorBody::new("bind_superseded", "route bind superseded by a newer epoch"),
        );
    }
    pub(super) fn retire(&self, route: RouteChannel) {
        let mut entries = self.publications.lock().unwrap();
        if entries
            .get(&route)
            .is_some_and(|entry| entry.publication.outcome().is_some())
        {
            entries.remove(&route);
        }
    }
    pub(super) fn reject(&self, route: RouteChannel, error: ErrorBody) -> bool {
        let pending = {
            let mut entries = self.publications.lock().unwrap();
            let Some(entry) = entries.get_mut(&route) else {
                return false;
            };
            if !entry.publication.retract(error) {
                return false;
            }
            entry
                .budget
                .take()
                .map(|budget| (entry.publication.clone(), budget, entry.metrics.clone()))
        };
        if let Some((publication, budget, metrics)) = pending {
            let meta = publication.0.meta.unwrap();
            let frame = route_bind_ack_frame(meta.ver, meta.corr, meta.flags)
                .expect("small bind ACK is encodable");
            self.queue(
                publication,
                frame,
                ControlCompletion::BindRetracted {
                    route: meta.route,
                    corr: meta.corr,
                },
                budget,
                &metrics,
            );
        }
        true
    }
    pub(super) fn enqueue_ack(
        &self,
        route: RouteChannel,
        frame: Frame,
        completion: ControlCompletion,
        metrics: &Arc<DispatchPathMetrics>,
    ) -> Result<AckPublication, ControlCompletion> {
        let reserved = {
            let mut entries = self.publications.lock().unwrap();
            entries.get_mut(&route).and_then(|entry| {
                entry
                    .budget
                    .take()
                    .map(|budget| (entry.publication.clone(), budget))
            })
        };
        let Some((publication, budget)) = reserved else {
            return Err(completion);
        };
        publication.queued();
        self.queue(publication.clone(), frame, completion, budget, metrics);
        Ok(publication)
    }
    fn queue(
        &self,
        publication: AckPublication,
        frame: Frame,
        completion: ControlCompletion,
        budget: PublicationBudget,
        metrics: &Arc<DispatchPathMetrics>,
    ) {
        let corr = completion.corr();
        let (mut outgoing, receipt) = WriterFrame::with_ack_receipt(frame, publication.clone());
        outgoing.set_ack_completion(AckCompletion {
            permit: budget.permit,
            completion,
            metrics: metrics.clone(),
        });
        let writer = self.writer.clone();
        let completions = self.completions.clone();
        let metrics = metrics.clone();
        let probe = self.probe.clone();
        let pending = match wire::try_enqueue_writer_item(&writer, &metrics, outgoing) {
            WriterEnqueueOutcome::Enqueued => {
                publication.mark_enqueued(false);
                if let Some(probe) = self.probe.as_ref() {
                    probe.route_bind_ack_queued(corr);
                }
                None
            }
            WriterEnqueueOutcome::Full(outgoing) => Some(outgoing),
            WriterEnqueueOutcome::Closed => {
                publication.mark_enqueued(true);
                None
            }
        };
        #[cfg(test)]
        let waiter_gate = self.waiter_gate.clone();
        tokio::spawn(async move {
            // The slot covers the entire receipt waiter, even after its writer
            // completion has been published and consumed by the frame loop.
            let _slot = budget.slot;
            let result = async {
                if let Some(outgoing) = pending {
                    wire::send_reliable_writer_item(&writer, &metrics, outgoing, "A0 bind ACK")
                        .await?;
                    publication.mark_enqueued(false);
                    if let Some(probe) = probe.as_ref() {
                        probe.route_bind_ack_queued(corr);
                    }
                }
                receipt.await.map_err(|_| SubcError::WriterClosed)?
            }
            .await;
            #[cfg(test)]
            if let Some(gate) = waiter_gate {
                gate.entered.add_permits(1);
                let _ = gate.release.acquire().await;
            }
            if result.is_err() {
                publication.mark_enqueued(true);
                let _ = send_counted_channel(
                    &completions,
                    &metrics.control_completion_queued,
                    ControlCompletion::WriteFailed,
                )
                .await;
            }
        });
    }
}
impl Drop for A0Egress {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    struct HeldWriter {
        entered: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
        released: bool,
        bytes: Arc<StdMutex<Vec<u8>>>,
    }

    impl AsyncWrite for HeldWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if !self.released {
                match Pin::new(&mut self.release).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(_) => self.released = true,
                }
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn a0_sender_is_captured_before_ledger_proxy_wraps_the_bulk_lane() {
        let source = include_str!("mod.rs");
        let body = source
            .split_once("async fn run_module_loop_with_task_scopes<")
            .unwrap()
            .1
            .split_once("fn classify_connection_end(")
            .unwrap()
            .0;
        let raw = body.find("let raw_writer_tx = writer_tx.clone();").unwrap();
        let proxy = body.find("ledger_edge.proxy(").unwrap();
        assert!(
            raw < proxy,
            "A0 replies would queue behind native ledger waits"
        );
        assert!(body.contains("A0Egress::new(raw_writer_tx, control_completion_tx.clone())"));
        let management = body.split_once("handle_management_request(").unwrap().1;
        assert!(management.trim_start().starts_with("&a0.writer,"));
    }

    #[test]
    fn cancellation_can_retract_only_an_ack_that_has_not_started_writing() {
        let cancelled = AckPublication::default();
        assert!(cancelled.cancel_before_write());
        assert!(!cancelled.begin_write());
        let writing = AckPublication::default();
        assert!(writing.begin_write());
        assert!(!writing.cancel_before_write());
        writing.written();
        assert!(!writing.cancel_before_write());
    }

    #[tokio::test]
    async fn stopping_egress_retracts_a_queued_ack_before_it_reaches_the_socket() {
        let (writer, mut frames) = mpsc::channel(1);
        let (events, mut completed) = mpsc::channel(1);
        let a0 = A0Egress::new(writer, events);
        a0.reserve(
            route_key(7, 1),
            PROTOCOL_VERSION,
            891,
            control_flags(),
            &Arc::new(DispatchPathMetrics::new()),
        )
        .unwrap()
        .commit();
        assert!(a0
            .enqueue_ack(
                route_key(7, 1),
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                ControlCompletion::ManagementAckWritten {
                    route: route_key(7, 1),
                    identity: ManagementIdentity::default(),
                    corr: 891
                },
                &Arc::new(DispatchPathMetrics::new())
            )
            .is_ok());
        let mut queued = frames.recv().await.unwrap();
        a0.stop();
        assert!(queued.begin_ack_write());
        assert_eq!(queued.header.ty, FrameType::Error);
        queued.finish_ack_write(true);
        assert!(matches!(
            completed.recv().await,
            Some(ControlCompletion::AckCancelled(_))
        ));
    }

    #[tokio::test]
    async fn ack_publication_reserves_capacity_before_spawning_a_waiter() {
        let (writer, mut frames) = mpsc::channel(1);
        let (events, _completed) = mpsc::channel(1);
        let a0 = A0Egress::new(writer, events);
        let held = a0
            .slots
            .acquire_many(WRITER_QUEUE_CAPACITY as u32)
            .await
            .unwrap();
        let result = a0.reserve(
            route_key(7, 1),
            PROTOCOL_VERSION,
            891,
            control_flags(),
            &Arc::new(DispatchPathMetrics::new()),
        );
        assert_eq!(result.err().unwrap().code, "busy");
        assert!(frames.try_recv().is_err());
        assert!(a0.publications.lock().unwrap().is_empty());
        drop(held);
    }

    #[tokio::test]
    async fn ack_completion_waits_for_actual_write_not_queue_acceptance() {
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (tx, rx) = mpsc::channel(8);
        let (pong, pong_rx) = mpsc::channel(8);
        let (done, mut completions) = mpsc::channel(8);
        let a0 = A0Egress::new(tx, done);
        a0.reserve(
            route_key(7, 1),
            PROTOCOL_VERSION,
            891,
            control_flags(),
            &metrics,
        )
        .unwrap()
        .commit();
        let (entered, entering) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let writer = spawn_writer_task(
            HeldWriter {
                entered: Some(entered),
                release: released,
                released: false,
                bytes: Arc::clone(&bytes),
            },
            rx,
            pong_rx,
            Arc::clone(&metrics),
        );
        assert!(a0
            .enqueue_ack(
                route_key(7, 1),
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                ControlCompletion::ManagementAckWritten {
                    route: route_key(7, 1),
                    identity: ManagementIdentity::default(),
                    corr: 891,
                },
                &metrics,
            )
            .is_ok());
        entering.await.unwrap();
        assert!(
            completions.try_recv().is_err(),
            "ACK published before write_all finished"
        );
        assert!(bytes.lock().unwrap().is_empty());
        release.send(()).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), completions.recv())
                .await
                .expect("actual write did not publish its reserved completion"),
            Some(ControlCompletion::ManagementAckWritten { .. })
        ));
        assert!(!bytes.lock().unwrap().is_empty());
        a0.stop();
        drop(a0);
        drop(pong);
        writer.await.unwrap().unwrap();
    }

    async fn control_ack_bypasses_held_ledger_wait(settling: bool) {
        let (_dir, root) = test_support::test_root("a0-held-ledger-detach");
        let executor = Arc::new(Executor::new());
        let ctx = test_support::test_ctx();
        let key = crate::db::call_ledger::Key {
            carrier: "egress-test".into(),
            call_key: "held-detach".into(),
        };
        let db = settling.then(|| {
            Arc::new(StdMutex::new(
                crate::db::open(&_dir.path().join("aft.db")).unwrap(),
            ))
        });
        if let Some(db) = db.as_ref() {
            crate::db::call_ledger::admit(
                &db.lock().unwrap(),
                &key,
                "digest",
                None,
                crate::db::call_ledger::State::DispatchStarted,
            )
            .unwrap();
            ctx.set_db(db.clone());
        }
        assert!(executor.register_actor(root.clone(), ctx));
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let held = executor.submit_async(
            root.clone(),
            Lane::Mutating,
            "held-egress-lane".into(),
            Box::new(move |_| {
                entered.send(()).unwrap();
                let _ = released.recv_timeout(Duration::from_secs(10));
                Response::success("held-egress-lane", json!({}))
            }),
        );
        entering.recv_timeout(Duration::from_secs(5)).unwrap();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (actual, mut receiving) = mpsc::channel(8);
        let (edge, _ready) = tool_provider::LedgerEdge::new();
        edge.retain_shell_for_egress_test(root.clone(), route_key(5, 1), 1);
        let (proxy, proxy_task) =
            edge.proxy(actual.clone(), Arc::clone(&executor), Arc::clone(&metrics));
        proxy
            .send(
                build_error_frame(
                    PROTOCOL_VERSION,
                    5,
                    1,
                    1,
                    control_flags(),
                    if settling { "done" } else { "detached" },
                    "running",
                )
                .unwrap()
                .into(),
            )
            .await
            .unwrap();
        let queued = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if executor.try_mutating_job_state_label(
                    &root,
                    if settling {
                        "ledger-settle"
                    } else {
                        "ledger-detach"
                    },
                ) == Some("queued")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let (completed, mut completion_rx) = mpsc::channel(8);
        let a0 = A0Egress::new(actual, completed);
        a0.reserve(
            route_key(7, 1),
            PROTOCOL_VERSION,
            891,
            control_flags(),
            &metrics,
        )
        .unwrap()
        .commit();
        assert!(a0
            .enqueue_ack(
                route_key(7, 1),
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                ControlCompletion::ManagementAckWritten {
                    route: route_key(7, 1),
                    identity: ManagementIdentity::default(),
                    corr: 891,
                },
                &metrics,
            )
            .is_ok());
        let ack = tokio::time::timeout(Duration::from_secs(1), receiving.recv()).await;
        let _ = release.send(());
        let _ = held.await;
        queued.expect("ledger-detach was never queued behind the holder");
        let mut ack = ack.expect("A0 ACK waited on ledger-detach").unwrap();
        assert_eq!(ack.header.corr, 891);
        assert!(ack.begin_ack_write());
        ack.finish_ack_write(true);
        assert!(matches!(
            completion_rx.recv().await,
            Some(ControlCompletion::ManagementAckWritten { .. })
        ));
        let terminal = tokio::time::timeout(Duration::from_secs(5), receiving.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.header.channel, 5);
        if let Some(db) = db {
            assert_eq!(
                crate::db::call_ledger::get(&db.lock().unwrap(), &key)
                    .unwrap()
                    .unwrap()
                    .state,
                crate::db::call_ledger::State::Settled
            );
        }
        drop(proxy);
        proxy_task.await.unwrap();
        a0.stop();
    }

    #[tokio::test]
    async fn control_ack_bypasses_a_real_ledger_detach_wait() {
        control_ack_bypasses_held_ledger_wait(false).await;
    }

    #[tokio::test]
    async fn control_ack_bypasses_a_real_ledger_settle_wait() {
        control_ack_bypasses_held_ledger_wait(true).await;
    }
}

#[cfg(test)]
mod fixup_tests {
    use super::*;

    async fn cancelled_receipt(tool: bool) {
        let (_dir, root) = test_support::test_root("retracted-ack-receipt-consumption");
        let route = route_key(7, 1);
        let (writer, mut frames) = mpsc::channel(8);
        let (done, mut completed) = mpsc::channel(8);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let a0 = A0Egress::new(writer.clone(), done);
        let publication = a0
            .reserve(route, PROTOCOL_VERSION, 891, control_flags(), &metrics)
            .unwrap()
            .commit();
        let mut pending = HashMap::new();
        let completion = if tool {
            pending.insert(
                route,
                PendingBind {
                    ack_publication: Some(publication.clone()),
                    bind_root_id: root.clone(),
                    inserted_new_actor: false,
                    cancelled: true,
                    configure_request_id: "retracted-receipt".into(),
                    started_at: Instant::now(),
                    warned_half_deadline: false,
                    deadline_reported: false,
                    corr: 891,
                    ver: PROTOCOL_VERSION,
                    flags: control_flags(),
                    cancellation: JobCancellation::new(),
                    session: None,
                },
            );
            ControlCompletion::AckWritten(RouteBindCompletion {
                expected_snapshot_id: None,
                applied_snapshot_id: None,
                route,
                identity: test_support::route_identity(&root, "receipt"),
                bind_root_id: root,
                inserted_new_actor: false,
                configure_response: Response::success("receipt", json!({})),
                diagnostics_on_edit: false,
                ver: PROTOCOL_VERSION,
                corr: 891,
                flags: control_flags(),
            })
        } else {
            ControlCompletion::ManagementAckWritten {
                route,
                identity: ManagementIdentity::default(),
                corr: 891,
            }
        };
        assert!(a0
            .enqueue_ack(
                route,
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                completion,
                &metrics
            )
            .is_ok());
        let mut queued = frames.recv().await.unwrap();
        a0.cancel(route);
        assert!(queued.begin_ack_write());
        assert_eq!(queued.header.ty, FrameType::Error);
        queued.finish_ack_write(true);
        let completion = completed.recv().await.unwrap();
        assert!(matches!(completion, ControlCompletion::AckCancelled(_)));
        let mut installed = HashMap::from([(7, 2)]);
        process_route_bind_completion(
            &writer,
            completion,
            &a0,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut pending,
            &mut installed,
            &Arc::new(Executor::new()),
            &Arc::new(Notify::new()),
            &metrics,
            None,
        )
        .await
        .expect("a valid retraction receipt must not close the connection");
        assert!(a0.publications.lock().unwrap().is_empty());
        assert!(pending.is_empty());
        assert_eq!(
            installed.get(&7),
            Some(&2),
            "late receipt erased a newer route epoch"
        );
        assert_eq!(metrics.control_completion_queued.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn retracted_management_ack_receipt_retires_without_closing_transport() {
        cancelled_receipt(false).await;
    }
    #[tokio::test]
    async fn retracted_tool_ack_receipt_retires_without_closing_transport() {
        cancelled_receipt(true).await;
    }

    #[tokio::test]
    async fn queued_ack_deadline_refuses_but_writing_ack_has_already_succeeded() {
        let (_dir, root) = test_support::test_root("ack-deadline-stages");
        for stage in [RESERVED, QUEUED, WRITING] {
            let writing = stage == WRITING;
            let route = route_key(7, 1);
            let (writer, mut frames) = mpsc::channel(8);
            let (done, mut completed) = mpsc::channel(8);
            let metrics = Arc::new(DispatchPathMetrics::new());
            let a0 = A0Egress::new(writer.clone(), done);
            let publication = a0
                .reserve(route, PROTOCOL_VERSION, 891, control_flags(), &metrics)
                .unwrap()
                .commit();
            let mut ack = if stage != RESERVED {
                assert!(a0
                    .enqueue_ack(
                        route,
                        route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                        ControlCompletion::ManagementAckWritten {
                            route,
                            identity: ManagementIdentity::default(),
                            corr: 891
                        },
                        &metrics
                    )
                    .is_ok());
                Some(frames.recv().await.unwrap())
            } else {
                None
            };
            if writing {
                assert!(ack.as_mut().unwrap().begin_ack_write());
            }
            let mut pending = HashMap::from([(
                route,
                PendingBind {
                    ack_publication: Some(publication.clone()),
                    bind_root_id: root.clone(),
                    inserted_new_actor: false,
                    cancelled: false,
                    configure_request_id: "ack-deadline".into(),
                    started_at: Instant::now() - ROUTE_BIND_DEADLINE - Duration::from_millis(1),
                    warned_half_deadline: false,
                    deadline_reported: false,
                    corr: 891,
                    ver: PROTOCOL_VERSION,
                    flags: control_flags(),
                    cancellation: JobCancellation::new(),
                    session: None,
                },
            )]);
            expire_overdue_route_binds(
                &writer,
                &a0,
                &Arc::new(Executor::new()),
                &mut pending,
                &mut HashMap::from([(7, 1)]),
                &metrics,
            )
            .await
            .unwrap();
            assert_eq!(pending[&route].deadline_reported, !writing);
            let mut ack = ack.take().unwrap_or_else(|| {
                frames
                    .try_recv()
                    .expect("RESERVED bind deadline did not queue an error")
            });
            assert!(
                frames.try_recv().is_err(),
                "deadline must not create a second terminal"
            );
            if !writing {
                assert!(ack.begin_ack_write());
                assert_eq!(ack.header.ty, FrameType::Error);
                let body: Value = serde_json::from_slice(&ack.body).unwrap();
                assert_eq!(body["code"], "actor_not_ready");
                assert!(body["message"]
                    .as_str()
                    .unwrap()
                    .contains(if stage == RESERVED {
                        "configure did not finish"
                    } else {
                        "ACK writer queue"
                    }));
            } else {
                assert_eq!(ack.header.ty, FrameType::Response);
            }
            ack.finish_ack_write(true);
            assert_eq!(publication.outcome(), Some(writing));
            let completion = completed.recv().await.unwrap();
            assert_eq!(
                matches!(completion, ControlCompletion::AckCancelled(_)),
                !writing
            );
        }
    }

    #[tokio::test]
    async fn receipt_waiters_keep_their_slot_after_completion_is_consumed() {
        let (writer, mut frames) = mpsc::channel(1);
        let (done, mut completed) = mpsc::channel(1);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let mut a0 = A0Egress::new(writer, done);
        let gate = Arc::new(WaiterGate {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        a0.waiter_gate = Some(gate.clone());
        for channel in 1..=WRITER_QUEUE_CAPACITY as u16 {
            let route = route_key(channel, 1);
            a0.reserve(
                route,
                PROTOCOL_VERSION,
                channel as u64,
                control_flags(),
                &metrics,
            )
            .unwrap()
            .commit();
            assert!(a0
                .enqueue_ack(
                    route,
                    route_bind_ack_frame(PROTOCOL_VERSION, channel as u64, control_flags())
                        .unwrap(),
                    ControlCompletion::ManagementAckWritten {
                        route,
                        identity: ManagementIdentity::default(),
                        corr: channel as u64
                    },
                    &metrics
                )
                .is_ok());
            let mut ack = frames.recv().await.unwrap();
            assert!(ack.begin_ack_write());
            ack.finish_ack_write(true);
            completed.recv().await.unwrap();
            a0.retire(route);
        }
        let entered = gate
            .entered
            .acquire_many(WRITER_QUEUE_CAPACITY as u32)
            .await
            .unwrap();
        assert!(a0.publications.lock().unwrap().is_empty());
        assert_eq!(
            a0.reserve(
                route_key(1000, 1),
                PROTOCOL_VERSION,
                1000,
                control_flags(),
                &metrics
            )
            .err()
            .unwrap()
            .code,
            "busy"
        );
        drop(entered);
        gate.release.add_permits(WRITER_QUEUE_CAPACITY);
    }

    #[tokio::test]
    async fn full_writer_ack_waits_off_loop_and_closed_writer_never_publishes() {
        let (writer, mut frames) = mpsc::channel(1);
        let (done, mut completed) = mpsc::channel(2);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let a0 = A0Egress::new(writer.clone(), done);
        writer
            .send(
                Frame::build(FrameType::Push, control_flags(), 7, 1, 0, b"{}".to_vec())
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
        let route = route_key(7, 1);
        let publication = a0
            .reserve(route, PROTOCOL_VERSION, 891, control_flags(), &metrics)
            .unwrap()
            .commit();
        assert!(a0
            .enqueue_ack(
                route,
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                ControlCompletion::ManagementAckWritten {
                    route,
                    identity: ManagementIdentity::default(),
                    corr: 891
                },
                &metrics
            )
            .is_ok());
        assert!(!publication.0.enqueued.load(Ordering::Acquire));
        assert!(completed.try_recv().is_err());
        frames.recv().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), publication.wait_enqueued())
            .await
            .unwrap()
            .unwrap();
        let mut ack = frames.recv().await.unwrap();
        assert!(ack.begin_ack_write());
        ack.finish_ack_write(false);
        assert!(matches!(
            completed.recv().await,
            Some(ControlCompletion::WriteFailed)
        ));
        assert_eq!(publication.outcome(), None);
        assert!(completed.try_recv().is_err());
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    #[tokio::test]
    async fn actual_ack_write_precedes_cross_lane_push_bg_and_candidate_result() {
        let (_dir, root) = test_support::test_root("cross-lane-ack-publication");
        let route = route_key(7, 1);
        let identity = test_support::route_identity(&root, "session-1");
        let replay_key = push::ReplayKey::from_identity(&identity);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let executor = Arc::new(Executor::new());
        let (actual, frames) = mpsc::channel(8);
        let (pong, pongs) = mpsc::channel(8);
        let (done, mut completed) = mpsc::channel(8);
        let a0 = A0Egress::new(actual.clone(), done);
        let publication = a0
            .reserve(route, PROTOCOL_VERSION, 891, control_flags(), &metrics)
            .unwrap()
            .commit();
        let (edge, _ready) = tool_provider::LedgerEdge::new();
        let (proxy, proxy_task) = edge.proxy(actual.clone(), executor.clone(), metrics.clone());
        let (mut peer, socket) = tokio::io::duplex(128 * 1024);
        let (entered, entering) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let writer = spawn_writer_task(
            crate::subc::bind_ack_fixup_tests::GateWrite {
                inner: socket,
                armed: Arc::new(AtomicBool::new(true)),
                entered: Some(entered),
                release: released,
                passed: false,
            },
            frames,
            pongs,
            metrics.clone(),
        );
        let completion = RouteBindCompletion {
            expected_snapshot_id: None,
            applied_snapshot_id: None,
            route,
            identity,
            bind_root_id: root.clone(),
            inserted_new_actor: false,
            configure_response: Response::success("bind", json!({})),
            diagnostics_on_edit: false,
            ver: PROTOCOL_VERSION,
            corr: 891,
            flags: control_flags(),
        };
        let mut pending = HashMap::from([(
            route,
            PendingBind {
                ack_publication: Some(publication),
                bind_root_id: root.clone(),
                inserted_new_actor: false,
                cancelled: false,
                configure_request_id: "bind".into(),
                started_at: Instant::now(),
                warned_half_deadline: false,
                deadline_reported: false,
                corr: 891,
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                cancellation: JobCancellation::new(),
                session: None,
            },
        )]);
        let mut routes = HashMap::new();
        let mut roots = HashMap::new();
        let mut sessions = HashMap::new();
        let mut buffered = HashMap::from([(
            replay_key.clone(),
            VecDeque::from([crate::subc::test_support::completion_frame(
                "cross-lane-replay",
            )]),
        )]);
        let mut live = HashMap::new();
        let mut installed = HashMap::from([(7, 1)]);
        let mut management = HashMap::new();
        let shutdown = Arc::new(Notify::new());
        handle_route_bind_completion(
            &proxy,
            completion,
            false,
            &a0,
            &mut routes,
            &mut roots,
            &mut sessions,
            &mut buffered,
            &mut live,
            &mut pending,
            &mut installed,
            &executor,
            &shutdown,
            &metrics,
            None,
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), entering)
            .await
            .expect("ACK never reached actual writer")
            .unwrap();
        assert!(
            routes.is_empty(),
            "a queued ACK is not publication authority"
        );
        assert_eq!(
            buffered[&replay_key].len(),
            1,
            "buffer ownership must survive the blocked ACK"
        );
        assert!(completed.try_recv().is_err());
        let mut candidates = bind_candidates::CandidateRequests::default();
        for corr in [1, 2] {
            let frame = DecodedFrame {
                frame: Frame::build(
                    FrameType::Request,
                    control_flags(),
                    7,
                    1,
                    corr,
                    b"{}".to_vec(),
                )
                .unwrap(),
                phase_trace: PhaseTrace::new(Instant::now()),
                parsed_body: Default::default(),
                confinement: None,
                codemode_policies: None,
                launch_restore: None,
            };
            assert!(candidates
                .intercept(frame, &a0, &mut pending, &executor, &metrics)
                .await
                .unwrap()
                .is_none());
        }
        candidates
            .release(&a0, &routes, &management, &metrics)
            .await
            .unwrap();
        assert!(
            candidates.next_ready().is_none(),
            "no candidate work may execute while ACK write is blocked"
        );
        release.send(()).unwrap();
        let completion = tokio::time::timeout(Duration::from_secs(5), completed.recv())
            .await
            .expect("actual ACK write produced no completion")
            .unwrap();
        process_route_bind_completion(
            &proxy,
            completion,
            &a0,
            &mut management,
            &mut routes,
            &mut roots,
            &mut sessions,
            &mut buffered,
            &mut live,
            &mut pending,
            &mut installed,
            &executor,
            &shutdown,
            &metrics,
            None,
        )
        .await
        .unwrap();
        candidates
            .release(&a0, &routes, &management, &metrics)
            .await
            .unwrap();
        let fast = candidates.next_ready().unwrap();
        assert_eq!(fast.frame.header.corr, 1);
        send_reliable_writer_frame(
            &proxy,
            &metrics,
            Frame::build(
                FrameType::Response,
                control_flags(),
                7,
                1,
                1,
                b"{}".to_vec(),
            )
            .unwrap(),
            "fast candidate result",
        )
        .await
        .unwrap();
        let subscription = candidates.next_ready().unwrap();
        assert_eq!(subscription.frame.header.corr, 2);
        let mut wakes = BgWakePending::from([(route, BgWakeState::armed(Instant::now()))]);
        let subscriptions = HashMap::from([(
            route,
            BgSub {
                corr: 2,
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                root,
                session: "session-1".into(),
            },
        )]);
        push::emit_bg_event_wakes(&proxy, &metrics, &subscriptions, &mut wakes);
        let ack = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut peer))
            .await
            .expect("ACK not written")
            .unwrap()
            .unwrap();
        assert_eq!((ack.header.channel, ack.header.corr), (0, 891));
        let mut seen = HashSet::new();
        for _ in 0..3 {
            let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut peer))
                .await
                .expect("cross-lane replay not written")
                .unwrap()
                .unwrap();
            assert_eq!(frame.header.channel, 7);
            seen.insert((frame.header.ty as u8, frame.header.corr));
        }
        assert!(seen.contains(&(FrameType::Push as u8, 0)));
        assert!(seen.contains(&(FrameType::Response as u8, 1)));
        assert!(seen.contains(&(FrameType::StreamData as u8, 2)));
        drop(proxy);
        proxy_task.await.unwrap();
        drop(a0);
        drop(actual);
        drop(pong);
        writer.await.unwrap().unwrap();
    }

    #[test]
    fn production_control_lane_and_completion_priority_are_explicit() {
        let source = include_str!("mod.rs");
        let body = source
            .split_once("async fn run_module_loop_with_task_scopes<")
            .unwrap()
            .1
            .split_once("fn classify_connection_end(")
            .unwrap()
            .0
            .split_whitespace()
            .collect::<String>();
        assert_eq!(body.matches("handle_control_request(").count(), 1);
        assert!(body.contains("handle_control_request(&a0.writer,&writer_tx,&a0,"));
        assert!(body.contains("expire_overdue_route_binds(&a0.writer,&a0,"));
        assert!(body.contains("handle_management_request(&a0.writer,"));
        assert!(body.contains("let(configure_completion_tx,mutconfigure_completion_rx)="));
        assert!(body.contains("&configure_completion_tx,"));
        let drain = body
            .find("matchdrain_pending_route_bind_completions(")
            .unwrap();
        assert!(drain < body.find("push::drain_lossy_push_batch(").unwrap());
        assert!(body.contains("tokio::select!{biased;Some(ended)=scopes_ended_rx.recv()"));
        let select = body.find("tokio::select!{biased;Some(ended)").unwrap();
        assert!(
            select
                < body
                    .find("completion=control_completion_rx.recv()")
                    .unwrap()
        );
        assert!(
            body.find("completion=control_completion_rx.recv()")
                .unwrap()
                < body
                    .find("(maybe_frame,database_waited,ledger_waited)")
                    .unwrap()
        );
        let egress = include_str!("a0_egress.rs");
        let publish = egress
            .split_once("pub(super) fn publish(self, cancelled: bool)")
            .unwrap()
            .1
            .split_once("struct PublicationBudget")
            .unwrap()
            .0;
        assert!(publish.contains("self.permit.send(completion)"));
        assert!(!publish.contains("tokio::spawn"));
        assert!(!publish.contains(".await"));
        let queue = egress
            .split_once("    fn queue(")
            .unwrap()
            .1
            .split_once("impl Drop for A0Egress")
            .unwrap()
            .0;
        assert!(
            queue.find("set_ack_completion(").unwrap()
                < queue.find("try_enqueue_writer_item(").unwrap()
        );
    }
}
