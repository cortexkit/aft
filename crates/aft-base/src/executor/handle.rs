use super::*;

impl<C: ExecutorContext> Executor<C> {
    pub fn new() -> Self {
        Self::with_config(ExecutorConfig::default())
    }

    pub fn with_config(config: ExecutorConfig) -> Self {
        let effective = config.effective();
        let state = Arc::new(TrackedMutex::new(
            SchedulerState::<C>::new(effective.clone()),
            file!(),
        ));
        let heavy = Arc::new(HeavySemaphore::new(effective.heavy_permits));
        let nonrunnable_dispatches = Arc::new(AtomicUsize::new(0));
        let completed_interactive = Arc::new(AtomicU64::new(0));
        let completed_maintenance = Arc::new(AtomicU64::new(0));
        let dispatch_liveness = Arc::new(DispatchLivenessAtomics::new());
        let (run_tx, run_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let dispatch_loop_liveness = Arc::new(DispatchLoopLiveness::<C>::new(event_rx.clone()));

        let scheduler_event_tx = event_tx.clone();
        let scheduler_loop_liveness = Arc::clone(&dispatch_loop_liveness);
        let scheduler_state = Arc::clone(&state);
        let scheduler_heavy = Arc::clone(&heavy);
        let scheduler_violations = Arc::clone(&nonrunnable_dispatches);
        let scheduler_completed_interactive = Arc::clone(&completed_interactive);
        let scheduler_completed_maintenance = Arc::clone(&completed_maintenance);
        let scheduler_dispatch_liveness = Arc::clone(&dispatch_liveness);
        let scheduler_handle = thread::Builder::new()
            .name("aft-executor-scheduler".to_string())
            .spawn(move || {
                scheduler_loop(
                    scheduler_state,
                    scheduler_heavy,
                    run_tx,
                    scheduler_event_tx,
                    event_rx,
                    scheduler_violations,
                    scheduler_completed_interactive,
                    scheduler_completed_maintenance,
                    scheduler_dispatch_liveness,
                    scheduler_loop_liveness,
                );
            })
            .expect("spawn AFT executor scheduler");

        let mut worker_handles = Vec::with_capacity(effective.total_workers());
        for worker_id in 0..effective.total_workers() {
            let worker_rx = run_rx.clone();
            // Workers run every tool call, including tree-sitter walks over
            // whatever a URL or file turns out to contain. The standalone loop
            // runs those on the 8 MiB main thread; the 2 MiB default for spawned
            // threads let a deep parse tree abort the whole daemon while the same
            // request succeeded standalone. Match the main thread.
            let handle = thread::Builder::new()
                .name(format!("aft-executor-worker-{worker_id}"))
                .stack_size(EXECUTOR_WORKER_STACK_BYTES)
                .spawn(move || worker_loop(worker_rx))
                .expect("spawn AFT executor worker");
            worker_handles.push(handle);
        }

        Self {
            inner: Arc::new(ExecutorInner::<C> {
                state,
                event_tx,
                scheduler_handle: Mutex::new(Some(scheduler_handle)),
                worker_handles: Mutex::new(worker_handles),
                config: effective,
                nonrunnable_dispatches,
                completed_interactive,
                completed_maintenance,
                dispatch_liveness,
                dispatch_loop_liveness,
                #[cfg(any(test, feature = "test-support"))]
                maintenance_root_check_hook: Mutex::new(None),
            }),
        }
    }

    /// Register an actor if one is not already present.
    ///
    /// Existing actors keep their current context and scheduler state; subc
    /// routing reuses them and reconfigures through the Mutating lane
    /// rather than replacing the per-root [`ExecutorContext`]. Returns `true` when a
    /// new actor was inserted.
    pub fn register_actor(&self, root_id: ProjectRootId, ctx: Arc<C>) -> bool {
        ctx.bind_status_context();
        let maintenance =
            RootMaintenanceExecutor::new(Arc::downgrade(&self.inner), root_id.clone());
        let memory_root = root_id.as_path().to_path_buf();
        let inserted = {
            let mut state = self.inner.state.lock();
            if state.actors.contains_key(&root_id) {
                false
            } else {
                state.actor_order.push(root_id.clone());
                state
                    .actors
                    .insert(root_id, ActorState::<C>::new(Arc::clone(&ctx)));
                true
            }
        };
        if inserted {
            ctx.install_callgraph_executor(maintenance);
            let app = ctx.app();
            crate::cache_location::markers::register_live_scope(&ctx.storage_dir(), &memory_root);
            app.register_memory_context(memory_root, &ctx);
            app.actor_root_registered();
        }
        self.wake_scheduler();
        inserted
    }

    /// Remove an actor from scheduler state.
    ///
    /// This is intentionally minimal: subc uses it only for a just-created
    /// RouteBind actor whose configure failed before any route was installed, so
    /// there is no in-flight work to quiesce. The removed [`ExecutorContext`] is
    /// dropped after releasing the scheduler lock so watcher/LSP teardown never
    /// runs under that mutex.
    pub fn remove_actor(&self, root_id: &ProjectRootId) {
        let removed = {
            let mut state = self.inner.state.lock();
            state.actor_order.retain(|actor_root| actor_root != root_id);
            state.actors.remove(root_id)
        };
        if let Some(actor) = removed.as_ref() {
            let app = actor.ctx.app();
            crate::cache_location::markers::unregister_live_scope(
                &actor.ctx.storage_dir(),
                root_id.as_path(),
            );
            app.unregister_memory_context(root_id.as_path(), &actor.ctx);
            app.actor_root_unregistered();
        }
        drop(removed);
        self.wake_scheduler();
    }

    /// Return true only when the actor has no queued or running executor work.
    pub fn actor_is_idle(&self, root_id: &ProjectRootId) -> bool {
        let state = self.inner.state.lock();
        state
            .actors
            .get(root_id)
            .is_some_and(ActorState::<C>::is_idle)
    }

    /// Idle probe for maintenance sweeps that waits at most
    /// [`SCHEDULER_PROBE_WAIT`] for the scheduler lock. A contended scheduler
    /// is reported separately so root retirement can conservatively wait for
    /// the next sweep without stalling the module loop.
    pub fn try_actor_is_idle(&self, root_id: &ProjectRootId) -> Option<bool> {
        let state = self.inner.state.try_lock_for(SCHEDULER_PROBE_WAIT)?;
        Some(
            state
                .actors
                .get(root_id)
                .is_some_and(ActorState::<C>::is_idle),
        )
    }

    /// Forget an idle actor and drop its root-scoped registries off the scheduler
    /// thread. This is reserved for roots whose project directory no longer
    /// exists; retained existing roots are reused on a later bind.
    pub fn retire_idle_actor_in_background(&self, root_id: &ProjectRootId) -> bool {
        let removed = {
            let mut state = self.inner.state.lock();
            if !state
                .actors
                .get(root_id)
                .is_some_and(ActorState::<C>::is_idle)
            {
                return false;
            }
            state.actor_order.retain(|actor_root| actor_root != root_id);
            state.actors.remove(root_id)
        };
        self.finish_idle_actor_retirement(root_id, removed)
    }

    /// [`Self::retire_idle_actor_in_background`] for the subc frame loop:
    /// `None` when the scheduler lock stays contended past
    /// [`SCHEDULER_PROBE_WAIT`], so the caller retries on its next sweep.
    pub fn try_retire_idle_actor_in_background(&self, root_id: &ProjectRootId) -> Option<bool> {
        let removed = {
            let mut state = self.inner.state.try_lock_for(SCHEDULER_PROBE_WAIT)?;
            if !state
                .actors
                .get(root_id)
                .is_some_and(ActorState::<C>::is_idle)
            {
                return Some(false);
            }
            state.actor_order.retain(|actor_root| actor_root != root_id);
            state.actors.remove(root_id)
        };
        Some(self.finish_idle_actor_retirement(root_id, removed))
    }

    fn finish_idle_actor_retirement(
        &self,
        root_id: &ProjectRootId,
        removed: Option<ActorState<C>>,
    ) -> bool {
        let Some(actor) = removed else {
            return false;
        };
        let app = actor.ctx.app();
        crate::cache_location::markers::unregister_live_scope(
            &actor.ctx.storage_dir(),
            root_id.as_path(),
        );
        app.unregister_memory_context(root_id.as_path(), &actor.ctx);
        app.actor_root_unregistered();
        std::thread::spawn(move || {
            actor.ctx.teardown_deleted_root();
            drop(actor);
        });
        self.wake_scheduler();
        true
    }

    /// Cancel maintenance jobs that have not started for one retained actor.
    ///
    /// Interactive work and already-running maintenance remain untouched. Each
    /// cancelled job receives a normal completion so its caller can settle
    /// bookkeeping through the same path as an executed job.
    pub fn cancel_queued_maintenance(&self, root_id: &ProjectRootId) -> usize {
        let cancelled = {
            let mut state = self.inner.state.lock();
            state
                .actors
                .get_mut(root_id)
                .map(|actor| actor.maintenance.cancel_queued_jobs())
                .unwrap_or(0)
        };
        if cancelled > 0 {
            self.wake_scheduler();
        }
        cancelled
    }

    /// When a root becomes unbound, cancel queued maintenance and metadata.
    /// Started provider commits retain their result. Running maintenance checks
    /// at checkpoints whether the root stayed unbound past the abandonment grace.
    pub fn cancel_queued_root_maintenance(&self, root_id: &ProjectRootId) -> usize {
        let cancelled = self
            .inner
            .state
            .lock()
            .actors
            .get_mut(root_id)
            .map(ActorState::<C>::cancel_queued_root_work)
            .unwrap_or(0);
        self.wake_scheduler();
        cancelled
    }

    /// Cancel queued maintenance and signal running root work at its checkpoints.
    /// Interactive tools other than inspect retain their normal completion path.
    pub fn cancel_root_maintenance(&self, root_id: &ProjectRootId) -> usize {
        let mut state = self.inner.state.lock();
        let mut cancelled = state
            .actors
            .get_mut(root_id)
            .map(ActorState::<C>::cancel_queued_root_work)
            .unwrap_or(0);
        if let Some(actor) = state.actors.get_mut(root_id) {
            for lane in ALL_LANES {
                for job in actor
                    .interactive
                    .queue(lane)
                    .iter()
                    .filter(|job| job.command == "inspect")
                {
                    if let Some(token) = &job.cancellation {
                        token.request_cancel();
                        cancelled += 1;
                    }
                }
            }
        }
        for job in state.running_jobs.values() {
            if &job.root_id == root_id
                && (job.job_class == JobClass::Maintenance || job.command == "inspect")
            {
                if let Some(token) = &job.cancellation {
                    token.request_cancel();
                    cancelled += 1;
                }
            }
        }
        drop(state);
        self.wake_scheduler();
        cancelled
    }

    /// Return whether scheduler state currently has an actor for this root.
    pub fn actor_registered(&self, root_id: &ProjectRootId) -> bool {
        let state = self.inner.state.lock();
        state.actors.contains_key(root_id)
    }

    /// Snapshot one actor context without retaining the scheduler lock while
    /// maintenance drops root-scoped resources.
    pub fn actor_context(&self, root_id: &ProjectRootId) -> Option<Arc<C>> {
        let state = self.inner.state.lock();
        state
            .actors
            .get(root_id)
            .map(|actor| Arc::clone(&actor.ctx))
    }

    pub fn scheduler_lock_diagnostics(&self) -> Arc<LockDiagnostics> {
        self.inner.state.diagnostics()
    }

    /// [`Self::actor_context`] waiting at most [`SCHEDULER_PROBE_WAIT`] for
    /// the scheduler lock: the outer `None` means it stayed contended. The subc frame
    /// loop's maintenance sweeps use this and retry on the next sweep. Request
    /// routing keeps the blocking form, because there a missing actor and a
    /// busy scheduler mean different things.
    pub fn try_actor_context(&self, root_id: &ProjectRootId) -> Option<Option<Arc<C>>> {
        let state = self.inner.state.try_lock_for(SCHEDULER_PROBE_WAIT)?;
        Some(
            state
                .actors
                .get(root_id)
                .map(|actor| Arc::clone(&actor.ctx)),
        )
    }

    /// Hold the scheduler state lock for the duration of `while_held`, so a
    /// test can prove a caller does not wait for it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_state_lock_for_test(&self, while_held: impl FnOnce()) {
        let _held = self.inner.state.lock();
        while_held();
    }

