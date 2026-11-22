//! The shell job owns the call until terminal settlement, including after yield.

use super::*;
use crate::subc::call_yield::{NotYieldable, YieldAnswer};
use std::sync::atomic::Ordering;

impl Service {
    pub(super) fn yield_shell(&mut self, frame: Frame, body: &Value, binding: &Binding) {
        let parsed = call_yield::parse_request(body);
        let result = match parsed {
            Ok(_) if !binding.call_yield => Ok((
                frame,
                YieldAnswer::NotYieldable(NotYieldable::NotNegotiated),
            )),
            Ok(request)
                if !matches!(
                    request.reason.as_str(),
                    call_yield::PERSON_MESSAGE | call_yield::RUNNER_RESTART
                ) =>
            {
                Ok((frame, YieldAnswer::NotYieldable(NotYieldable::Reason)))
            }
            Ok(request) => {
                let route = route_key(frame.header.channel, frame.header.epoch);
                match self.bash.request_yield(route, &request.call_key, frame) {
                    Ok(()) => return,
                    Err(reply) => Ok(reply),
                }
            }
            Err(error) => Err((frame, error)),
        };
        let edge = self.edge.clone();
        self.jobs.spawn(async move {
            match result {
                Ok((frame, answer)) => edge.yield_answer(&frame, answer).await,
                Err((frame, error)) => edge.error(&frame, error).await,
            };
            None
        });
    }
}

