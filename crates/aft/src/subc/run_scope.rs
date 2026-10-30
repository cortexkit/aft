//! Bound long-running codemode work to its daemon-authenticated run deadline
//! and stop its processes only after the daemon confirms the run has ended.

use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct RunBudget {
    deadline: Option<Instant>,
    #[cfg(test)]
    now_for_test: Option<Instant>,
}

impl RunBudget {
    #[cfg(test)]
    fn from_record(record: &subc_protocol::scope::ScopeRecord) -> Self {
        Self::from_expires_at_ms(record.expires_at_ms)
    }

    fn from_expires_at_ms(expires_at_ms: Option<u64>) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            deadline: expires_at_ms
                .map(|at| Instant::now() + Duration::from_millis(at.saturating_sub(now_ms))),
            #[cfg(test)]
            now_for_test: None,
        }
    }

    /// Fit a codemode program's shell call to its run. A PTY needs someone to
    /// drive it interactively, so it is refused. A background command is
    /// allowed: it joins its owner route's session (the head agent's, or a
    /// worker run's own worker's), so that owner can read it with `bash_status`, but it sends no completion wake, because the
    /// program is the one waiting for it; the run's end kills it. A
    /// foreground command blocks until it completes, and its wait is cut at
    /// the run's deadline.
    pub(super) fn constrain_bash(
        &self,
        args: &mut serde_json::Map<String, Value>,
    ) -> Result<(), ErrorBody> {
        if args.get("pty").and_then(Value::as_bool) == Some(true) {
            return Err(ErrorBody::new(
                "run_pty_refused",
                "codemode runs can't open a PTY; nobody can drive it interactively",
            ));
        }
        let remaining = self.deadline.map(|deadline| {
            let now = Instant::now();
            #[cfg(test)]
            let now = self.now_for_test.unwrap_or(now);
            deadline.saturating_duration_since(now).as_millis() as u64
        });
        if remaining == Some(0) {
            return Err(ErrorBody::new(
                "run_deadline_expired",
                "the codemode run's time budget has expired",
            ));
        }
        if args.get("background").and_then(Value::as_bool) == Some(true) {
            args.insert("notify_on_completion".into(), json!(false));
            return Ok(());
        }
        if let Some(remaining) = remaining {
            let normal = args
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(crate::bash_background::registry::DEFAULT_BG_TIMEOUT.as_millis() as u64);
            args.insert("timeout".into(), json!(normal.min(remaining)));
        }
        args.insert("block_to_completion".into(), json!(true));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(ms: Option<u64>) -> RunBudget {
        let record = serde_json::from_value(json!({
            "ref": "run-scope", "scope_epoch": 1, "kind": "worker",
            "expires_at_ms": ms.map(|ms| {
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .unwrap().as_millis() as u64 + ms
            })
        }))
        .unwrap();
        RunBudget::from_record(&record)
    }

    #[test]
    fn run_pty_is_refused_by_name() {
        let mut args = json!({"pty": true}).as_object().unwrap().clone();
        let error = budget(Some(5000)).constrain_bash(&mut args).unwrap_err();
        assert_eq!(error.code, "run_pty_refused");
        assert!(error.message.contains("codemode runs can't open a PTY"));
    }

    #[test]
    fn run_background_launches_without_a_completion_wake() {
        let mut args = json!({"background": true, "timeout": 60000, "notify_on_completion": true})
            .as_object()
            .unwrap()
            .clone();
        budget(Some(5000)).constrain_bash(&mut args).unwrap();
        assert_eq!(args["notify_on_completion"], false);
        assert!(args.get("block_to_completion").is_none());
        assert_eq!(args["timeout"], 60000);
    }

    #[test]
    fn run_timeout_is_clamped_to_remaining_time() {
        let mut args = json!({"timeout": 5000}).as_object().unwrap().clone();
        budget(Some(1000)).constrain_bash(&mut args).unwrap();
        let timeout = args["timeout"].as_u64().unwrap();
        assert!(timeout > 0 && timeout <= 1000, "{timeout}");
        assert_eq!(args["block_to_completion"], true);
        let mut shorter = json!({"timeout": 1}).as_object().unwrap().clone();
        budget(Some(1000)).constrain_bash(&mut shorter).unwrap();
        assert_eq!(shorter["timeout"], 1);
    }

    #[test]
    fn run_missing_deadline_falls_back_only_for_timeout() {
        let mut args = json!({"command": "sleep 1"}).as_object().unwrap().clone();
        budget(None).constrain_bash(&mut args).unwrap();
        assert!(args.get("timeout").is_none());
        assert_eq!(args["block_to_completion"], true);
        let mut pty = json!({"pty": true}).as_object().unwrap().clone();
        assert_eq!(
            budget(None).constrain_bash(&mut pty).unwrap_err().code,
            "run_pty_refused"
        );
    }
}

