use super::*;

thread_local! {
    pub(super) static TEST_SERVICE: std::cell::RefCell<Option<Arc<Service>>> = const { std::cell::RefCell::new(None) };
}

#[tokio::test]
async fn published_card_link_request_samples_match_afts_finish_and_progress() {
    use serde_json::json;
    let vectors: Value = serde_json::from_str(
        cortexkit_role_agent_orchestrator::vectors::file("requests.json").unwrap(),
    )
    .unwrap();
    let service = Service::new("demo_1-A");
    let key = "work!42~";
    for (sample, event, id) in [
        (
            "finish_notice_record",
            Event::Finish {
                outcome: Outcome::Done,
                summary: "Complete.\nTwo files checked.".into(),
            },
            "command!42~",
        ),
        (
            "progress_notice_record",
            Event::Progress("First file checked.\nOne remaining.".into()),
            "watch!42~",
        ),
    ] {
        let mut expected = vectors[sample].clone();
        assert_eq!(
            expected["params"]["notice_record"],
            json!({"writer":"demo-writer", "id":id})
        );
        expected["params"]["notice_record"]["writer"] = json!("aft");
        if sample == "finish_notice_record" {
            expected["params"]["more"] =
                json!("full output: bash_status({ taskId: \"work!42~\" })");
        }
        let request = service.request_body(key, event, Some(id.into()));
        request.validate().unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), expected);
    }
}

#[cfg(unix)]
mod bash_background {
    use super::*;
    use crate::config::Config;
    use crate::context::{App, AppContext};
    use crate::protocol::{RawRequest, Response};
    use crate::subc::{
        bash, drain, test_support, BindTrust, DispatchPathMetrics, Executor,
        PersistentCancelSignal, RouteChannel,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;
    use subc_protocol::{Flags, Principal, Priority, PROTOCOL_VERSION};
    use tokio::sync::{mpsc, Notify};

    #[derive(Clone, Copy)]
    enum Answer {
        Open,
        Refuse,
        Disconnect,
        LostReply,
        LateCommit,
    }

    struct Fake {
        calls: mpsc::UnboundedSender<(Request, Option<SessionScope>)>,
        answer: Answer,
        allow_open: Notify,
        listed: parking_lot::Mutex<Vec<String>>,
        cancel_offline: AtomicBool,
        route_refused: AtomicBool,
        cancel_stuck: AtomicBool,
        replay_offline: AtomicBool,
        replay_code: parking_lot::Mutex<Option<&'static str>>,
        seen_opens: parking_lot::Mutex<HashSet<String>>,
        sealed: parking_lot::Mutex<HashSet<String>>,
        existing_open_replies: std::sync::atomic::AtomicUsize,
        cancel_refusal: AtomicBool,
        finish_failure: AtomicBool,
        warming: AtomicBool,
        commit_on_sync: parking_lot::Mutex<Option<String>>,
        lost: parking_lot::Mutex<Vec<String>>,
        applied_finishes: parking_lot::Mutex<Vec<FinishParams>>,
    }

    fn vector(name: &str) -> Value {
        serde_json::from_str(cortexkit_role_agent_orchestrator::vectors::file(name).unwrap())
            .unwrap()
    }

    #[async_trait::async_trait]
    impl Wire for Fake {
        async fn request(
            &self,
            body: Vec<u8>,
            scope: Option<&SessionScope>,
        ) -> Result<Vec<u8>, Failure> {
            let req: Request = serde_json::from_slice(&body).expect("contract request");
            req.validate().expect("valid request");
            self.calls.send((req.clone(), scope.cloned())).unwrap();
            if matches!(req, Request::Finish(_) | Request::Cancel(_))
                && self.route_refused.load(Ordering::SeqCst)
            {
                return Err(Failure::Refused(
                    subc_protocol::error_codes::TARGET_UNAVAILABLE.into(),
                ));
            }
            let examples = vector("replies.json");
            let (key, label, index) = match req {
                Request::List(_) => {
                    if self.warming.load(Ordering::SeqCst) {
                        return Err(Failure::Refused(
                            subc_protocol::error_codes::BIND_BUSY.into(),
                        ));
                    }
                    let mut listing = examples["list"][0].clone();
                    let item = listing["items"][0].clone();
                    listing["items"] = json!(self
                        .listed
                        .lock()
                        .iter()
                        .map(|key| {
                            let mut item = item.clone();
                            item["key"] = json!(key);
                            item
                        })
                        .collect::<Vec<_>>());
                    return Ok(serde_json::to_vec(&listing).unwrap());
                }
                Request::Sync(params) => {
                    let mut reply = examples["sync"][0].clone();
                    let mut listed = self.listed.lock();
                    if let Some(late) = self.commit_on_sync.lock().take() {
                        listed.push(late);
                    }
                    let lost: Vec<_> = listed
                        .iter()
                        .filter(|key| !params.open.contains(key))
                        .cloned()
                        .collect();
                    let unknown: Vec<_> = params
                        .open
                        .iter()
                        .filter(|key| !listed.contains(key))
                        .collect();
                    reply["unknown"] = json!(unknown);
                    listed.retain(|key| params.open.contains(key));
                    self.lost.lock().extend(lost.iter().cloned());
                    self.sealed.lock().extend(lost.iter().cloned());
                    reply["lost"] = json!(lost);
                    return Ok(serde_json::to_vec(&reply).unwrap());
                }
                Request::Open(open) => {
                    let initial = self.seen_opens.lock().insert(open.key.clone());
                    if !initial {
                        if self.replay_offline.load(Ordering::SeqCst) {
                            return Err(Failure::Unknown("replay response not received".into()));
                        }
                        if self.sealed.lock().contains(&open.key) {
                            return Err(Failure::Refused("key_reused".into()));
                        }
                        let mut listed = self.listed.lock();
                        if listed.contains(&open.key) {
                            self.existing_open_replies.fetch_add(1, Ordering::SeqCst);
                        } else if let Some(code) = *self.replay_code.lock() {
                            return Err(Failure::Refused(code.into()));
                        } else {
                            listed.push(open.key.clone());
                        }
                        let mut reply = examples["item"][0].clone();
                        reply["item"]["key"] = json!(open.key);
                        reply["item"]["label"] = json!(open.label);
                        return Ok(serde_json::to_vec(&reply).unwrap());
                    }
                    self.allow_open.notified().await;
                    match self.answer {
                        Answer::Open => (open.key, Some(open.label), 0),
                        Answer::Refuse => {
                            let code = vector("refusals.json")[3]["code"]
                                .as_str()
                                .unwrap()
                                .to_owned();
                            return Err(Failure::Refused(code));
                        }
                        Answer::Disconnect => {
                            return Err(Failure::Transport("route closed".into()))
                        }
                        Answer::LostReply => {
                            let mut listed = self.listed.lock();
                            if !listed.contains(&open.key) {
                                listed.push(open.key);
                            }
                            return Err(CallError::OutcomeUnknown(Box::new(
                                std::io::Error::other("reply lost after write"),
                            ))
                            .into());
                        }
                        Answer::LateCommit => {
                            return Err(Failure::Unknown("open timed out before commit".into()))
                        }
                    }
                }
                Request::Finish(_) if self.finish_failure.swap(false, Ordering::SeqCst) => {
                    return Err(Failure::Unknown("finish reply lost".into()));
                }
                Request::Finish(finish) => {
                    let mut listed = self.listed.lock();
                    if listed.contains(&finish.key) {
                        self.applied_finishes.lock().push(finish.clone());
                    }
                    listed.retain(|key| key != &finish.key);
                    (
                        finish.key,
                        None,
                        if finish.outcome == Outcome::Done {
                            1
                        } else {
                            2
                        },
                    )
                }
                Request::Cancel(_) if self.cancel_stuck.load(Ordering::SeqCst) => {
                    return std::future::pending().await;
                }
                Request::Cancel(_) if self.cancel_offline.load(Ordering::SeqCst) => {
                    return Err(Failure::Transport("cancel route down".into()))
                }
                Request::Cancel(_) if self.cancel_refusal.swap(false, Ordering::SeqCst) => {
                    return Err(Failure::Refused("unknown_key".into()));
                }
                Request::Cancel(cancel) => {
                    self.listed.lock().retain(|key| key != &cancel.key);
                    self.sealed.lock().insert(cancel.key.clone());
                    (cancel.key, None, 3)
                }
                Request::Progress(progress) => (progress.key, None, 0),
                _ => panic!("unexpected awaited-work method"),
            };
            let mut reply = examples["item"][index].clone();
            reply["item"]["key"] = json!(key);
            if let Some(label) = label {
                reply["item"]["label"] = json!(label);
            }
            Ok(serde_json::to_vec(&reply).unwrap())
        }
    }

    type Calls = mpsc::UnboundedReceiver<(Request, Option<SessionScope>)>;
    fn fake(answer: Answer, listed: Vec<String>) -> (Arc<Service>, Arc<Fake>, Calls) {
        let (tx, rx) = mpsc::unbounded_channel();
        let fake = Arc::new(Fake {
            calls: tx,
            answer,
            allow_open: Notify::new(),
            listed: listed.into(),
            cancel_offline: false.into(),
            route_refused: false.into(),
            cancel_stuck: false.into(),
            replay_offline: false.into(),
            replay_code: None.into(),
            seen_opens: Default::default(),
            sealed: Default::default(),
            existing_open_replies: Default::default(),
            cancel_refusal: false.into(),
            finish_failure: false.into(),
            warming: false.into(),
            commit_on_sync: None.into(),
            lost: Vec::new().into(),
            applied_finishes: Vec::new().into(),
        });
        let service = Arc::new(Service::new("worker-provider-test"));
        *service.wire.write() = Some(fake.clone());
        (service, fake, rx)
    }

    async fn call(rx: &mut Calls) -> (Request, Option<SessionScope>) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("provider request")
            .unwrap()
    }