    /// Snapshot the registered actor contexts.
    ///
    /// The returned [`Arc`]s keep contexts alive after the scheduler lock is
    /// released, so callers can run teardown without holding executor state.
    pub fn actor_contexts(&self) -> Vec<Arc<C>> {
        let state = self.inner.state.lock();
        state
            .actors
            .values()
            .map(|actor_state| Arc::clone(&actor_state.ctx))
            .collect()
    }

    /// Snapshot the registered root ids paired with their actor contexts.
    pub fn actor_entries(&self) -> Vec<(ProjectRootId, Arc<C>)> {
        let state = self.inner.state.lock();
        state
            .actors
            .iter()
            .map(|(root_id, actor_state)| (root_id.clone(), Arc::clone(&actor_state.ctx)))
            .collect()
    }

    /// Non-blocking variant for the health path: the probe reply must stay
    /// cheap under any load, so it skips the actor list (reported as busy)
    /// rather than waiting on the scheduler state lock.
    pub fn try_actor_entries(&self) -> Option<Vec<(ProjectRootId, Arc<C>)>> {
        let state = self.inner.state.try_lock()?;
        Some(
            state
                .actors
                .iter()
                .map(|(root_id, actor_state)| (root_id.clone(), Arc::clone(&actor_state.ctx)))
                .collect(),
        )
    }

