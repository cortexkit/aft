use super::*;

#[derive(Default)]
pub(super) struct BudgetWaits(Arc<StdMutex<HashMap<(RouteChannel, u64), Arc<AdmissionWait>>>>);

pub(in crate::subc) struct Ready {
    decoded: DecodedFrame,
    admission: Arc<AdmissionWait>,
}

fn key(frame: &Frame) -> (RouteChannel, u64) {
    (
        route_key(frame.header.channel, frame.header.epoch),
        frame.header.corr,
    )
}

impl BudgetWaits {
    pub(super) fn start(
        &self,
        decoded: DecodedFrame,
        name: &str,
        scope: Arc<RunScope>,
        background_error: Option<ErrorBody>,
        control: Arc<readiness::DaemonControl>,
        ready: mpsc::UnboundedSender<Ready>,
    ) {
        let admission = Arc::new(AdmissionWait::from_decoded(&decoded, name));
        let key = key(&decoded.frame);
        self.0.lock().unwrap().insert(key, admission.clone());
        let pending = self.0.clone();
        tokio::spawn(async move {
            let result = match background_error {
                Some(error) => Err(error),
                None => scope.read_budget().await,
            };
            match result {
                Ok(_) => {
                    // The request still needs a terminal response after lookup.
                    // The frame loop claims this exact admission on resumption.
                    let _ = ready.send(Ready { decoded, admission });
                }
                Err(error) if claim(&pending, key, &admission) => {
                    let h = &decoded.frame.header;
                    if let Ok(frame) = build_error_frame(
                        h.ver,
                        h.channel,
                        h.epoch,
                        h.corr,
                        h.flags,
                        &error.code,
                        &error.message,
                    ) {
                        let _ = send_reliable_writer_frame(
                            &control.writer,
                            &control.metrics,
                            frame,
                            "run scope refusal",
                        )
                        .await;
                    }
                }
                Err(_) => {}
            }
        });
    }

    pub(super) fn resume(&self, ready: Ready) -> Option<DecodedFrame> {
        claim(&self.0, key(&ready.decoded.frame), &ready.admission).then_some(ready.decoded)
    }

    pub(super) fn cancel(&self, route: RouteChannel, corr: u64) -> Option<RequestFrameMeta> {
        self.0
            .lock()
            .unwrap()
            .remove(&(route, corr))
            .map(|wait| wait.meta)
    }

    pub(super) fn release(&self, route: RouteChannel) -> Result<Vec<Frame>, SubcError> {
        let mut pending = self.0.lock().unwrap();
        let mut replies = Vec::new();
        pending.retain(|key, wait| {
            if key.0 != route {
                return true;
            }
            replies.push(wait.released_frame());
            false
        });
        replies.into_iter().collect()
    }

    pub(super) fn drain(&self) -> Result<Vec<Frame>, SubcError> {
        self.0
            .lock()
            .unwrap()
            .drain()
            .map(|(_, wait)| wait.draining_frame())
            .collect()
    }
}

fn claim(
    pending: &StdMutex<HashMap<(RouteChannel, u64), Arc<AdmissionWait>>>,
    key: (RouteChannel, u64),
    expected: &Arc<AdmissionWait>,
) -> bool {
    let mut pending = pending.lock().unwrap();
    if pending
        .get(&key)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        pending.remove(&key);
        true
    } else {
        false
    }
}
