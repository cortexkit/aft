use super::*;

thread_local! {
    pub(super) static TEST_SERVICE: std::cell::RefCell<Option<Arc<Service>>> = const { std::cell::RefCell::new(None) };
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
    use std::time::Instant;
    use subc_protocol::{Flags, Principal, Priority, PROTOCOL_VERSION};
    use tokio::sync::{mpsc, Notify};

    #[derive(Clone, Copy)]
    enum Answer {
        Open,
        Refuse,
        Disconnect,
    }

    struct Fake {
        calls: mpsc::UnboundedSender<(Request, Option<SessionScope>)>,
        answer: Answer,
        allow_open: Notify,
        listed: Vec<String>,
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
            let examples = vector("replies.json");
            let (key, label, index) = match req {
                Request::List(_) => {
                    let mut listing = examples["list"][0].clone();
                    let item = listing["items"][0].clone();
                    listing["items"] = json!(self
                        .listed
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
                    reply["lost"] = json!(self
                        .listed
                        .iter()
                        .filter(|key| !params.open.contains(key))
                        .collect::<Vec<_>>());
                    reply["unknown"] = json!([]);
                    return Ok(serde_json::to_vec(&reply).unwrap());
                }
                Request::Open(open) => {
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
                    }
                }
                Request::Finish(finish) => (
                    finish.key,
                    None,
                    if finish.outcome == Outcome::Done {
                        1
                    } else {
                        2
                    },
                ),
                Request::Cancel(cancel) => (cancel.key, None, 3),
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
            listed,
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
            false,
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
            let scope = ScopeStamp::new(
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
            );
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
    async fn bash_background_watch_read_first_cancels_instead_of_finishing() {
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

    #[tokio::test]
    async fn tool_provider_restart_sync_excludes_unknown_and_finishes_reattached_task() {
        let f = Fixture::new();
        let registry = f.ctx.bash_background();
        let key = registry.seed_awaited_task_for_test(f.dir.path(), f.root.as_path());
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
        executor.register_actor(f.root.clone(), restarted);
        let (service, wire, mut calls) =
            fake(Answer::Open, vec![key.clone(), "unknown-task".into()]);
        *service.wire.write() = None;
        reconcile(&service, wire, &executor).await.unwrap();
        assert!(matches!(call(&mut calls).await, (Request::List(_), None)));
        assert!(
            matches!(call(&mut calls).await, (Request::Sync(sync), None) if sync.open == [key.clone()])
        );
        assert!(
            matches!(call(&mut calls).await, (Request::Finish(finish), None) if finish.key == key)
        );
        no_more(&mut calls).await;
    }
}