pub(super) struct RunScope {
    stamp: subc_protocol::scope::ScopeStamp,
    control: Arc<readiness::DaemonControl>,
    describe_lock: tokio::sync::Mutex<()>,
    budget: StdMutex<Option<RunBudget>>,
    revoked: AtomicBool,
    /// Permission asks this run's calls left open with `access_pending`.
    /// They are withdrawn once the run's scope is confirmed ended.
    parked_asks: StdMutex<Vec<super::consent::ParkedAsk>>,
}

impl RunScope {
    /// Whether scope.describe confirmed that the run's scope ended.
    pub(super) fn revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Keep an ask the run left open so the run's end withdraws it. An ask
    /// parked after the end was confirmed is withdrawn at once.
    pub(super) fn park_ask(self: &Arc<Self>, ask: super::consent::ParkedAsk) {
        self.parked_asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ask);
        if self.revoked() {
            self.withdraw_parked_asks();
        }
    }

    fn withdraw_parked_asks(&self) {
        let parked = std::mem::take(
            &mut *self
                .parked_asks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if parked.is_empty() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(super::consent::withdraw_parked(parked));
        }
    }

    pub(super) fn cached_budget(&self) -> Result<RunBudget, ErrorBody> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(ErrorBody::new(
                "run_scope_revoked",
                "the codemode run's scope has ended",
            ));
        }
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ok_or_else(|| {
                ErrorBody::new(
                    "run_scope_unavailable",
                    "run scope record has not been read",
                )
            })
    }

    async fn read_budget(&self) -> Result<RunBudget, ErrorBody> {
        tokio::time::timeout(Duration::from_secs(3), self.read_budget_serial())
            .await
            .map_err(|_| {
                ErrorBody::new(
                    "run_scope_unavailable",
                    "run scope read timed out waiting for describe",
                )
            })?
    }

    async fn read_budget_serial(&self) -> Result<RunBudget, ErrorBody> {
        let _describe = self.describe_lock.lock().await;
        if self.revoked() {
            return self.cached_budget();
        }
        if self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return self.cached_budget();
        }
        let reply = self
            .control
            .describe(&self.stamp)
            .await
            .map_err(|error| ErrorBody::new("run_scope_unavailable", error.to_string()))?;
        use subc_protocol::{scope::ScopeStatus, session::ModuleControlResponseToModule};
        let ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            scope_epoch: Some(epoch),
            scope: Some(stamp),
            ..
        } = &reply
        else {
            return Err(ErrorBody::new(
                "run_scope_unavailable",
                "scope.describe did not confirm a live run scope",
            ));
        };
        if *epoch != self.stamp.scope_epoch
            || stamp.scope_epoch != *epoch
            || stamp.owner != self.stamp.owner
            || stamp.scope_ref != self.stamp.scope_ref
        {
            return Err(ErrorBody::new(
                "run_scope_unavailable",
                "scope.describe did not match the route's run scope",
            ));
        }
        let expires_at_ms = match (self.stamp.expires_at_ms, deadline_from_describe(stamp)) {
            (Some(bound), Some(described)) => Some(bound.min(described)),
            (bound, _) => bound,
        };
        let budget = RunBudget::from_expires_at_ms(expires_at_ms);
        *self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(budget);
        self.cached_budget()
    }
}

pub(super) struct RunScopes {
    control: Arc<readiness::DaemonControl>,
    scopes: StdMutex<HashMap<(String, String, u64), Arc<RunScope>>>,
}

impl RunScopes {
    pub(super) fn new(control: Arc<readiness::DaemonControl>) -> Self {
        Self {
            control,
            scopes: StdMutex::new(HashMap::new()),
        }
    }