    /// Registered actor count under the scheduler lock. Tests that assert a
    /// bind had no actor side effect use this rather than the try-lock form,
    /// which reports contention (`None`) instead of a count.
    pub fn actor_count(&self) -> usize {
        self.inner.state.lock().actors.len()
    }

    /// Constant-time scheduler contention signal for the health reply path.
    pub fn try_actor_count(&self) -> Option<usize> {
        self.inner.state.try_lock().map(|state| state.actors.len())
    }

    pub fn submit(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
    ) -> CompletionHandle {
        let (completion_tx, completion_rx) = crossbeam_channel::bounded(1);
        self.submit_with_completion(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            job,
            CompletionSender::Sync(completion_tx),
        );
        CompletionHandle { rx: completion_rx }
    }

    pub fn submit_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
    ) -> oneshot::Receiver<Response> {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
        );
        completion_rx
    }

    /// Submit an interactive job with an exact-job cancellation token.
    ///
    /// The returned token cancels THIS job only (queued: removed and settled
    /// with `request_cancelled`; running: signalled cooperatively). The job
    /// observes the token via [`current_job_cancellation`].
    pub fn submit_cancellable_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
    ) -> (oneshot::Receiver<Response>, JobCancellation) {
        let cancellation = JobCancellation::new();
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion_cancellable(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
            Some(cancellation.clone()),
            None,
            None,
        );
        (completion_rx, cancellation)
    }

    /// [`Executor::submit_cancellable_async`] for a tool call: the job is
    /// labelled with the tool's name, so the census and the blockers a
    /// waiting bind reports say which tool a long-running job is running.
    pub fn submit_tool_call_cancellable_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        tool: &str,
        job: ExecutorJob<C>,
    ) -> (oneshot::Receiver<Response>, JobCancellation) {
        let cancellation = JobCancellation::new();
        let completion_rx = self.submit_tool_call_with_cancellation_async(
            root_id,
            lane,
            request_id,
            tool,
            job,
            cancellation.clone(),
        );
        (completion_rx, cancellation)
    }

    /// Allows a receipt deadline to cancel admission even while submission is
    /// waiting for the scheduler state lock on a blocking worker.
    pub fn submit_tool_call_with_cancellation_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        tool: &str,
        job: ExecutorJob<C>,
        cancellation: JobCancellation,
    ) -> oneshot::Receiver<Response> {
        // Checkout callgraph views rebuild as Maintenance/HeavyInit jobs.
        // Admit their tool reads as PureRead so a one-permit pool can answer
        // Building while the rebuild owns the heavy permit.
        let lane = if lane == Lane::HeavyInit
            && matches!(
                tool,
                "callgraph"
                    | "callers"
                    | "call_tree"
                    | "impact"
                    | "trace_to"
                    | "trace_to_symbol"
                    | "trace_data"
            )
            && self.actor_context(&root_id).is_some_and(|_ctx| false)
        {
            Lane::PureRead
        } else {
            lane
        };
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_labeled(
            root_id,
            JobClass::Interactive,
            lane,
            request_id,
            tool.to_string(),
            job,
            CompletionSender::Async(completion_tx),
            Some(cancellation.clone()),
            None,
            None,
        );
        completion_rx
    }

    /// Submit a route-bind configure (a request id starting `subc-bind-`)
    /// that may run more than once, with an exact-job cancellation token.
    ///
    /// Unlike [`Executor::submit_cancellable_async`], the scheduler may admit
    /// it beside a running maintenance job or readers of its actor with only a
    /// shared hold on the epoch gate; see [`request_exclusive_rerun`] for how a run
    /// that must change the root hands its worker back.
    pub fn submit_bind_cancellable_async(
        &self,
        root_id: ProjectRootId,
        request_id: String,
        job: RepeatableJob<C>,
    ) -> (oneshot::Receiver<Response>, JobCancellation) {
        debug_assert!(is_configure_request(&request_id));
        let cancellation = JobCancellation::new();
        let (completion_tx, completion_rx) = oneshot::channel();
        let first_run = Arc::clone(&job);
        self.submit_with_completion_cancellable(
            root_id,
            JobClass::Interactive,
            Lane::Mutating,
            request_id,
            Box::new(move |ctx| first_run(ctx)),
            CompletionSender::Async(completion_tx),
            Some(cancellation.clone()),
            None,
            Some(job),
        );
        (completion_rx, cancellation)
    }

    /// Cancel one exact job by its token.
    ///
    /// Queued jobs are removed and settled immediately; running jobs are
    /// signalled and return through their normal completion path at the next
    /// cooperative checkpoint. Jobs that sealed their commit finish normally.
    pub fn cancel_job(&self, root_id: &ProjectRootId, token: &JobCancellation) -> JobCancelOutcome {
        // Signal BEFORE actor lookup, deliberately: a job whose actor was
        // already torn down (fatal teardown, root removal) must still abort at
        // its next checkpoint, so the token is cancelled regardless. The
        // outcome must then reflect what the signal actually did — reporting
        // NotFound for a token that was RUNNING at signal time would mislead
        // the caller into treating a signalled job as nonexistent.
        let observed = token.signal_cancel();
        let (outcome, settled) = {
            let mut state = self.inner.state.lock();
            let Some(actor) = state.actors.get_mut(root_id) else {
                return match observed {
                    JOB_CANCEL_STATE_COMMITTED => JobCancelOutcome::RunningCommitted,
                    JOB_CANCEL_STATE_RUNNING | JOB_CANCEL_STATE_PENDING => {
                        JobCancelOutcome::RunningSignalled
                    }
                    _ => JobCancelOutcome::NotFound,
                };
            };
            match actor.remove_queued_cancellable(token) {
                Some(queued) => (JobCancelOutcome::QueuedRemoved, Some(queued)),
                None => match observed {
                    // The seal won the race: the job commits and finishes.
                    JOB_CANCEL_STATE_COMMITTED => (JobCancelOutcome::RunningCommitted, None),
                    // RUNNING at signal time, or PENDING at signal time but
                    // dispatched before we took the scheduler lock: either way
                    // the job aborts at its next checkpoint.
                    JOB_CANCEL_STATE_RUNNING | JOB_CANCEL_STATE_PENDING => {
                        (JobCancelOutcome::RunningSignalled, None)
                    }
                    // Already cancelled by an earlier call and no longer queued.
                    _ => (JobCancelOutcome::NotFound, None),
                },
            }
        };
        if let Some(queued) = settled {
            queued.completion.send(Response::error(
                queued.request_id,
                "request_cancelled",
                "request cancelled before execution",
            ));
            self.wake_scheduler();
        }
        outcome
    }

    /// Cancel a route job only while its command closure has not started.
    ///
    /// Detaching a route must discard work that cannot produce a live reply,
    /// while a command already executing remains eligible for session replay.
    /// A dispatched job may already reserve scheduler capacity; its worker sees
    /// the cancelled token before invoking the closure and releases that
    /// reservation through the normal completion event.
    pub fn cancel_job_before_execution(
        &self,
        root_id: &ProjectRootId,
        token: &JobCancellation,
    ) -> PreExecutionCancelOutcome {
        let (outcome, settled) = {
            let mut state = self.inner.state.lock();
            if !token.try_cancel_before_running() {
                return PreExecutionCancelOutcome::AlreadyStarted;
            }
            let settled = state
                .actors
                .get_mut(root_id)
                .and_then(|actor| actor.remove_queued_cancellable(token));
            let outcome = if settled.is_some() {
                PreExecutionCancelOutcome::QueuedRemoved
            } else {
                PreExecutionCancelOutcome::DispatchedCancelled
            };
            (outcome, settled)
        };
        if let Some(queued) = settled {
            queued.completion.send(Response::error(
                queued.request_id,
                "request_cancelled",
                "request cancelled before execution",
            ));
            self.wake_scheduler();
        }
        outcome
    }

    pub fn submit_maintenance_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
    ) -> oneshot::Receiver<Response> {
        self.submit_maintenance_async_with_key(root_id, lane, request_id, job, None)
    }

    /// Final persistence is allowed after unbind so TTL eviction does not lose
    /// retained deltas. This exception does not admit indexing or refresh work.
    pub fn submit_unbound_teardown_async(
        &self,
        root_id: ProjectRootId,
        request_id: String,
        job: ExecutorJob<C>,
    ) -> oneshot::Receiver<Response> {
        let (tx, rx) = oneshot::channel();
        self.submit_labeled(
            root_id,
            JobClass::Maintenance,
            Lane::MaintenanceCommit,
            request_id,
            "unbound-teardown-persist".to_owned(),
            job,
            CompletionSender::Async(tx),
            None,
            None,
            None,
        );
        rx
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn submit_maintenance_cancellable_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
    ) -> (oneshot::Receiver<Response>, JobCancellation) {
        let (completion_tx, completion_rx) = oneshot::channel();
        let cancellation = JobCancellation::new();
        self.submit_with_completion_cancellable(
            root_id,
            JobClass::Maintenance,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
            Some(cancellation.clone()),
            None,
            None,
        );
        (completion_rx, cancellation)
    }

    pub fn submit_coalescable_maintenance_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        coalesce_key: MaintenanceCoalesceKey,
        job: ExecutorJob<C>,
    ) -> oneshot::Receiver<Response> {
        self.submit_maintenance_async_with_key(root_id, lane, request_id, job, Some(coalesce_key))
    }

    fn submit_maintenance_async_with_key(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
        coalesce_key: Option<MaintenanceCoalesceKey>,
    ) -> oneshot::Receiver<Response> {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion_cancellable(
            root_id,
            JobClass::Maintenance,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
            None,
            coalesce_key,
            None,
        );
        completion_rx
    }

    fn submit_with_completion(
        &self,
        root_id: ProjectRootId,
        job_class: JobClass,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
        completion: CompletionSender,
    ) {
        self.submit_with_completion_cancellable(
            root_id, job_class, lane, request_id, job, completion, None, None, None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_with_completion_cancellable(
        &self,
        root_id: ProjectRootId,
        job_class: JobClass,
        lane: Lane,
        request_id: String,
        job: ExecutorJob<C>,
        completion: CompletionSender,
        cancellation: Option<JobCancellation>,
        maintenance_coalesce_key: Option<MaintenanceCoalesceKey>,
        rerun: Option<RepeatableJob<C>>,
    ) {
        // Final LSP receipts must drain after the last route closes, even
        // though configure, watcher and indexing maintenance are suspended.
        let command = if job_class == JobClass::Maintenance
            && maintenance_coalesce_key == Some(MaintenanceCoalesceKey::LspDrain)
        {
            "lsp-event-drain".to_owned()
        } else {
            job_command(job_class, lane)
        };
        self.submit_labeled(
            root_id,
            job_class,
            lane,
            request_id,
            command,
            job,
            completion,
            cancellation,
            maintenance_coalesce_key,
            rerun,
        );
    }

    /// Queue one job. `command` is the label census, blocker and panic lines
    /// show for it.
    #[allow(clippy::too_many_arguments)]
    fn submit_labeled(
        &self,
        root_id: ProjectRootId,
        job_class: JobClass,
        lane: Lane,
        request_id: String,
        command: String,
        job: ExecutorJob<C>,
        completion: CompletionSender,
        cancellation: Option<JobCancellation>,
        maintenance_coalesce_key: Option<MaintenanceCoalesceKey>,
        rerun: Option<RepeatableJob<C>>,
    ) {
        let cancellation = if job_class == JobClass::Maintenance
            || command == "inspect"
            || lane == Lane::ProviderMetadata
        {
            Some(cancellation.unwrap_or_else(JobCancellation::new))
        } else {
            cancellation
        };
        let mut cancellation = cancellation.map(|token| token.with_root(root_id.as_path()));
        let mut rerun = rerun;
        let mut job = Some(job);
        let mut completion = Some(completion);
        let mut duplicate_victims = Vec::new();

        // A root stat can stall on disk I/O. Probe before taking the shared
        // scheduler lock; execution checks again if the root disappears later.
        let maintenance_root_exists =
            job_class != JobClass::Maintenance || self.maintenance_root_is_dir(&root_id);
        let response = {
            let mut state = self.inner.state.lock();
            match state.actors.get_mut(&root_id) {
                Some(actor) if actor.fatal => Some(actor_fatal_response(request_id.clone())),
                Some(actor) => {
                    // Validate the sampled configure generation against the bound
                    // root under its lifecycle lock. A separate bound check and
                    // later sample could let an older request use a rebind's generation.
                    let provider_generation = if lane == Lane::ProviderMetadata {
                        let generation = actor.ctx.configure_generation();
                        actor
                            .ctx
                            .run_if_subc_bound_generation(generation, || generation)
                    } else {
                        None
                    };
                    if (job_class == JobClass::Maintenance
                        || command == "inspect"
                        || lane == Lane::ProviderMetadata)
                        && !maintenance_allowed_while_unbound(&command)
                    {
                        cancellation = cancellation.map(|token| {
                            token.with_lifecycle(actor.ctx.subc_lifecycle_admission())
                        });
                    }
                    let mut admission_error = None;
                    if lane == Lane::CallLedger && job_class != JobClass::Interactive {
                        admission_error = Some(Response::error(
                            request_id.clone(),
                            "invalid_lane",
                            "call ledger work requires interactive admission",
                        ));
                    }
                    if lane == Lane::ProviderMetadata {
                        if job_class != JobClass::Interactive {
                            admission_error = Some(Response::error(
                                request_id.clone(),
                                "invalid_lane",
                                "provider metadata requires interactive admission",
                            ));
                        } else if provider_generation.is_none() {
                            admission_error = Some(Response::error(
                                request_id.clone(),
                                "request_cancelled",
                                "provider metadata root is unbound",
                            ));
                        }
                    }
                    if job_class == JobClass::Maintenance {
                        if (actor.ctx.subc_unbound_quiesced()
                            && !maintenance_allowed_while_unbound(&command))
                            || !maintenance_root_exists
                        {
                            admission_error = Some(maintenance_cancelled_response(
                                request_id.clone(),
                                "maintenance root is unbound or missing",
                            ));
                        } else if maintenance_coalesce_key
                            .is_some_and(|key| actor.maintenance.has_maintenance_coalesce_key(key))
                        {
                            admission_error = Some(maintenance_cancelled_response(
                                request_id.clone(),
                                "maintenance drain coalesced behind an identical queued drain",
                            ));
                        } else {
                            if actor.maintenance.queued_count() >= MAINTENANCE_QUEUE_CAP {
                                duplicate_victims =
                                    actor.maintenance.remove_duplicate_maintenance_jobs();
                            }
                            if actor.maintenance.queued_count() >= MAINTENANCE_QUEUE_CAP {
                                admission_error =
                                    Some(maintenance_backpressure_response(request_id.clone()));
                            }
                        }
                    }

                    if admission_error.is_none() {
                        actor.push_job(
                            job_class,
                            lane,
                            QueuedJob::<C> {
                                job: job.take().expect("executor job already queued"),
                                completion: completion
                                    .take()
                                    .expect("executor completion already queued"),
                                request_id: request_id.clone(),
                                command,
                                queued_at: Instant::now(),
                                cancellation: cancellation.clone(),
                                maintenance_coalesce_key,
                                rerun: rerun.take(),
                                force_exclusive: false,
                                not_before: None,
                                exclusive_attempts: 0,
                                provider_generation,
                            },
                        );
                    }
                    admission_error
                }
                None => Some(Response::error(
                    request_id.clone(),
                    "actor_not_registered",
                    "executor actor is not registered",
                )),
            }
        };

        for victim in duplicate_victims {
            victim.completion.send(maintenance_cancelled_response(
                victim.request_id,
                "duplicate maintenance drain removed to preserve queue capacity",
            ));
        }

        if let Some(response) = response {
            if let Some(completion) = completion {
                completion.send(response);
            }
            return;
        }

        self.wake_scheduler();
    }

    pub(super) fn maintenance_root_is_dir(&self, root_id: &ProjectRootId) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            let hook = self.inner.maintenance_root_check_hook.lock().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        root_id.as_path().is_dir()
    }

    /// Workers not running a job right now, for tests that assert nothing
    /// holds a worker while it waits.
    #[cfg(any(test, feature = "test-support"))]
    pub fn idle_workers_for_test(&self) -> usize {
        self.inner.state.lock().idle_workers
    }

    /// Number of workers reserved for provider metadata, so tests can distinguish
    /// those idle workers from the separate bind reservation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn provider_reserve_for_test(&self) -> usize {
        self.inner.config.provider_reserve
    }

    pub fn pool_size(&self) -> usize {
        self.inner.config.pool_size
    }

    pub fn actor_cap(&self) -> usize {
        self.inner.config.actor_cap
    }

    pub fn read_cap(&self) -> usize {
        self.inner.config.read_cap
    }

    pub fn heavy_permits(&self) -> usize {
        self.inner.config.heavy_permits
    }

    pub fn interactive_reserve(&self) -> usize {
        self.inner.config.interactive_reserve
    }

    pub fn maintenance_cap(&self) -> usize {
        self.inner.config.maintenance_cap
    }

    pub fn try_dispatch_liveness_snapshot(&self) -> Option<DispatchLivenessSnapshot> {
        Some(self.inner.dispatch_liveness.snapshot(&self.inner.config))
    }

    /// In-memory read-job identities for management attribution; contention stays unknown.
    pub fn try_running_read_requests(&self) -> Option<HashSet<String>> {
        self.inner.state.try_lock().map(|state| {
            state
                .running_jobs
                .values()
                .filter(|job| {
                    matches!(job.lane, Lane::PureRead | Lane::SerialLspStatus)
                        && job.execution_started.load(Ordering::Acquire)
                })
                .map(|job| job.request_id.clone())
                .collect()
        })
    }

    pub fn try_mutating_lane_snapshots(&self) -> Option<Vec<MutatingLaneSnapshot>> {
        self.inner
            .state
            .try_lock()
            .map(|state| state.mutating_lane_snapshots())
    }

    pub fn try_root_job_snapshots(
        &self,
        root_id: &ProjectRootId,
        current: Option<&JobCancellation>,
    ) -> Option<Vec<RootJobSnapshot>> {
        let state = self.inner.state.try_lock()?;
        let now = Instant::now();
        let mut jobs = state
            .running_jobs
            .iter()
            .filter(|(_, job)| &job.root_id == root_id)
            .map(|(job_id, job)| RootJobSnapshot {
                job_id: *job_id,
                request_id: job.request_id.clone(),
                command: job.command.clone(),
                lane: job.lane,
                started_age_ms: duration_millis_u64(now.saturating_duration_since(job.started_at)),
                execution_started: job.execution_started.load(Ordering::Acquire),
                is_current: current.is_some_and(|current| {
                    job.cancellation
                        .as_ref()
                        .is_some_and(|token| token.same_token(current))
                }),
            })
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.job_id);
        Some(jobs)
    }

    pub fn try_mutating_job_state_label(
        &self,
        root_id: &ProjectRootId,
        request_id: &str,
    ) -> Option<&'static str> {
        self.inner
            .state
            .try_lock()
            .map(|state| state.mutating_job_state_label(root_id, request_id))
    }

    /// The lane in which a job with this request id is queued on `root_id`,
    /// for tests that hold a lane and must see the job wait behind it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn queued_lane_for_test(&self, root_id: &ProjectRootId, request_id: &str) -> Option<Lane> {
        let state = self.inner.state.lock();
        let actor = state.actors.get(root_id)?;
        for queues in [&actor.interactive, &actor.maintenance] {
            for lane in ALL_LANES {
                if queues
                    .queue(lane)
                    .iter()
                    .any(|job| job.request_id == request_id)
                {
                    return Some(lane);
                }
            }
        }
        None
    }

    /// Snapshot RouteBind blockers, waiting for the scheduler lock if it is
    /// held. The module's bind-deadline refusal uses it to name what held the
    /// bind; it takes the same lock right after to cancel the bind, so
    /// waiting here costs nothing extra.
    pub fn bind_blocker_snapshot(
        &self,
        root_id: &ProjectRootId,
        request_id: &str,
    ) -> BindBlockerSnapshot {
        self.inner
            .state
            .lock()
            .bind_blocker_snapshot(root_id, request_id)
    }

    /// Snapshot RouteBind blockers without waiting on scheduler state. The subc
    /// health path uses this only for a delayed-bind breadcrumb, so contention
    /// is reported as scheduler busy rather than delaying the transport loop.
    pub fn try_bind_blocker_snapshot(
        &self,
        root_id: &ProjectRootId,
        request_id: &str,
    ) -> Option<BindBlockerSnapshot> {
        self.inner
            .state
            .try_lock()
            .map(|state| state.bind_blocker_snapshot(root_id, request_id))
    }

    /// Return each over-age interactive occupant once. Marking happens under the
    /// scheduler lock so repeated transport-loop censuses cannot flood logs while
    /// the same job remains stuck.
    pub fn try_take_long_running_interactive_jobs(
        &self,
        minimum_age: Duration,
    ) -> Option<Vec<LongRunningInteractiveJobSnapshot>> {
        let now = Instant::now();
        let mut state = self.inner.state.try_lock()?;
        let mut snapshots = state
            .running_jobs
            .values_mut()
            .filter_map(|job| {
                let age = now.saturating_duration_since(job.started_at);
                if job.job_class != JobClass::Interactive
                    || job.occupancy_reported
                    || age < minimum_age
                {
                    return None;
                }
                job.occupancy_reported = true;
                Some(LongRunningInteractiveJobSnapshot {
                    root_id: job.root_id.clone(),
                    request_id: job.request_id.clone(),
                    command: job.command.clone(),
                    lane: job.lane,
                    age,
                    execution_started: job.execution_started.load(Ordering::Acquire),
                })
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.root_id
                .as_path()
                .cmp(right.root_id.as_path())
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        Some(snapshots)
    }

    /// Progress marker of the scheduler loop, for the subc stall watchdog.
    pub fn dispatch_loop_liveness(&self) -> Arc<DispatchLoopLiveness<C>> {
        Arc::clone(&self.inner.dispatch_loop_liveness)
    }

    /// Holds the scheduler state mutex on a helper thread and wakes the
    /// scheduler, so the scheduler loop receives an event and then blocks on
    /// the lock: a stalled dispatch loop with pending work. The hold ends when
    /// `release` receives a message or its sender is dropped.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_dispatch_loop_for_test(&self, release: Receiver<()>) -> JoinHandle<()> {
        let state = Arc::clone(&self.inner.state);
        let event_tx = self.inner.event_tx.clone();
        let (held_tx, held_rx) = crossbeam_channel::bounded(1);
        let handle = thread::spawn(move || {
            let _state = state.lock();
            let _ = event_tx.send(SchedulerEvent::<C>::Wake);
            let _ = held_tx.send(());
            let _ = release.recv();
        });
        held_rx.recv().expect("dispatch hold thread started");
        handle
    }

    pub fn nonrunnable_dispatch_count(&self) -> usize {
        self.inner.nonrunnable_dispatches.load(Ordering::Acquire)
    }

    pub fn completion_counts(&self) -> (u64, u64) {
        (
            self.inner.completed_interactive.load(Ordering::Relaxed),
            self.inner.completed_maintenance.load(Ordering::Relaxed),
        )
    }

    pub fn actor_is_fatal(&self, root_id: &ProjectRootId) -> bool {
        self.inner
            .state
            .lock()
            .actors
            .get(root_id)
            .map(|actor| actor.fatal)
            .unwrap_or(false)
    }

    fn wake_scheduler(&self) {
        let _ = self.inner.event_tx.send(SchedulerEvent::<C>::Wake);
    }
}
