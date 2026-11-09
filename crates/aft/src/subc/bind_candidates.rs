//! Candidate requests are not executed until the bind ACK has finished writing.
//! If the bind is refused, their errors follow its control-channel error.

use super::*;

const HELD_REQUESTS: usize = 256;
const HELD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct CandidateRequests {
    held: HashMap<RouteChannel, (a0_egress::AckPublication, VecDeque<DecodedFrame>)>,
    count: usize,
    bytes: usize,
    ready: VecDeque<DecodedFrame>,
    closing: HashMap<RouteChannel, a0_egress::AckPublication>,
    cancelled: HashSet<(RouteChannel, u64)>,
}

impl CandidateRequests {
    pub(super) fn next_ready(&mut self) -> Option<DecodedFrame> {
        let frame = self.ready.pop_front()?;
        self.count -= 1;
        self.bytes -= frame.frame.body.len();
        Some(frame)
    }
    fn take(&mut self, route: RouteChannel) -> VecDeque<DecodedFrame> {
        let frames = self
            .held
            .remove(&route)
            .map(|(_, frames)| frames)
            .unwrap_or_default();
        self.count -= frames.len();
        self.bytes -= frames
            .iter()
            .map(|frame| frame.frame.body.len())
            .sum::<usize>();
        frames
    }

    pub(super) async fn intercept(
        &mut self,
        frame: DecodedFrame,
        a0: &A0Egress,
        pending: &mut HashMap<RouteChannel, PendingBind>,
        executor: &Arc<Executor>,
        metrics: &Arc<DispatchPathMetrics>,
    ) -> Result<Option<DecodedFrame>, SubcError> {
        if frame.frame.header.ty == FrameType::Cancel {
            let header = &frame.frame.header;
            let route = route_key(header.channel, header.epoch);
            if self.held.get(&route).is_some_and(|(_, frames)| {
                frames
                    .iter()
                    .any(|held| held.frame.header.corr == header.corr)
            }) {
                self.cancelled.insert((route, header.corr));
                return Ok(None);
            }
        }
        if frame.frame.header.ty != FrameType::Request || frame.frame.header.channel == 0 {
            return Ok(Some(frame));
        }
        let route = route_key(frame.frame.header.channel, frame.frame.header.epoch);
        let Some(publication) = a0.publication(route) else {
            return Ok(Some(frame));
        };
        if !publication.is_retracted()
            && self.count < HELD_REQUESTS
            && frame.frame.body.len() <= HELD_BYTES.saturating_sub(self.bytes)
        {
            self.count += 1;
            self.bytes += frame.frame.body.len();
            self.held
                .entry(route)
                .or_insert_with(|| (publication, VecDeque::new()))
                .1
                .push_back(frame);
            return Ok(None);
        }

        // An ACK already writing has won admission. Overflow can retract only
        // a bind whose ACK has not started writing; it must not emit both an
        // ACK and an error for the same channel-0 request.
        let rejected = a0.reject(
            route,
            ErrorBody::new("busy", a0_egress::BIND_BUSY_MESSAGE)
                .with_detail(json!({"retryable":true})),
        );
        if rejected {
            if let Some(bind) = pending.get_mut(&route) {
                bind.cancelled = true;
                executor.cancel_job(&bind.bind_root_id, &bind.cancellation);
            }
        }
        publication.wait_enqueued().await?;
        if publication.is_retracted() {
            let frames = self.take(route);
            for held in frames {
                self.refuse(&a0.writer, held, metrics).await?;
            }
        }
        self.refuse(&a0.writer, frame, metrics).await?;
        if rejected {
            self.closing.insert(route, publication);
        }
        Ok(None)
    }