    pub(super) fn revoke(&self, stamp: &subc_protocol::scope::ScopeStamp) {
        if let Some(scope) = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&super::scope_describe::stamp_key(stamp))
        {
            scope.revoked.store(true, Ordering::Release);
            scope.withdraw_parked_asks();
        }
    }

    /// Pending Cingulate asks keep a run alive for scope confirmation even
    /// when the run currently has no shell process to watch.
    pub(super) fn parked_scopes(&self) -> Vec<subc_protocol::scope::ScopeStamp> {
        self.scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|scope| {
                !scope.revoked()
                    && !scope
                        .parked_asks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_empty()
            })
            .map(|scope| scope.stamp.clone())
            .collect()
    }

    pub(super) fn bind(&self, identity: &RouteIdentity) -> Option<Arc<RunScope>> {
        let stamp = identity
            .scope
            .as_ref()
            .filter(|_| identity.call_mode == subc_protocol::call_mode::CallMode::Codemode)?;
        Some(Arc::clone(identity.run_scope.get_or_init(|| {
            let key = super::scope_describe::stamp_key(stamp);
            Arc::clone(
                self.scopes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(key)
                    .or_insert_with(|| {
                        Arc::new(RunScope {
                            stamp: stamp.clone(),
                            control: Arc::clone(&self.control),
                            describe_lock: tokio::sync::Mutex::new(()),
                            budget: StdMutex::new(None),
                            revoked: AtomicBool::new(false),
                            parked_asks: StdMutex::new(Vec::new()),
                        })
                    }),
            )
        })))
    }
}

fn deadline_from_describe(stamp: &subc_protocol::scope::ScopeStamp) -> Option<u64> {
    stamp.expires_at_ms
}

impl RunScopes {
    /// Defer only long operations. Admission and permissions still use the
    /// agent's existing catalog and policy; runs add lifetime controls only.
    pub(super) fn defer(
        &self,
        decoded: DecodedFrame,
        identity: &RouteIdentity,
        ready: &mpsc::UnboundedSender<DecodedFrame>,
    ) -> Result<(), DecodedFrame> {
        let body = serde_json::from_slice::<Value>(&decoded.frame.body).unwrap_or(Value::Null);
        let name = body.get("name").and_then(Value::as_str).unwrap_or("");
        if body
            .get("op")
            .and_then(Value::as_str)
            .is_some_and(|op| op != "tool.call")
            || super::tool_provider::recognized_operation(name)
        {
            return Err(decoded);
        }
        if let Err(error) = super::tool_provider::admit_call_mode(name, &identity.call_mode) {
            let writer = self.control.writer.clone();
            let metrics = Arc::clone(&self.control.metrics);
            tokio::spawn(async move {
                let _ = super::send_provider_error(&writer, &metrics, &decoded.frame, error).await;
            });
            return Ok(());
        }
        if identity.call_mode == subc_protocol::call_mode::CallMode::Codemode
            && matches!(name, "bash" | "powershell")
            && identity
                .scope
                .as_ref()
                .and_then(|scope| scope.expires_at_ms)
                .is_none()
        {
            let mut error = ErrorBody::new(
                "run_deadline_missing",
                "codemode bash requires the run deadline on its bind",
            );
            error.detail = Some(json!({"tool":name,"mode":identity.call_mode}));
            let writer = self.control.writer.clone();
            let metrics = Arc::clone(&self.control.metrics);
            tokio::spawn(async move {
                let _ = super::send_provider_error(&writer, &metrics, &decoded.frame, error).await;
            });
            return Ok(());
        }
        let Some(scope) = self.bind(identity) else {
            return Err(decoded);
        };
        // Scope-end refusal also covers short calls, so a parked consent ask
        // cannot be retried into an execution after its run was revoked.
        if !scope.revoked()
            && !matches!(
                name,
                "bash" | "powershell" | "bash_watch" | "inspect" | "delete"
            )
            && !crate::commands::lsp_navigation::is_lsp_navigation_command(name)
        {
            return Err(decoded);
        }
        let raw_args = body
            .get("arguments")
            .or_else(|| body.get("params"))
            .unwrap_or(&Value::Null);
        let args = raw_args.get("params").unwrap_or(raw_args);
        let background_error = if matches!(name, "bash" | "powershell" | "bash_watch") {
            let mut args = args.as_object().cloned().unwrap_or_default();
            RunBudget {
                deadline: None,
                #[cfg(test)]
                now_for_test: None,
            }
            .constrain_bash(&mut args)
            .err()
        } else {
            None
        };
        if background_error.is_none() && scope.cached_budget().is_ok() {
            return Err(decoded);
        }
        let control = Arc::clone(&self.control);
        let ready = ready.clone();
        tokio::spawn(async move {
            let result = match background_error {
                Some(error) => Err(error),
                None => scope.read_budget().await,
            };
            match result {
                Ok(_) => {
                    let _ = ready.send(decoded);
                }
                Err(error) => {
                    let frame = &decoded.frame;
                    if let Ok(refusal) = build_error_frame(
                        frame.header.ver,
                        frame.header.channel,
                        frame.header.epoch,
                        frame.header.corr,
                        frame.header.flags,
                        &error.code,
                        &error.message,
                    ) {
                        let _ = send_reliable_writer_frame(
                            &control.writer,
                            &control.metrics,
                            refusal,
                            "run scope refusal",
                        )
                        .await;
                    }
                }
            }
        });
        Ok(())
    }
}

