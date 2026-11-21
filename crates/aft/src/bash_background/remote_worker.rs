use super::*;

/// Send a retry's resubmission. Tests can make it fail as a call that never
/// reached the runner (see `seams::refuse_resubmission`).
pub(super) async fn resubmit(
    client: &ExecRemoteClient,
    task: &Arc<BgTask>,
    request: &RunRequest,
    _root: &Path,
    timings: exec::LaunchTimings,
) -> Result<exec::RemoteStream, exec::Error> {
    #[cfg(test)]
    if seams::refuse_resubmission(_root) {
        return Err(exec::Error::Protocol(
            "test: resubmission failed before it was sent".into(),
        ));
    }
    run_diagnosed(client, task, request, timings).await
}

pub(super) async fn run_diagnosed(
    client: &ExecRemoteClient,
    task: &Arc<BgTask>,
    request: &RunRequest,
    timings: exec::LaunchTimings,
) -> Result<exec::RemoteStream, exec::Error> {
    let capture = task.state.lock().ok().and_then(|state| {
        state
            .metadata
            .remote
            .as_ref()
            .filter(|r| r.explicit_runon)
            .map(|_| {
                super::super::runon_capture::Submission::new(
                    task.clone(),
                    Path::new(&request.workspace_key),
                )
            })
    });
    client
        .run_with_diagnostics(
            request,
            Some(exec::RunDiagnostics {
                capture,
                task_id: &task.task_id,
                session: &task.session_id,
                timings,
            }),
        )
        .await
}

impl BgTaskRegistry {
    pub(super) fn cancel_before_dispatch(&self, task: &Arc<BgTask>) {
        if !self.finish_owner_pause_cancel(task, Ok(())) {
            self.remote_terminal_with_note(
                task,
                Verdict::Cancelled,
                None,
                Some("cancelled before remote dispatch to ck-motor; command never started"),
            );
        }
    }
}

pub(super) fn run(
    registry: BgTaskRegistry,
    task: Arc<BgTask>,
    initial: Option<(Result<RunRequest, RemoteRequestError>, LocalFallback)>,
    timings: Option<exec::LaunchTimings>,
) {
    // Counted in the live-connection gauge line: a worker can
    // outlive its task's reported end while it waits for the
    // runner to confirm a cancel, and it holds its connection.
    let _live_worker = aft_base::registration::count_live("remote_worker_thread");
    let run = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|rt| rt.block_on(registry.run_remote_task(task.clone(), initial, timings)))
    };
    #[cfg(test)]
    let result = seams::capture_remote_launch_logs(&task, run);
    #[cfg(not(test))]
    let result = run();
    if let Err(mut error) = result {
        if task.state.lock().is_ok_and(|state| {
            state
                .metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.cancel_requested && remote.job_id.is_none())
        }) {
            error = format!("{PRE_ACCEPTANCE_CANCEL_UNCONFIRMED}: {error}");
        }
        if registry.finish_owner_pause_cancel(&task, Err(error.clone())) {
            task.remote_launch_ready.store(true, Ordering::Release);
            release_worker_owed_cancel_claim(&task.task_id);
            task.remote_worker_running.store(false, Ordering::Release);
            #[cfg(test)]
            seams::note_remote_worker_exit(&task.task_id);
            return;
        }
        let point = task
            .state
            .lock()
            .ok()
            .and_then(|state| state.metadata.remote.as_ref()?.point())
            .filter(|point| point.gap_recovery.is_some());
        let verdict = point
            .map(|point| {
                // Exhausted attach/connect recovery cannot invalidate a
                // terminal already received. Preserve buffered output
                // and mark only its missing sequences as lost.
                let known = exec::grade(&point.gap_recovery.as_ref().unwrap().terminal);
                let mut consumer = exec::StreamConsumer::resume(point.clone());
                match TaskSink::new(&registry, task.clone()) {
                    Ok(mut sink) => consumer.finish_lost_output(&mut sink).unwrap_or(known),
                    Err(_) => known,
                }
            })
            .unwrap_or(Verdict::OutcomeUnknown);
        registry.remote_terminal(&task, verdict, Some(error));
    }
    // Refusals must be rendered from their settled terminal state,
    // not raced against the worker that is still recording them.
    task.remote_launch_ready.store(true, Ordering::Release);
    release_worker_owed_cancel_claim(&task.task_id);
    task.remote_worker_running.store(false, Ordering::Release);
    #[cfg(test)]
    seams::note_remote_worker_exit(&task.task_id);
}