    fn spawn(req: RawRequest, ctx: &AppContext) -> Response {
        crate::bash_background::spawn(
            &req.id,
            req.session(),
            req.params["command"].as_str().unwrap(),
            crate::bash_background::BashShell::Bash,
            "/bin/bash".into(),
            None,
            None,
            crate::bash_background::HardKill::from_timeout_ms(Some(12_000)),
            ctx,
            false,
            true,
            false,
            false,
            24,
            80,
            Vec::new(),
            false,
            None,
            None,
        )
    }

    struct Fixture {
        dir: tempfile::TempDir,
        ctx: Arc<AppContext>,
        root: crate::subc::ProjectRootId,
        executor: Arc<Executor>,
    }

    fn call_scope(worker: bool) -> ScopeStamp {
        ScopeStamp::new(
            Principal::Reserved {
                module_id: ORCHESTRATOR.into(),
            },
            "session-scope",
            71,
            if worker {
                ScopeKind::Worker
            } else {
                ScopeKind::Head
            },
        )
    }

    impl Fixture {
        fn new() -> Self {
            let (dir, root) = test_support::test_root("awaited-work-shell");
            let ctx = Arc::new(AppContext::from_app(
                App::default_shared(),
                Config {
                    project_root: Some(root.as_path().into()),
                    storage_dir: Some(dir.path().join("storage")),
                    ..Default::default()
                },
            ));
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), ctx.clone());
            Self {
                dir,
                ctx,
                root,
                executor,
            }
        }

        fn release(&self) {
            std::fs::write(self.dir.path().join("release"), "").unwrap();
        }

        fn start(
            &self,
            service: Option<Arc<Service>>,
            worker: bool,
        ) -> mpsc::Receiver<bash::BashDeferredCompletion> {
            let (tx, rx) = mpsc::channel(2);
            let (touch, _) = mpsc::channel(2);
            let scope = call_scope(worker);
            let command = format!(
                "while [ ! -f '{}' ]; do sleep 0.02; done; printf 'awaited-finish-tail\\n'",
                self.dir.path().join("release").display()
            );
            TEST_SERVICE.with(|slot| {
                let previous = slot.replace(service);
                bash::submit_deferred_bash(false, &self.executor, &tx, &touch, &Arc::new(DispatchPathMetrics::new()),
                    spawn, self.root.clone(), self.root.as_path().into(), "shell-session".into(), "shell-request".into(),
                    RouteChannel { channel: 1, epoch: 1 }, 1, Flags::new(false, Priority::Interactive, false), PROTOCOL_VERSION,
                    json!({"command":command,"description":"Check\t output\n","background":true,"block_to_completion":true}), Default::default(),
                    bash::BashWaitCancel { keepalive: None, speech_seed: None, run_scope: None, scope: Some(scope), late_results: false,
                        ledger_carrier: None, connection: PersistentCancelSignal::new(), route: PersistentCancelSignal::new(), drain: drain::ModuleDrainWindow::default() },
                    BindTrust::FirstParty, crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                    None, None, None, None, None, worker, worker, false, true,
                    crate::subc::remote_policy::RemoteSource::None, Instant::now());
                slot.replace(previous);
            });
            rx
        }
    }

    async fn reply(
        rx: &mut mpsc::Receiver<bash::BashDeferredCompletion>,
    ) -> bash::BashDeferredCompletion {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("shell reply")
            .unwrap()
    }

    #[tokio::test]
    async fn subc_bash_worker_opens_on_its_epoch_before_launch_reply_and_finishes() {
        let f = Fixture::new();
        let (legacy, mut legacy_rx) = mpsc::unbounded_channel();
        f.ctx
            .set_progress_sender(Some(Arc::new(Box::new(move |frame| {
                if matches!(frame, PushFrame::BashCompleted(_)) {
                    let _ = legacy.send(frame);
                }
            }))));
        let (service, fake, mut calls) = fake(Answer::Open, Vec::new());
        let mut rx = f.start(Some(service.clone()), true);
        let (Request::Open(open), Some(scope)) = call(&mut calls).await else {
            panic!("missing scoped open")
        };
        assert_eq!(scope.scope.scope_epoch, Some(71));
        assert_eq!(scope.scope.scope_ref, "session-scope");
        assert_eq!(scope.identity.session, "shell-session");
        assert_eq!(open.label, "Check output");
        assert!(rx.try_recv().is_err());
        fake.allow_open.notify_one();
        let launch = reply(&mut rx).await;
        assert_eq!(launch.response_for_test().data["task_id"], open.key);
        assert_eq!(launch.response_for_test().data["status"], "running");
        f.release();
        let (Request::Finish(finish), None) = call(&mut calls).await else {
            panic!("missing finish")
        };
        assert_eq!(finish.key, open.key);
        assert_eq!(finish.outcome, Outcome::Done);
        assert!(finish.summary.contains("exit 0"), "{}", finish.summary);
        assert!(
            finish.summary.contains("awaited-finish-tail"),
            "{}",
            finish.summary
        );
        assert_eq!(
            finish.more,
            Some(format!(
                "full output: bash_status({{ taskId: \"{}\" }})",
                open.key
            ))
        );
        f.ctx
            .bash_background()
            .ack_terminal_result_for_session(&open.key, "shell-session");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), calls.recv())
                .await
                .is_err()
        );
        assert!(
            legacy_rx.try_recv().is_err(),
            "AFT sent a separate completion notice"
        );
    }

    async fn fallback(answer: Option<Answer>, worker: bool) {
        let f = Fixture::new();
        let (service, fake, mut calls) = fake(answer.unwrap_or(Answer::Open), Vec::new());
        if answer.is_none() {
            *service.wire.write() = None;
        }
        let mut rx = f.start(Some(service), worker);
        if answer.is_some() && worker {
            assert!(matches!(
                call(&mut calls).await,
                (Request::Open(_), Some(_))
            ));
            fake.allow_open.notify_one();
        }
        assert!(tokio::time::timeout(Duration::from_millis(150), rx.recv())
            .await
            .is_err());
        f.release();
        let end = reply(&mut rx).await;
        assert_eq!(end.response_for_test().data["status"], "completed");
        assert!(!end.text_for_test().unwrap().contains("inbox"));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), calls.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn subc_bash_without_capability_keeps_held_reply() {
        fallback(None, true).await;
    }

    #[tokio::test]
    async fn subc_bash_contract_refusal_keeps_held_reply() {
        fallback(Some(Answer::Refuse), true).await;
    }

    #[tokio::test]
    async fn subc_bash_transport_failure_keeps_held_reply() {
        fallback(Some(Answer::Disconnect), true).await;
    }

    #[tokio::test]
    async fn subc_bash_head_does_not_use_awaited_work() {
        fallback(Some(Answer::Open), false).await;
    }

    fn read_watch(f: &Fixture, key: &str) {
        let mut reply = Response::success(
            "watch",
            json!({"task_id":key,"status":"completed","output":"the terminal result"}),
        );
        crate::response_finalize::finalize_response(
            &mut reply,
            &f.ctx,
            "shell-session",
            "bash_watch",
        );
        assert_eq!(reply.data["output"], "the terminal result");
    }

    async fn no_more(calls: &mut Calls) {
        assert!(
            tokio::time::timeout(Duration::from_millis(150), calls.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bash_background_terminal_reply_claim_wins_before_publication() {
        let f = Fixture::new();
        let (service, _fake, mut calls) = fake(Answer::Open, Vec::new());
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.attach_awaited_for_test(&service);
        read_watch(&f, &key);
        registry.publish_awaited_for_test(&key);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == key)
        );
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn bash_background_watch_read_after_finish_does_not_cancel() {
        let f = Fixture::new();
        let (service, _fake, mut calls) = fake(Answer::Open, Vec::new());
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.attach_awaited_for_test(&service);
        registry.publish_awaited_for_test(&key);
        assert!(
            matches!(call(&mut calls).await, (Request::Finish(finish), None) if finish.key == key)
        );
        read_watch(&f, &key);
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn bash_background_terminal_publication_and_watch_share_one_racing_decision() {
        let f = Fixture::new();
        let (service, _fake, mut calls) = fake(Answer::Open, Vec::new());
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.attach_awaited_for_test(&service);
        registry.race_awaited_for_test(&key, || read_watch(&f, &key));
        match call(&mut calls).await {
            (Request::Finish(finish), None) => assert_eq!(finish.key, key),
            (Request::Cancel(cancel), None) => assert_eq!(cancel.key, key),
            _ => panic!("missing terminal claim"),
        }
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn bash_background_pattern_watch_progress_replaces_old_notices_and_reminders() {
        let f = Fixture::new();
        let (service, _fake, mut calls) = fake(Answer::Open, Vec::new());
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.attach_awaited_for_test(&service);
        let (legacy, mut legacy_rx) = mpsc::unbounded_channel();
        f.ctx
            .set_progress_sender(Some(Arc::new(Box::new(move |frame| {
                let _ = legacy.send(frame);
            }))));
        registry.awaited_progress_for_test(&key);
        assert!(
            matches!(call(&mut calls).await, (Request::Progress(progress), None) if progress.key == key && progress.text == "one matched line")
        );
        assert!(legacy_rx.try_recv().is_err());
        no_more(&mut calls).await;
    }

    async fn restart(cancel: bool, notice: Option<&str>) {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        if cancel {
            // There is no transport yet; the reply wins and saves its
            // decision, but only the next instance can send it.
            read_watch(&f, &key);
        } else if let Some(id) = notice {
            registry.save_awaited_finish_card_for_test(&key, id);
        }
        let restarted = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(f.root.as_path().into()),
                storage_dir: Some(f.dir.path().into()),
                ..Default::default()
            },
        ));
        let (legacy, mut legacy_rx) = mpsc::unbounded_channel();
        restarted.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            if matches!(frame, PushFrame::BashCompleted(_)) {
                let _ = legacy.send(frame);
            }
        }))));
        restarted
            .bash_background()
            .replay_session_for_project(f.dir.path(), "shell-session", f.root.as_path())
            .unwrap();
        let executor = Arc::new(Executor::new());
        executor.register_actor(f.root.clone(), restarted);
        let (service, wire, mut calls) =
            fake(Answer::Open, vec![key.clone(), "unknown-task".into()]);
        *service.wire.write() = None;
        reconcile(&service, wire, &executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [key.clone()])
        );
        let result = call(&mut calls).await;
        if cancel {
            assert!(matches!(result, (Request::Cancel(cancel), None) if cancel.key == key));
        } else {
            let (Request::Finish(finish), None) = result else {
                panic!("missing replayed finish")
            };
            assert_eq!(finish.key, key);
            assert_eq!(
                finish.notice_record,
                notice.map(|id| NoticeRecord {
                    writer: "aft".into(),
                    id: id.into()
                })
            );
        }
        no_more(&mut calls).await;
        assert!(
            legacy_rx.try_recv().is_err(),
            "replay sent a separate completion notice"
        );
    }

    #[tokio::test]
    async fn tool_provider_restart_sync_excludes_unknown_and_finishes_reattached_task() {
        restart(false, None).await;
    }

    #[tokio::test]
    async fn tool_provider_restart_replays_the_persisted_cancel_decision() {
        restart(true, None).await;
    }

    #[tokio::test]
    async fn tool_provider_restart_replays_the_same_finish_notice_pointer() {
        restart(false, Some("notice:awaited:saved")).await;
    }

    #[tokio::test]
    async fn lost_awaited_open_reply_replays_identically_then_cancels_the_existing_item() {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LostReply, Vec::new());
        wire.cancel_refusal.store(true, Ordering::SeqCst);
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        assert_eq!(wire.existing_open_replies.load(Ordering::SeqCst), 1);
        for _ in 0..2 {
            assert!(
                matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
            );
        }
        assert!(wire.sealed.lock().contains(&open.key));
        assert!(!wire.listed.lock().contains(&open.key));
        assert!(
            rx.try_recv().is_err(),
            "an uncertain open cannot hand back a launch ID"
        );
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn timing_out_at_the_awaited_registration_gate_is_a_definite_non_send() {
        let f = Fixture::new();
        let (service, _fake, mut calls) = fake(Answer::Open, Vec::new());
        let _gate = service.registrations.lock().await;
        let mut rx = f.start(Some(service.clone()), true);
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn awaited_resync_cannot_claim_a_live_open_or_finish_a_fallback() {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LostReply, Vec::new());
        wire.replay_offline.store(true, Ordering::SeqCst);
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.listed.lock().push(open.key.clone());
        let sync = {
            let (service, wire, executor) = (service.clone(), wire.clone(), f.executor.clone());
            tokio::spawn(async move { reconcile(&service, wire, &executor).await.unwrap() })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), calls.recv())
                .await
                .is_err()
        );
        wire.allow_open.notify_one();
        sync.await.unwrap();
        let mut saw_sync = false;
        while let Ok((request, _)) = calls.try_recv() {
            match request {
                Request::Sync(sync) => {
                    assert_eq!(sync.open, [open.key.clone()]);
                    saw_sync = true;
                }
                Request::List(_) | Request::Open(_) => {}
                _ => panic!("resync routed a fallback as awaited work"),
            }
        }
        assert!(saw_sync);
        assert!(rx.try_recv().is_err());
        wire.replay_offline.store(false, Ordering::SeqCst);
        assert_eq!(call(&mut calls).await, first);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
        );
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
    }

    async fn kill_live_awaited(own_reply: bool) {
        let f = Fixture::new();
        let (service, fake, mut calls) = fake(Answer::Open, Vec::new());
        let mut rx = f.start(Some(service.clone()), true);
        let (Request::Open(open), _) = call(&mut calls).await else {
            panic!("missing open")
        };
        fake.allow_open.notify_one();
        let _launch = reply(&mut rx).await;
        if own_reply {
            let request: RawRequest =
                serde_json::from_value(json!({"id":"kill", "command":"bash_kill",
                "session_id":"shell-session", "params":{"task_id":open.key}}))
                .unwrap();
            let result = {
                let mut route = test_support::route_identity(&f.root, "shell-session");
                Arc::get_mut(&mut route.0).unwrap().scope = Some(call_scope(true));
                let _identity = crate::subc::secret_consent::SecretIdentityGuard::install(route);
                crate::commands::bash_kill::handle(&request, &f.ctx)
            };
            assert!(result.success, "{:?}", result.data);
            assert_eq!(result.data["status"], "killed");
            assert!(
                matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
            );
        } else {
            f.ctx
                .bash_background()
                .kill(&open.key, "shell-session")
                .unwrap();
            assert!(
                matches!(call(&mut calls).await, (Request::Finish(finish), None) if finish.key == open.key && finish.outcome == Outcome::Failed)
            );
        }
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn the_awaited_worker_kill_reply_cancels_without_finishing() {
        kill_live_awaited(true).await;
    }

    #[tokio::test]
    async fn an_external_kill_still_finishes_awaited_work() {
        kill_live_awaited(false).await;
    }

    fn restored(f: &Fixture) -> (Arc<Executor>, Arc<AppContext>) {
        let restarted = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(f.root.as_path().into()),
                storage_dir: Some(f.dir.path().into()),
                ..Default::default()
            },
        ));
        restarted
            .bash_background()
            .replay_session_for_project(f.dir.path(), "shell-session", f.root.as_path())
            .unwrap();
        let executor = Arc::new(Executor::new());
        executor.register_actor(f.root.clone(), restarted.clone());
        (executor, restarted)
    }

    async fn unlisted_mark_is_retired(opening: bool) {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        if opening {
            registry.seed_awaited_opening_for_test(&key);
        }
        let (executor, restarted) = restored(&f);
        let (service, wire, mut calls) = fake(Answer::Open, Vec::new());
        *service.wire.write() = None;
        reconcile(&service, wire, &executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open.is_empty())
        );
        let registry = restarted.bash_background();
        let metadata = serde_json::to_value(registry.notice_metadata(&key).unwrap()).unwrap();
        assert_eq!(metadata["awaited_work"], "settled");
        assert_eq!(metadata["completion_delivered"], true);
        registry.attach_awaited_for_test(&service);
        registry.publish_awaited_for_test(&key);
        assert!(registry
            .pending_completions_for_session("shell-session")
            .is_empty());
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn restart_retires_unlisted_awaited_opening_without_a_notice() {
        unlisted_mark_is_retired(true).await;
    }

    #[tokio::test]
    async fn restart_retires_unlisted_open_awaited_task_without_finishing() {
        unlisted_mark_is_retired(false).await;
    }

    #[tokio::test]
    async fn restart_resends_persisted_uncertain_open_cleanup() {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        let open = record_test_open(registry, &key);
        // Simulate an AFT restart before the first scoped open's reply was read.
        let (executor, _restarted) = restored(&f);
        let (mut service, wire, mut calls) = fake(Answer::Open, Vec::new());
        Arc::get_mut(&mut service).unwrap().instance = "after-restart".into();
        *wire.commit_on_sync.lock() = Some(key.clone());
        wire.seen_opens.lock().insert(key.clone());
        reconcile(&service, wire.clone(), &executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [key.clone()])
        );
        assert_eq!(
            call(&mut calls).await,
            (open.request("after-restart", &key), Some(open.scope))
        );
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == key)
        );
        assert!(wire.lost.lock().is_empty());
        assert!(wire.sealed.lock().contains(&key));
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn awaited_finish_retries_after_its_registry_entry_is_dropped() {
        let f = Fixture::new();
        let (service, fake, mut calls) = fake(Answer::Open, Vec::new());
        fake.finish_failure.store(true, Ordering::SeqCst);
        let mut rx = f.start(Some(service.clone()), true);
        let (Request::Open(open), _) = call(&mut calls).await else {
            panic!("missing open")
        };
        fake.allow_open.notify_one();
        let _launch = reply(&mut rx).await;
        f.release();
        let (Request::Finish(first), _) = call(&mut calls).await else {
            panic!("missing finish")
        };
        *service.wire.write() = None;
        f.ctx
            .bash_background()
            .drop_awaited_task_registration_for_test(&open.key);
        fake.listed.lock().push(open.key.clone());
        reconcile(&service, fake.clone(), &f.executor)
            .await
            .unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [open.key.clone()])
        );
        assert!(fake.lost.lock().is_empty());
        assert!(matches!(call(&mut calls).await, (Request::Finish(retry), None) if retry == first));
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn awaited_open_committing_between_list_and_sync_is_kept_for_cleanup() {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LateCommit, Vec::new());
        wire.replay_offline.store(true, Ordering::SeqCst);
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        *service.wire.write() = None;
        assert!(wire.listed.lock().is_empty());
        *wire.commit_on_sync.lock() = Some(open.key.clone());
        wire.replay_offline.store(false, Ordering::SeqCst);
        reconcile(&service, wire.clone(), &f.executor)
            .await
            .unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [open.key.clone()])
        );
        assert_eq!(call(&mut calls).await, first);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
        );
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn an_awaited_busy_list_is_retried_and_is_not_authoritative() {
        let f = Fixture::new();
        let key = f
            .ctx
            .bash_background()
            .seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        let (executor, restarted) = restored(&f);
        let (service, wire, mut calls) = fake(Answer::Open, Vec::new());
        wire.warming.store(true, Ordering::SeqCst);
        *service.wire.write() = None;
        let mut observed = None;
        let first = discover_catalog(&mut observed, "same catalog", async {
            reconcile(&service, wire.clone(), &executor)
                .await
                .map(|()| true)
        })
        .await;
        assert_eq!(first, None);
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            !restarted
                .bash_background()
                .notice_metadata(&key)
                .unwrap()
                .completion_delivered
        );
        assert!(calls.try_recv().is_err());
        wire.warming.store(false, Ordering::SeqCst);
        let again = discover_catalog(&mut observed, "same catalog", async {
            reconcile(&service, wire.clone(), &executor)
                .await
                .map(|()| true)
        })
        .await;
        assert_eq!(again, Some(true));
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open.is_empty())
        );
        assert!(
            restarted
                .bash_background()
                .notice_metadata(&key)
                .unwrap()
                .completion_delivered
        );
    }

    async fn wait_for_cleanup_to_settle(registry: &BgTaskRegistry, key: &str) {
        while registry
            .notice_metadata(key)
            .unwrap()
            .has_pending_fallback_cancel()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn an_old_listed_awaited_cleanup_survives_an_offline_cancel_route() {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.seed_awaited_fallback_for_test(&key);
        registry.set_awaited_cleanup_age_and_scope_for_test(
            &key,
            Duration::from_secs(24 * 60 * 60 + 60),
            call_scope(true),
        );
        let (executor, restarted) = restored(&f);
        let (service, wire, mut calls) = fake(Answer::Open, vec![key.clone()]);
        wire.cancel_offline.store(true, Ordering::SeqCst);
        *service.wire.write() = None;
        reconcile(&service, wire.clone(), &executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [key.clone()])
        );
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == key)
        );
        assert!(wire.listed.lock().contains(&key));
        assert!(restarted
            .bash_background()
            .notice_metadata(&key)
            .unwrap()
            .has_pending_fallback_cancel());
        wire.cancel_offline.store(false, Ordering::SeqCst);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == key)
        );
        tokio::time::timeout(
            Duration::from_secs(4),
            wait_for_cleanup_to_settle(restarted.bash_background(), &key),
        )
        .await
        .unwrap();
        assert!(!wire.listed.lock().contains(&key));
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn restart_replays_an_open_even_when_sync_did_not_find_it() {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        let open = record_test_open(registry, &key);
        assert!(!registry.settle_awaited_open(&key, OpenOutcome::Unknown));
        let (executor, restarted) = restored(&f);
        let (service, wire, mut calls) = fake(Answer::Open, Vec::new());
        wire.seen_opens.lock().insert(key.clone());
        reconcile(&service, wire.clone(), &executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [key.clone()])
        );
        assert_eq!(
            call(&mut calls).await,
            (open.request(&service.instance, &key), Some(open.scope))
        );
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == key)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_cleanup_to_settle(restarted.bash_background(), &key),
        )
        .await
        .unwrap();
        assert!(wire.sealed.lock().contains(&key));
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
    }

    #[tokio::test(start_paused = true)]
    async fn scope_end_settles_awaited_cleanup_without_waiting_for_retry() {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.seed_awaited_fallback_for_test(&key);
        registry.set_awaited_cleanup_age_and_scope_for_test(&key, Duration::ZERO, call_scope(true));
        let (service, wire, mut calls) = fake(Answer::Open, vec![key.clone()]);
        wire.cancel_stuck.store(true, Ordering::SeqCst);
        reconcile(&service, wire, &f.executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(matches!(call(&mut calls).await, (Request::Sync(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == key)
        );
        registry.fence_scope_end(&call_scope(true));
        tokio::time::timeout(
            FIRST_RETRY_DELAY / 2,
            wait_for_cleanup_to_settle(registry, &key),
        )
        .await
        .unwrap();
        no_more(&mut calls).await;
    }

    enum CardRace {
        Ready,
        Timeout,
        Evicted,
        FrozenWithoutCard,
    }

    async fn finish_with_gated_card_rename(race: CardRace) {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.set_awaited_notice_owner_for_test(&key);
        let cards = Arc::new(crate::subc::rich_ui::display::TestAwaitedCards::new(
            f.dir.path().join("cards.db"),
        ));
        assert!(cards.read("unwritten".into()).await.is_none());
        cards.pause_rename.store(true, Ordering::Release);
        let (mut service, _fake, mut calls) = fake(Answer::Open, Vec::new());
        Arc::get_mut(&mut service).unwrap().cards = AwaitedCards::Test(cards.clone());
        registry.attach_awaited_for_test(&service);
        registry.publish_awaited_for_test(&key);
        tokio::time::timeout(Duration::from_secs(5), cards.rename_pending.notified())
            .await
            .unwrap();
        let id = cards.staged_id.lock().clone().unwrap();
        assert!(
            calls.try_recv().is_err(),
            "the draft must not be sent as a pointer"
        );
        match race {
            CardRace::Ready => cards.allow_rename.notify_one(),
            CardRace::Timeout => {
                tokio::time::advance(CARD_PREPARATION_LIMIT + Duration::from_millis(1)).await
            }
            CardRace::Evicted => {
                registry.drop_awaited_task_registration_for_test(&key);
                cards.allow_rename.notify_one();
            }
            CardRace::FrozenWithoutCard => {
                registry.freeze_awaited_finish_without_card_for_test(&key);
                cards.allow_rename.notify_one();
            }
        }
        let (Request::Finish(finish), None) = call(&mut calls).await else {
            panic!("missing finish")
        };
        if !matches!(race, CardRace::Ready) {
            assert!(finish.notice_record.is_none());
            if matches!(race, CardRace::Timeout) {
                assert!(cards.read(format!("draft:{id}")).await.is_some());
                cards.allow_rename.notify_one();
            }
            tokio::time::timeout(Duration::from_secs(5), cards.discarded.notified())
                .await
                .unwrap();
            assert!(cards.read(format!("draft:{id}")).await.is_none());
            assert!(
                cards.read(id).await.is_none(),
                "the unlinked real row must also be deleted"
            );
        } else {
            assert_eq!(finish.notice_record.unwrap().id, id);
            assert!(
                cards.read(id).await.is_some(),
                "the pointed-to bytes must already be committed"
            );
        }
        no_more(&mut calls).await;
    }

    async fn route_refusal_then_resync(cancel: bool) {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        let (service, wire, mut calls) = fake(Answer::Open, vec![key.clone()]);
        wire.route_refused.store(true, Ordering::SeqCst);
        registry.attach_awaited_for_test(&service);
        if cancel {
            read_watch(&f, &key);
        } else {
            registry.publish_awaited_for_test(&key);
        }
        let method = if cancel {
            "awaited_work.cancel"
        } else {
            "awaited_work.finish"
        };
        let (first, _) = call(&mut calls).await;
        assert_eq!(first.method(), method);
        assert_eq!(
            call(&mut calls).await.0,
            first,
            "route refusal must retain the decision"
        );

        *service.wire.write() = None;
        wire.route_refused.store(false, Ordering::SeqCst);
        reconcile(&service, wire.clone(), &f.executor)
            .await
            .unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [key.clone()])
        );
        assert_eq!(call(&mut calls).await.0, first);
        assert!(wire.lost.lock().is_empty());
        assert!(wire.listed.lock().is_empty());
        assert_eq!(wire.applied_finishes.lock().len(), usize::from(!cancel));
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn an_awaited_finish_survives_route_refusal_and_resync() {
        route_refusal_then_resync(false).await;
    }

    #[tokio::test]
    async fn an_awaited_cancel_survives_route_refusal_and_resync() {
        route_refusal_then_resync(true).await;
    }

    async fn awaited_locked_database_probe(launch_owner: bool) {
        let f = Fixture::new();
        let registry = f.ctx.bash_background().clone();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        if launch_owner {
            registry.set_awaited_notice_owner_for_test(&key);
        } else {
            registry.set_awaited_runner_harness_for_test(&key);
        }
        let (service, _wire, mut calls) = fake(Answer::Open, Vec::new());
        registry.attach_awaited_for_test(&service);
        let pool = App::default_shared()
            .open_db(&f.dir.path().join("aft.db"))
            .unwrap();
        registry.set_db_pool(pool.clone());
        let busy = pool.lock().unwrap();
        let publish = tokio::task::spawn_blocking(move || registry.publish_awaited_for_test(&key));
        let while_locked = tokio::time::timeout(Duration::from_millis(500), calls.recv())
            .await
            .ok()
            .flatten();
        eprintln!(
            "awaited locked aft.db probe: owner={launch_owner} finish while locked={}",
            while_locked.is_some()
        );
        drop(busy);
        publish.await.unwrap();
        let result = match while_locked {
            Some(result) => result,
            None => call(&mut calls).await,
        };
        assert!(matches!(result, (Request::Finish(_), None)));
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn probe_awaited_round4_locked_database_eventually_finishes() {
        awaited_locked_database_probe(false).await;
        awaited_locked_database_probe(true).await;
    }

    async fn cleanup_after_sync_late_commit(at_old_boundary: bool) {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LateCommit, Vec::new());
        wire.replay_offline.store(true, Ordering::SeqCst);
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        let old_deadline = tokio::time::Instant::now() + Duration::from_secs(62);
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );

        tokio::time::pause();
        *service.wire.write() = None;
        while let Ok(request) = calls.try_recv() {
            assert_eq!(request, first);
        }
        reconcile(&service, wire.clone(), &f.executor)
            .await
            .unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [open.key.clone()])
        );
        assert_eq!(call(&mut calls).await, first);
        assert!(wire.listed.lock().is_empty());
        if at_old_boundary {
            // Leave the open ambiguous until ten milliseconds before the boundary.
            let remaining = old_deadline - tokio::time::Instant::now();
            tokio::time::advance(remaining - Duration::from_millis(10)).await;
            assert_eq!(call(&mut calls).await, first);
            assert!(tokio::time::Instant::now() < old_deadline);
        }
        wire.listed.lock().push(open.key.clone());
        wire.replay_offline.store(false, Ordering::SeqCst);
        assert_eq!(call(&mut calls).await, first);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
        );
        let registry = f.ctx.bash_background();
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_cleanup_to_settle(registry, &open.key),
        )
        .await
        .unwrap();
        assert!(wire.sealed.lock().contains(&open.key));
        assert!(wire.listed.lock().is_empty());
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
        if at_old_boundary {
            assert!(tokio::time::Instant::now() > old_deadline);
            registry.drop_awaited_task_registration_for_test(&open.key);
            reconcile(&service, wire.clone(), &f.executor)
                .await
                .unwrap();
            assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
            assert!(matches!(call(&mut calls).await, (Request::Sync(_), None)));
            assert!(wire.lost.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn awaited_cleanup_cancels_an_open_that_commits_after_sync_unknown() {
        cleanup_after_sync_late_commit(false).await;
    }

    #[tokio::test]
    async fn awaited_cleanup_cancels_an_open_committing_10ms_before_the_former_deadline() {
        cleanup_after_sync_late_commit(true).await;
    }

    fn record_test_open(registry: &BgTaskRegistry, key: &str) -> OpenReplay {
        let stamp = call_scope(true);
        let open = OpenReplay {
            scope: SessionScope {
                identity: BindIdentity::new("aft-awaited-work", "shell-session"),
                scope: ScopeSelector::new(stamp.owner, stamp.scope_ref, Some(stamp.scope_epoch)),
            },
            label: "exact original shell label".into(),
        };
        registry.seed_awaited_opening_for_test(key);
        registry
            .record_awaited_open_attempt(key, open.clone())
            .unwrap();
        open
    }

    async fn cleanup_replay_definite_refusal(code: &'static str) {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LateCommit, Vec::new());
        *wire.replay_code.lock() = Some(code);
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_cleanup_to_settle(f.ctx.bash_background(), &open.key),
        )
        .await
        .unwrap();
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn awaited_open_replay_key_reused_retires_without_cancel() {
        cleanup_replay_definite_refusal("key_reused").await;
    }

    #[tokio::test]
    async fn awaited_open_replay_session_ended_retires_without_cancel() {
        cleanup_replay_definite_refusal("session_ended").await;
    }

    async fn cleanup_replay_capacity(listed_at_sync: bool) {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LateCommit, Vec::new());
        *wire.replay_code.lock() = Some("too_many_open");
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        no_more(&mut calls).await;
        if listed_at_sync {
            wire.listed.lock().push(open.key.clone());
        }
        reconcile(&service, wire.clone(), &f.executor)
            .await
            .unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        let (Request::Sync(sync), None) = call(&mut calls).await else {
            panic!("missing sync")
        };
        assert_eq!(sync.open, [open.key.clone()]);
        assert_eq!(call(&mut calls).await, first);
        if !listed_at_sync {
            no_more(&mut calls).await;
            let registry = f.ctx.bash_background();
            let other = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
            wire.listed.lock().push(other.clone());
            *wire.replay_code.lock() = None;
            registry.publish_awaited_for_test(&other);
            assert!(
                matches!(call(&mut calls).await, (Request::Finish(finish), None) if finish.key == other)
            );
            assert_eq!(call(&mut calls).await, first);
        }
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
        );
        assert!(wire.sealed.lock().contains(&open.key));
        assert!(wire.lost.lock().is_empty());
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn awaited_open_replay_waits_for_sync_or_a_freed_slot_when_at_capacity() {
        cleanup_replay_capacity(false).await;
    }

    #[tokio::test]
    async fn awaited_capacity_blocked_open_is_kept_if_a_later_list_shows_it() {
        cleanup_replay_capacity(true).await;
    }

    #[tokio::test]
    async fn awaited_capacity_refused_key_is_kept_when_the_original_open_commits_during_sync() {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LateCommit, Vec::new());
        *wire.replay_code.lock() = Some("too_many_open");
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        no_more(&mut calls).await;
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );

        *service.wire.write() = None;
        // The list is empty; the original request commits just before sync.
        *wire.commit_on_sync.lock() = Some(open.key.clone());
        reconcile(&service, wire.clone(), &f.executor)
            .await
            .unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [open.key.clone()])
        );
        assert_eq!(call(&mut calls).await, first);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_cleanup_to_settle(f.ctx.bash_background(), &open.key),
        )
        .await
        .unwrap();
        assert!(wire.listed.lock().is_empty());
        assert!(wire.sealed.lock().contains(&open.key));
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
    }

    #[tokio::test]
    async fn awaited_capacity_retry_observes_an_externally_freed_slot() {
        let f = Fixture::new();
        let (service, wire, mut calls) = fake(Answer::LateCommit, Vec::new());
        *wire.replay_code.lock() = Some("too_many_open");
        let mut rx = f.start(Some(service.clone()), true);
        let first = call(&mut calls).await;
        let Request::Open(open) = &first.0 else {
            panic!("missing open")
        };
        wire.allow_open.notify_one();
        assert_eq!(call(&mut calls).await, first);
        no_more(&mut calls).await;
        f.release();
        assert_eq!(
            reply(&mut rx).await.response_for_test().data["status"],
            "completed"
        );

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(call(&mut calls).await, first);
        // The provider frees a slot without any AFT finish, cancel, or sync.
        *wire.replay_code.lock() = None;
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            calls.try_recv().is_err(),
            "the second capacity delay backs off"
        );
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(call(&mut calls).await, first);
        assert!(
            matches!(call(&mut calls).await, (Request::Cancel(cancel), None) if cancel.key == open.key)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_cleanup_to_settle(f.ctx.bash_background(), &open.key),
        )
        .await
        .unwrap();
        assert!(wire.sealed.lock().contains(&open.key));
        assert!(wire.lost.lock().is_empty());
        no_more(&mut calls).await;
    }

    #[tokio::test(start_paused = true)]
    async fn awaited_unconfirmed_cleanup_has_no_clock_expiry_and_stops_on_scope_end() {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
        registry.set_awaited_cleanup_age_and_scope_for_test(&key, Duration::ZERO, call_scope(true));
        let open = record_test_open(registry, &key);
        let (service, wire, mut calls) = fake(Answer::Open, Vec::new());
        wire.seen_opens.lock().insert(key.clone());
        wire.replay_offline.store(true, Ordering::SeqCst);
        registry.attach_awaited_for_test(&service);
        assert!(!registry.settle_awaited_open(&key, OpenOutcome::Unknown));
        let expected = (open.request(&service.instance, &key), Some(open.scope));
        assert_eq!(call(&mut calls).await, expected);
        tokio::time::advance(Duration::from_secs(48 * 60 * 60)).await;
        assert_eq!(call(&mut calls).await, expected);
        assert!(registry
            .notice_metadata(&key)
            .unwrap()
            .has_pending_fallback_cancel());
        registry.fence_scope_end(&call_scope(true));
        tokio::time::timeout(
            FIRST_RETRY_DELAY / 2,
            wait_for_cleanup_to_settle(registry, &key),
        )
        .await
        .unwrap();
        no_more(&mut calls).await;
    }

    #[tokio::test(start_paused = true)]
    async fn awaited_finish_waits_for_a_committed_notice_record_before_sending_its_pointer() {
        finish_with_gated_card_rename(CardRace::Ready).await;
    }

    #[tokio::test(start_paused = true)]
    async fn awaited_finish_omits_a_slow_notice_record_and_cleans_up_its_late_write() {
        finish_with_gated_card_rename(CardRace::Timeout).await;
    }

    #[tokio::test(start_paused = true)]
    async fn awaited_finish_discards_an_unlinked_notice_record_after_task_eviction() {
        finish_with_gated_card_rename(CardRace::Evicted).await;
    }

    #[tokio::test(start_paused = true)]
    async fn awaited_finish_discards_a_notice_record_when_absence_was_already_frozen() {
        finish_with_gated_card_rename(CardRace::FrozenWithoutCard).await;
    }
}

#[test]
fn awaited_retry_failures_log_at_most_once_per_minute() {
    let start = tokio::time::Instant::now();
    let mut log = RetryLog::new(false);
    assert!(log.due(start));
    for seconds in 1..60 {
        assert!(!log.due(start + Duration::from_secs(seconds)));
    }
    assert!(log.due(start + Duration::from_secs(60)));
    assert!(!log.due(start + Duration::from_secs(61)));
}

#[tokio::test(start_paused = true)]
async fn an_awaited_finish_can_be_built_when_the_card_store_never_answers() {
    let start = tokio::time::Instant::now();
    let id = card_before_deadline(
        AwaitedCards::Module,
        std::future::pending::<Option<String>>(),
    )
    .await;
    assert!(start.elapsed() <= CARD_PREPARATION_LIMIT + Duration::from_millis(1));
    let service = Service::new("instance");
    let request = service.request_body(
        "bash-test",
        Event::Finish {
            outcome: Outcome::Done,
            summary: "done".into(),
        },
        id,
    );
    assert!(matches!(&request, Request::Finish(params) if params.notice_record.is_none()));
    assert!(activate_linked_card(AwaitedCards::Module, &request).is_none());
}