    pub(super) async fn release(
        &mut self,
        a0: &A0Egress,
        routes: &HashMap<RouteChannel, RouteIdentity>,
        management: &HashMap<RouteChannel, ManagementIdentity>,
        metrics: &Arc<DispatchPathMetrics>,
    ) -> Result<(), SubcError> {
        let resolved: Vec<_> = self
            .held
            .iter()
            .filter_map(|(route, (publication, _))| {
                publication.outcome().and_then(|success| {
                    (!success || routes.contains_key(route) || management.contains_key(route))
                        .then_some((*route, success))
                })
            })
            .collect();
        for (route, success) in resolved {
            let frames = self.take(route);
            if success {
                for frame in frames {
                    if self.cancelled.contains(&(route, frame.frame.header.corr)) {
                        self.refuse(&a0.writer, frame, metrics).await?;
                    } else {
                        self.count += 1;
                        self.bytes += frame.frame.body.len();
                        self.ready.push_back(frame);
                    }
                }
            } else {
                for frame in frames {
                    self.refuse(&a0.writer, frame, metrics).await?;
                }
            }
        }
        let closed: Vec<_> = self
            .closing
            .iter()
            .filter_map(|(route, publication)| publication.outcome().map(|_| *route))
            .collect();
        for route in closed {
            self.closing.remove(&route);
            send_reliable_writer_frame(
                &a0.writer,
                metrics,
                build_goodbye_frame(PROTOCOL_VERSION, route.channel, route.epoch, 0)?,
                "candidate bind overflow close",
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn stop(
        &mut self,
        a0: &A0Egress,
        metrics: &DispatchPathMetrics,
    ) -> Result<(), SubcError> {
        let routes: Vec<_> = self.held.keys().copied().collect();
        for route in routes {
            self.held[&route].0.wait_enqueued().await?;
            for frame in self.take(route) {
                self.refuse(&a0.writer, frame, metrics).await?;
            }
        }
        while let Some(frame) = self.next_ready() {
            self.refuse(&a0.writer, frame, metrics).await?;
        }
        Ok(())
    }

    async fn refuse(
        &mut self,
        tx: &WriterSender,
        frame: DecodedFrame,
        metrics: &DispatchPathMetrics,
    ) -> Result<(), SubcError> {
        let header = frame.frame.header;
        let (code, message) = if self
            .cancelled
            .remove(&(route_key(header.channel, header.epoch), header.corr))
        {
            ("cancelled", "request cancelled")
        } else {
            ("route_not_bound", "route is not bound before tool call")
        };
        send_reliable_writer_frame(
            tx,
            metrics,
            build_error_frame(
                header.ver,
                header.channel,
                header.epoch,
                header.corr,
                header.flags,
                code,
                message,
            )?,
            "candidate bind refusal",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(corr: u64) -> DecodedFrame {
        DecodedFrame {
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
        }
    }

    fn egress(
        capacity: usize,
    ) -> (
        A0Egress,
        mpsc::Receiver<WriterFrame>,
        mpsc::Receiver<ControlCompletion>,
        Arc<DispatchPathMetrics>,
    ) {
        let (writer, frames) = mpsc::channel(capacity);
        let (completed, completions) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let a0 = A0Egress::new(writer, completed);
        a0.reserve(
            route_key(7, 1),
            PROTOCOL_VERSION,
            891,
            control_flags(),
            &metrics,
        )
        .unwrap()
        .commit();
        (a0, frames, completions, metrics)
    }

    #[tokio::test]
    async fn candidate_calls_wait_through_configure_queue_and_actual_ack_write() {
        let (a0, mut frames, mut completions, metrics) = egress(8);
        let mut candidates = CandidateRequests::default();
        let executor = Arc::new(Executor::new());
        let mut pending = HashMap::new();
        assert!(candidates
            .intercept(request(1), &a0, &mut pending, &executor, &metrics)
            .await
            .unwrap()
            .is_none());
        assert!(
            frames.try_recv().is_err(),
            "configure has not produced an ACK"
        );
        assert!(a0
            .enqueue_ack(
                route_key(7, 1),
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                ControlCompletion::ManagementAckWritten {
                    route: route_key(7, 1),
                    identity: ManagementIdentity::default(),
                    corr: 891
                },
                &metrics
            )
            .is_ok());
        candidates
            .intercept(request(2), &a0, &mut pending, &executor, &metrics)
            .await
            .unwrap();
        let mut ack = frames.recv().await.unwrap();
        assert!(ack.begin_ack_write());
        let management = HashMap::from([(route_key(7, 1), ManagementIdentity::default())]);
        candidates
            .release(&a0, &HashMap::new(), &management, &metrics)
            .await
            .unwrap();
        assert!(
            candidates.next_ready().is_none(),
            "write-start is not completed write"
        );
        assert!(completions.try_recv().is_err());
        ack.finish_ack_write(true);
        completions.recv().await.unwrap();
        candidates
            .release(&a0, &HashMap::new(), &management, &metrics)
            .await
            .unwrap();
        assert_eq!(
            candidates.count, 2,
            "ready frames retain their admission credit"
        );
        assert_eq!(candidates.next_ready().unwrap().frame.header.corr, 1);
        assert_eq!(candidates.next_ready().unwrap().frame.header.corr, 2);
        assert!(candidates.next_ready().is_none());
        assert_eq!(candidates.count, 0);
        assert_eq!(candidates.bytes, 0);
    }

    #[tokio::test]
    async fn cancelled_candidate_never_executes_or_answers_twice_after_ack() {
        let (a0, mut frames, mut completed, metrics) = egress(8);
        let mut candidates = CandidateRequests::default();
        let executor = Arc::new(Executor::new());
        candidates
            .intercept(request(1), &a0, &mut HashMap::new(), &executor, &metrics)
            .await
            .unwrap();
        let mut cancel = request(1);
        cancel.frame =
            Frame::build(FrameType::Cancel, control_flags(), 7, 1, 1, Vec::new()).unwrap();
        assert!(candidates
            .intercept(cancel, &a0, &mut HashMap::new(), &executor, &metrics)
            .await
            .unwrap()
            .is_none());
        assert!(a0
            .enqueue_ack(
                route_key(7, 1),
                route_bind_ack_frame(PROTOCOL_VERSION, 891, control_flags()).unwrap(),
                ControlCompletion::ManagementAckWritten {
                    route: route_key(7, 1),
                    identity: ManagementIdentity::default(),
                    corr: 891
                },
                &metrics
            )
            .is_ok());
        let mut ack = frames.recv().await.unwrap();
        assert!(ack.begin_ack_write());
        ack.finish_ack_write(true);
        completed.recv().await.unwrap();
        let management = HashMap::from([(route_key(7, 1), ManagementIdentity::default())]);
        candidates
            .release(&a0, &HashMap::new(), &management, &metrics)
            .await
            .unwrap();
        assert!(
            candidates.next_ready().is_none(),
            "cancelled candidate escaped to execution"
        );
        let terminal = frames.try_recv().expect("one cancelled candidate terminal");
        assert_eq!(terminal.header.corr, 1);
        let body: Value = serde_json::from_slice(&terminal.body).unwrap();
        assert_eq!(body["code"], "cancelled");
        assert_eq!(body["message"], "request cancelled");
        candidates
            .release(&a0, &HashMap::new(), &management, &metrics)
            .await
            .unwrap();
        assert!(frames.try_recv().is_err());
        assert!(candidates.cancelled.is_empty());
        assert_eq!(candidates.count, 0);
    }

    #[tokio::test]
    async fn failed_bind_answers_held_requests_after_its_terminal_in_arrival_order() {
        let (a0, mut frames, mut completions, metrics) = egress(8);
        let mut candidates = CandidateRequests::default();
        for corr in [1, 2, 3] {
            candidates
                .intercept(
                    request(corr),
                    &a0,
                    &mut HashMap::new(),
                    &Arc::new(Executor::new()),
                    &metrics,
                )
                .await
                .unwrap();
        }
        assert!(a0.reject(
            route_key(7, 1),
            ErrorBody::new("config_divergence", "configure failed during route bind")
        ));
        candidates
            .release(&a0, &HashMap::new(), &HashMap::new(), &metrics)
            .await
            .unwrap();
        let mut terminal = frames.try_recv().expect("bind refusal was not queued");
        assert!(terminal.begin_ack_write());
        assert_eq!(terminal.header.channel, 0);
        assert_eq!(terminal.header.corr, 891);
        assert_eq!(terminal.header.ty, FrameType::Error);
        assert!(
            frames.try_recv().is_err(),
            "held calls escaped before bind terminal"
        );
        terminal.finish_ack_write(true);
        completions.recv().await.unwrap();
        candidates
            .release(&a0, &HashMap::new(), &HashMap::new(), &metrics)
            .await
            .unwrap();
        for corr in [1, 2, 3] {
            let refused = frames.recv().await.unwrap();
            assert_eq!(refused.header.corr, corr);
            let body: Value = serde_json::from_slice(&refused.body).unwrap();
            assert_eq!(body["code"], "route_not_bound");
            assert_eq!(body["message"], "route is not bound before tool call");
        }
        assert!(frames.try_recv().is_err());
    }

    #[tokio::test]
    async fn candidate_overflow_fails_bind_then_refuses_every_owner_before_close() {
        let (a0, mut frames, mut completions, metrics) = egress(HELD_REQUESTS + 8);
        let mut candidates = CandidateRequests::default();
        let executor = Arc::new(Executor::new());
        for corr in 1..=HELD_REQUESTS as u64 + 2 {
            candidates
                .intercept(request(corr), &a0, &mut HashMap::new(), &executor, &metrics)
                .await
                .unwrap();
        }
        assert_eq!(candidates.count, 0);
        let mut terminal = frames.try_recv().expect("bind refusal was not queued");
        assert!(terminal.begin_ack_write());
        assert_eq!(terminal.header.corr, 891);
        let body: Value = serde_json::from_slice(&terminal.body).unwrap();
        assert_eq!(body["code"], "busy");
        assert_eq!(body["message"], a0_egress::BIND_BUSY_MESSAGE);
        terminal.finish_ack_write(true);
        completions.recv().await.unwrap();
        candidates
            .release(&a0, &HashMap::new(), &HashMap::new(), &metrics)
            .await
            .unwrap();
        for corr in 1..=HELD_REQUESTS as u64 + 2 {
            let refused = frames.recv().await.unwrap();
            assert_eq!(refused.header.ty, FrameType::Error);
            assert_eq!(refused.header.channel, 7);
            assert_eq!(refused.header.corr, corr);
        }
        assert_eq!(frames.recv().await.unwrap().header.ty, FrameType::Goodbye);
        assert!(frames.try_recv().is_err());
    }

    #[tokio::test]
    async fn candidate_wire_bytes_are_bounded_before_count_capacity() {
        let (a0, mut frames, mut completed, metrics) = egress(16);
        let mut candidates = CandidateRequests::default();
        let executor = Arc::new(Executor::new());
        for corr in 1..=9 {
            let mut frame = request(corr);
            frame.frame = Frame::build(
                FrameType::Request,
                control_flags(),
                7,
                1,
                corr,
                vec![b' '; 8 * 1024 * 1024],
            )
            .unwrap();
            candidates
                .intercept(frame, &a0, &mut HashMap::new(), &executor, &metrics)
                .await
                .unwrap();
        }
        assert_eq!(candidates.count, 0, "byte overflow did not reject the bind");
        assert_eq!(candidates.bytes, 0);
        let mut terminal = frames
            .try_recv()
            .expect("byte overflow did not queue a bind refusal");
        assert!(terminal.begin_ack_write());
        assert_eq!(terminal.header.channel, 0);
        let body: Value = serde_json::from_slice(&terminal.body).unwrap();
        assert_eq!(body["code"], "busy");
        terminal.finish_ack_write(true);
        completed.recv().await.unwrap();
        candidates
            .release(&a0, &HashMap::new(), &HashMap::new(), &metrics)
            .await
            .unwrap();
        for corr in 1..=9 {
            assert_eq!(frames.try_recv().unwrap().header.corr, corr);
        }
        assert_eq!(frames.try_recv().unwrap().header.ty, FrameType::Goodbye);
    }

    #[test]
    fn candidate_interception_precedes_all_admission_and_ready_frames_precede_new_ingress() {
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
        assert_eq!(body.matches("candidate_requests.intercept(").count(), 1);
        let hold = body.find("candidate_requests.intercept(").unwrap();
        assert!(hold < body.find("!ingress_route_should_be_processed(").unwrap());
        assert!(hold < body.find("codemode::admit(").unwrap());
        assert!(
            body.find("candidate_requests.next_ready()").unwrap()
                < body.find("frame=reader_rx.recv()").unwrap()
        );
        assert!(body.contains("candidate_requests.release(&a0,&routes,&management_routes"));
    }
}