impl fmt::Debug for RunScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunScope")
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}

impl RunBudget {
    pub(super) fn guard_pending(&self, pending: PendingResponse) -> PendingResponse {
        let Some(deadline) = self.deadline else {
            return pending;
        };
        let mut pending = pending.with_poll_interval(PENDING_POLL_INTERVAL);
        let mut poll = pending.poll;
        let cancellation = pending.cancellation.clone();
        let id = pending.request_id.clone();
        pending.poll = Box::new(move |ctx| {
            if Instant::now() >= deadline {
                if let Some(cancel) = &cancellation {
                    cancel.request_cancel();
                }
                Some(Response::error(
                    &id,
                    "run_deadline_expired",
                    "the codemode run's time budget has expired",
                ))
            } else {
                poll(ctx)
            }
        });
        pending
    }
}

#[cfg(test)]
impl RunScope {
    pub(super) fn for_test(deadline: Option<Instant>) -> Arc<Self> {
        Self::for_test_with_clock(deadline, None)
    }

    // Freeze only the budget constraint's clock. Process startup and the
    // registry's hard-kill timer still run normally with the clamped timeout.
    pub(super) fn for_test_with_clock(
        deadline: Option<Instant>,
        now: Option<Instant>,
    ) -> Arc<Self> {
        let (writer, _) = mpsc::channel(8);
        let scopes = RunScopes::new(Arc::new(readiness::DaemonControl::new(
            writer,
            Arc::new(DispatchPathMetrics::new()),
            PROTOCOL_VERSION,
            vec![],
            vec![],
            readiness::PendingControlReplies::default(),
            HELLO_CORR + 1,
        )));
        Arc::new(Self {
            stamp: serde_json::from_value(json!({
                "owner":{"kind":"reserved", "module_id":"prefrontal-core"},
                "ref":"run", "scope_epoch":1, "kind":"worker",
                "attributes":{"agent_id":"agent", "run_id":"run"}, "owner_authorized":true
            }))
            .unwrap(),
            control: scopes.control,
            describe_lock: tokio::sync::Mutex::new(()),
            budget: StdMutex::new(Some(RunBudget {
                deadline,
                now_for_test: now,
            })),
            revoked: AtomicBool::new(false),
            parked_asks: StdMutex::new(Vec::new()),
        })
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[test]
    fn confirmed_scope_end_invalidates_cached_run_budget() {
        let scope = RunScope::for_test(None);
        assert!(scope.cached_budget().is_ok());
        scope.revoked.store(true, Ordering::Release);
        assert_eq!(scope.cached_budget().unwrap_err().code, "run_scope_revoked");
    }

    #[test]
    fn close_outcomes_are_definitive_only() {
        use subc_protocol::{scope::ScopeStatus, session::ModuleControlResponseToModule};

        for (status, epoch, synced, expected) in [
            (ScopeStatus::Live, Some(1), true, false),
            (ScopeStatus::Live, Some(2), true, false),
            (ScopeStatus::Live, None, true, false),
            (ScopeStatus::Ended, Some(1), true, true),
            (ScopeStatus::NotLive, None, true, true),
            (ScopeStatus::NotLive, None, false, false),
        ] {
            let reply = ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch: epoch,
                daemon_incarnation: "daemon".into(),
                owner_synced: synced,
                owner_configured: true,
                scope: None,
            };
            assert_eq!(
                super::super::scope_describe::describe_answer_ends_scope(&reply),
                expected,
                "{status:?}, {epoch:?}, synced={synced}"
            );
        }
        assert!(!super::super::scope_describe::describe_answer_ends_scope(
            &ModuleControlResponseToModule::CatalogUpdate {}
        ));
    }

    #[test]
    fn run_budget_cancels_pending_long_work() {
        let cancel = JobCancellation::new();
        let pending = PendingResponse::polling(
            "run-inspect".into(),
            "run".into(),
            "inspect".into(),
            Box::new(|_| None),
        )
        .with_cancellation(cancel.clone());
        let mut pending = RunBudget {
            deadline: Some(Instant::now()),
            now_for_test: None,
        }
        .guard_pending(pending);
        let response = (pending.poll)(&test_support::test_ctx())
            .expect("expired run must resolve pending work");
        assert!(!response.success);
        assert_eq!(response.data["code"], "run_deadline_expired");
        assert!(cancel.cancel_already_requested());
    }
}
