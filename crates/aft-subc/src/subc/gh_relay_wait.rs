use super::*;

#[derive(Clone, Copy)]
pub(super) enum Stop {
    Cancelled,
    Draining,
}

impl Stop {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Draining => drain::MODULE_DRAINING_ERROR_CODE,
        }
    }

    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "request cancelled",
            Self::Draining => "module is reloading",
        }
    }
}

type Waiters =
    Arc<StdMutex<HashMap<(RouteChannel, u64), tokio::sync::watch::Sender<Option<Stop>>>>>;

#[derive(Default)]
pub(super) struct Requests(Waiters);

impl Requests {
    pub(super) fn register(&self, frame: &Frame, draining: bool) -> Wait {
        let key = (
            route_key(frame.header.channel, frame.header.epoch),
            frame.header.corr,
        );
        let (sender, receiver) = tokio::sync::watch::channel(draining.then_some(Stop::Draining));
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, sender);
        Wait {
            key,
            waiters: self.0.clone(),
            receiver,
        }
    }

    pub(super) fn cancel(&self, route: RouteChannel, corr: u64) -> bool {
        let waiters = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sender) = waiters.get(&(route, corr)) else {
            return false;
        };
        sender.send_if_modified(|stop| {
            if stop.is_some() {
                return false;
            }
            *stop = Some(Stop::Cancelled);
            true
        });
        true
    }

    pub(super) fn drain(&self) {
        for sender in self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            sender.send_if_modified(|stop| {
                if stop.is_some() {
                    return false;
                }
                *stop = Some(Stop::Draining);
                true
            });
        }
    }
}

impl Drop for Requests {
    fn drop(&mut self) {
        self.drain();
    }
}

pub(super) struct Wait {
    key: (RouteChannel, u64),
    waiters: Waiters,
    receiver: tokio::sync::watch::Receiver<Option<Stop>>,
}

impl Wait {
    pub(super) async fn stopped(&mut self) -> Stop {
        loop {
            if let Some(stop) = *self.receiver.borrow_and_update() {
                return stop;
            }
            if self.receiver.changed().await.is_err() {
                return Stop::Draining;
            }
        }
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

pub(super) async fn terminal(
    frame: &Frame,
    operation: &str,
    wait: &mut Wait,
    reply: impl std::future::Future<Output = crate::gh_shim_relay::RelayReply>,
) -> Result<Frame, SubcError> {
    tokio::select! {
        biased;
        stop = wait.stopped() => build_error_frame(
            frame.header.ver, frame.header.channel, frame.header.epoch,
            frame.header.corr, frame.header.flags, stop.code(), stop.message(),
        ),
        reply = reply => {
            let body = json!({
                "op": operation,
                "status": if reply.ok { "ok" } else { "error" },
                "data": reply.data,
            });
            Frame::build_with_version(
                frame.header.ver, FrameType::Response, frame.header.flags,
                frame.header.channel, frame.header.epoch, frame.header.corr,
                serde_json::to_vec(&body).expect("relay reply serializes"),
            ).map_err(SubcError::FrameBuild)
        }
    }
}

#[cfg(test)]
mod gh_shim_relay_tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_and_drain_interrupt_the_reply_wait_with_one_terminal() {
        for cancel in [true, false] {
            let requests = Requests::default();
            let frame =
                Frame::build(FrameType::Request, control_flags(), 3, 7, 42, Vec::new()).unwrap();
            let mut wait = requests.register(&frame, false);
            let waiting = Arc::new(tokio::sync::Notify::new());
            let ready = waiting.clone();
            let caller = tokio::spawn(async move {
                let reply = async {
                    ready.notify_one();
                    tokio::time::sleep(Duration::from_secs(160)).await;
                    crate::gh_shim_relay::RelayReply {
                        ok: true,
                        data: json!({}),
                    }
                };
                terminal(&frame, "gh_shim.bot_request", &mut wait, reply)
                    .await
                    .unwrap()
            });
            waiting.notified().await;
            if cancel {
                assert!(requests.cancel(route_key(3, 7), 42));
            } else {
                requests.drain();
            }
            let terminal = tokio::time::timeout(Duration::from_secs(1), caller)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(terminal.header.ty, FrameType::Error);
            assert_eq!(
                (
                    terminal.header.channel,
                    terminal.header.epoch,
                    terminal.header.corr
                ),
                (3, 7, 42)
            );
            let body: Value = serde_json::from_slice(&terminal.body).unwrap();
            assert_eq!(
                body["code"],
                if cancel {
                    "cancelled"
                } else {
                    "module_reloading"
                }
            );
            assert!(
                requests.0.lock().unwrap().is_empty(),
                "the terminal owner retires its waiter exactly once"
            );
        }
    }
}