impl Edge {
    async fn yield_answer(&self, frame: &Frame, answer: YieldAnswer) {
        let body = serde_json::to_vec(&answer.body()).expect("yield answer is JSON");
        if let Ok(reply) = Frame::build_with_version(
            frame.header.ver,
            FrameType::Response,
            frame.header.flags,
            frame.header.channel,
            frame.header.epoch,
            frame.header.corr,
            body,
        ) {
            let _ = send_reliable_writer_frame(
                &self.writer,
                &self.metrics,
                reply,
                "remote yield answer",
            )
            .await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_shell(
        &self,
        call: Call,
        root: ProjectRootId,
        key: ledger::Key,
        shell: Shell,
        decision: &'static str,
        grant: Option<String>,
        jobs: Running,
    ) {
        let frame = &call.frame;
        let route = route_key(frame.header.channel, frame.header.epoch);
        let mut job = jobs.register(route, key.call_key.clone(), grant.clone());
        let record = call.record.clone();
        let begin_key = key.clone();
        let ctx = self.executor.actor_context(&root);
        #[cfg(all(test, target_os = "linux"))]
        let test_launcher = self.test_launcher.clone();
        let prepared = blocking(move || {
            let ctx = ctx.ok_or_else(not_working)?;
            let workdir = shell.check(&ctx)?;
            use crate::sandbox_spawn::remote::prepare;
            #[cfg(all(test, target_os = "linux"))]
            let prepare = |ctx: &AppContext, folder: &Path, workdir, command| {
                if let Some(launcher) = test_launcher {
                    aft_engine::test_bridge::bash_remote::sandbox_spawn::remote::prepare_with_launcher(
                        ctx, folder, workdir, command, launcher,
                    )
                } else {
                    prepare(ctx, folder, workdir, command)
                }
            };
            let launch = prepare(&ctx, &shell.folder, workdir, shell.command).map_err(|error| {
                log::warn!("remote shell setup: {error}");
                remote_bash::unavailable()
            })?;
            record.begin_with_grant(&begin_key, decision, grant.as_deref())?;
            Ok((launch, shell.timeout))
        })
        .await;
        let started = prepared.and_then(|(launch, timeout)| {
            if call.request_cancel.token.is_cancelled() {
                return Err(cancelled());
            }
            if call.lifetime.ended() || job.revoked.is_cancelled() {
                return Err(authorization::denied());
            }
            if call.route_cancel.is_cancelled() {
                return Err(admission_refusal());
            }
            launch
                .start(timeout)
                .map(|child| (child, timeout))
                .map_err(|error| {
                    log::warn!("remote shell launch: {error}");
                    remote_bash::unavailable()
                })
        });
        let (child, timeout) = match started {
            Ok(started) => started,
            Err(error) => {
                let error = if call.request_cancel.token.is_cancelled() {
                    cancelled()
                } else {
                    error
                };
                self.refuse(frame, call.record, Some(key), error).await;
                return;
            }
        };
        job.running.store(true, Ordering::Release);
        let stop = CancellationToken::new();
        let _stop_on_drop = stop.clone().drop_guard();
        let stop_child = stop.clone();
        let lifetime = call.lifetime.clone();
        let revoked = job.revoked.clone();
        let mut wait = tokio::spawn(async move {
            let child_wait = child.wait(stop_child.clone());
            tokio::pin!(child_wait);
            tokio::select! {
                biased;
                _ = lifetime.cancelled() => {
                    stop_child.cancel();
                    (child_wait.await, true)
                }
                _ = revoked.cancelled() => {
                    stop_child.cancel();
                    (child_wait.await, true)
                }
                result = &mut child_wait => (result, false),
            }
        });
        let mut held = false;
        let mut yielded = false;
        let mut route_closed = false;
        let mut request_cancelled = false;
        let (output, denied) = loop {
            tokio::select! {
                biased;
                _ = call.request_cancel.token.cancelled(), if !request_cancelled => {
                    request_cancelled = true;
                    stop.cancel();
                }
                pending = &mut job.yield_rx, if !yielded && !request_cancelled => {
                    yielded = true;
                    if let Ok(request) = pending {
                        let hold = call_yield::restart_held_frame(frame.header.ver, route, frame.header.corr, frame.header.flags, None);
                        if let Ok(hold) = hold {
                            let body = hold.body.clone();
                            let record = call.record.clone();
                            let key = key.clone();
                            if blocking(move || record.hold(&key, &body)).await.is_ok() {
                                held = true;
                                let _ = send_reliable_writer_frame(&self.writer, &self.metrics, hold, "remote held shell").await;
                                self.yield_answer(&request, YieldAnswer::Accepted).await;
                                continue;
                            }
                        }
                        self.yield_answer(&request, YieldAnswer::NotYieldable(NotYieldable::NotWaiting)).await;
                    }
                }
                _ = call.route_cancel.cancelled(), if !held && !route_closed => {
                    route_closed = true;
                    stop.cancel();
                }
                output = &mut wait => break output.unwrap_or_else(|error| (Err(error.to_string()), false)),
            }
        };
        // Do not accept a yield once the terminal result has been selected.
        job.running.store(false, Ordering::Release);
        if let Ok(request) = job.yield_rx.try_recv() {
            self.yield_answer(&request, YieldAnswer::UnknownCall).await;
        }
        let result = match output {
            Ok(output) if request_cancelled && held => {
                let error = cancelled();
                let mut result = output.result(&key.call_key, Some(error.message));
                result.response.data["code"] = json!(error.code);
                Ok(result)
            }
            _ if request_cancelled => Err(cancelled()),
            Ok(output) => {
                let line = if denied || route_closed {
                    Some(authorization::denied().message)
                } else if output.timed_out && timeout == remote_bash::MAX_RUNTIME {
                    Some(remote_bash::TIME_LIMIT.to_owned())
                } else {
                    None
                };
                Ok(output.result(&key.call_key, line))
            }
            Err(error) => {
                log::warn!("remote shell did not run to completion: {error}");
                Err(remote_bash::unavailable())
            }
        };
        let reply = self.tool_reply(&call, result);
        // The call ran, even on timeout or cancellation. A not-started reason
        // would make the ledger omit its output from the late-results feed.
        self.settle(frame, call.record, Some(key), reply, decision, None, held)
            .await;
    }
}
