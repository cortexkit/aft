use super::*;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

const PROJECT: &str = "compatibility-project";

fn control_frame(kind: FrameType, corr: u64, body: Value) -> Frame {
    Frame::build(
        kind,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

async fn negotiate(projects: bool) -> bool {
    let (module, mut daemon) = tokio::io::duplex(64 * 1024);
    let (mut read, mut write) = tokio::io::split(module);
    let (hello, ()) = tokio::join!(
        startup::negotiate(&mut read, &mut write, None, true, true),
        async {
            let hello = subc_transport::read_frame(&mut daemon)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(hello.header.ty, FrameType::Hello);
            let mut capabilities = vec![CONFIG_GATE_CAPABILITY];
            if projects {
                capabilities.push("config.settings.projects/v1");
            }
            let ack = control_frame(
                FrameType::HelloAck,
                hello.header.corr,
                json!({
                    "negotiated_ver": PROTOCOL_VERSION,
                    "subc_ops": [],
                    "subc_capabilities": capabilities,
                }),
            );
            subc_transport::write_frame(&mut daemon, &ack)
                .await
                .unwrap();
        }
    );
    let (gated, project_views) = hello.unwrap().settings_modes(true);
    assert!(gated);
    project_views
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: ProjectRootId,
    channels: RootChannels,
    session: Arc<Session>,
    executor: Arc<Executor>,
    metrics: Arc<DispatchPathMetrics>,
    control: Arc<readiness::DaemonControl>,
    replies: readiness::PendingControlReplies,
    writer: WriterSender,
    written: mpsc::Receiver<WriterFrame>,
    waits: BindWaits,
    channel: u16,
}

impl Fixture {
    async fn new(projects: bool) -> Self {
        let project_views = negotiate(projects).await;
        let (dir, root) = crate::subc::test_support::test_root("gated-project-compatibility");
        // Opening this path as a config file would fail the bind.
        std::fs::create_dir_all(root.as_path().join(".cortexkit/aft.jsonc")).unwrap();
        let (writer, written) = mpsc::channel(8);
        let replies = readiness::PendingControlReplies::default();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let control = Arc::new(readiness::DaemonControl::new(
            writer.clone(),
            metrics.clone(),
            PROTOCOL_VERSION,
            vec![],
            vec![],
            replies.clone(),
            2,
        ));
        let schema = Arc::new(Schema::new(crate::subc::settings::tests::sample_schema()).unwrap());
        let user_view = ConfigView::new(
            1,
            json!({"format_on_edit":false}),
            Default::default(),
            vec![],
            ConfigStatus::Active,
        );
        let session = Arc::new(Session::new(
            schema,
            validate_user(&user_view.effective).unwrap(),
            user_view,
            project_views,
        ));
        session.mark_ready();
        Self {
            _dir: dir,
            root,
            channels: Default::default(),
            session,
            executor: Arc::new(Executor::new()),
            metrics,
            control,
            replies,
            writer,
            written,
            waits: Default::default(),
            channel: 1,
        }
    }

    async fn defer_bind(&mut self) -> bool {
        let mut scope = ScopeStamp::new(
            subc_protocol::Principal::Direct,
            "settings-scope",
            1,
            ScopeKind::Head,
        );
        scope.project_id = Some(PROJECT.into());
        let request = ModuleControlRequest::RouteBind {
            route_channel: self.channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new("pi", "settings-compatibility"),
            principal: Some(subc_protocol::Principal::Direct),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: Some(scope),
            role_versions: None,
            call_mode: None,
        };
        self.channel += 1;
        let frame = DecodedFrame {
            frame: control_frame(FrameType::Request, u64::from(self.channel), json!(request)),
            phase_trace: PhaseTrace::new(Instant::now()),
            parsed_body: Default::default(),
            confinement: None,
            codemode_policies: None,
            launch_restore: None,
        };
        self.waits
            .defer(
                frame,
                &self.session,
                &self.control,
                &self.executor,
                &self.writer,
                &self.metrics,
            )
            .await
            .unwrap()
            .is_none()
    }

    async fn get(&mut self) -> u64 {
        let (corr, get) = self.next_request().await;
        assert_eq!(get, json!({"op":"config.get","project_id":PROJECT}));
        corr
    }

    async fn next_request(&mut self) -> (u64, Value) {
        let request = self.written.recv().await.unwrap();
        (
            request.header.corr,
            serde_json::from_slice(&request.body).unwrap(),
        )
    }

    fn respond(&self, corr: u64, kind: FrameType, body: Value) {
        assert!(self.replies.resolve(control_frame(kind, corr, body)));
    }

    fn admit(&mut self, result: Option<ProjectBind>) -> Result<bool, (&'static str, &'static str)> {
        let (user, project) =
            select_for_bind(&self.channels, &self.root, Some((&self.session, result)))?;
        let approved = project.as_ref().map(|project| &project.settings).or(user);
        let prepared = crate::subc_config::PreparedBindConfig::read_with_approved(
            None,
            approved,
            self.root.as_path(),
            &crate::harness::Harness::Pi,
        )
        .unwrap();
        assert!(prepared.sources(prepared.resolved()).project.path.is_none());
        if let Some(project) = project {
            self.channels.remember_project(&self.root, project);
        }
        Ok(prepared.resolved().format_on_edit)
    }

    async fn finish_bind(&mut self) -> Result<bool, (&'static str, &'static str)> {
        let (_, result) = self.waits.next().await;
        self.admit(Some(result))
    }

    fn project_reply(status: ConfigStatus, attempt: Option<u64>) -> Value {
        let mut view = ConfigView::new(
            2,
            json!({"format_on_edit":true}),
            Default::default(),
            vec![],
            status.clone(),
        );
        view.project_id = Some(PROJECT.into());
        view.attempt = attempt;
        if status == ConfigStatus::Pending {
            json!(ConfigGetReply::new(None, Some(view)))
        } else {
            json!(ConfigGetReply::new(Some(view), None))
        }
    }
}

fn capture_runtime(run: impl std::future::Future<Output = ()>) -> Vec<String> {
    crate::logging::capture_log_lines(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(15), run)
                    .await
                    .unwrap();
            });
    })
    .1
}

#[test]
fn settings_gate_without_project_capability_admits_the_user_view() {
    let logs = capture_runtime(async {
        let mut fixture = Fixture::new(false).await;
        assert!(
            !fixture.defer_bind().await,
            "a user-only daemon must not queue a project get"
        );
        assert_eq!(fixture.admit(None), Ok(false));
        assert!(fixture.written.try_recv().is_err());
    });
    let mode: Vec<_> = logs
        .iter()
        .filter(|line| line.contains("daemon settings mode:"))
        .collect();
    assert_eq!(mode.len(), 1, "{logs:?}");
    assert!(
        mode[0].starts_with("INFO ") && mode[0].ends_with("user view only"),
        "{logs:?}"
    );
}

#[test]
fn settings_definite_project_refusal_admits_user_then_retries_bind_and_change() {
    let logs = capture_runtime(async {
        let mut fixture = Fixture::new(true).await;
        for _ in 0..2 {
            assert!(fixture.defer_bind().await);
            let corr = fixture.get().await;
            fixture.respond(
                corr,
                FrameType::Error,
                json!(ErrorBody::new(
                    "project_registry_unavailable",
                    "registry is not ready"
                )),
            );
            assert_eq!(fixture.finish_bind().await, Ok(false));
        }
        let watch = tokio::spawn({
            let session = fixture.session.clone();
            let control = fixture.control.clone();
            let executor = fixture.executor.clone();
            async move { session.run(control, executor).await }
        });
        let mut change = ConfigChanged::new(2, vec![]);
        change.project_id = Some(PROJECT.into());
        assert!(fixture
            .session
            .intercept(
                &control_frame(FrameType::Push, 0, json!(change)),
                &fixture.writer,
                &fixture.metrics
            )
            .await
            .unwrap());
        let corr = fixture.get().await;
        fixture.respond(
            corr,
            FrameType::Response,
            Fixture::project_reply(ConfigStatus::Pending, Some(7)),
        );
        let (corr, applied) = fixture.next_request().await;
        assert_eq!(applied["project_id"], PROJECT);
        assert_eq!(applied["result"], "ack");
        fixture.respond(corr, FrameType::Response, json!(ConfigAppliedReply::new()));
        assert!(fixture.defer_bind().await);
        assert_eq!(fixture.finish_bind().await, Ok(true));
        assert!(
            fixture.written.try_recv().is_err(),
            "the recovered view is shared"
        );
        watch.abort();
        let _ = watch.await;
    });
    let warnings: Vec<_> = logs
        .iter()
        .filter(|line| line.starts_with("WARN ") && line.contains(PROJECT))
        .collect();
    assert_eq!(warnings.len(), 1, "{logs:?}");
    assert!(
        warnings[0].contains("project_registry_unavailable")
            && warnings[0].contains("using the user view")
    );
}

#[tokio::test(start_paused = true)]
async fn settings_project_timeout_warms_only_without_a_view() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut fixture = Fixture::new(true).await;
        assert!(fixture.defer_bind().await);
        fixture.get().await;
        assert_eq!(
            fixture.finish_bind().await,
            Err(("actor_not_ready", "module is warming"))
        );

        assert!(fixture.defer_bind().await);
        let corr = fixture.get().await;
        fixture.respond(
            corr,
            FrameType::Error,
            json!(ErrorBody::new(
                "config_gated",
                "no approved project settings"
            )),
        );
        assert_eq!(fixture.finish_bind().await, Ok(false));

        assert!(fixture.defer_bind().await);
        fixture.get().await;
        assert_eq!(fixture.finish_bind().await, Ok(false));

        assert!(fixture.defer_bind().await);
        let corr = fixture.get().await;
        fixture.respond(
            corr,
            FrameType::Response,
            Fixture::project_reply(ConfigStatus::Active, None),
        );
        assert_eq!(fixture.finish_bind().await, Ok(true));
        let project = fixture.session.projects.existing(PROJECT).unwrap();
        let control = fixture.control.clone();
        let executor = fixture.executor.clone();
        let (result, _) = tokio::join!(project.fetch(&control, &executor, false), fixture.get());
        assert!(
            result.is_ok(),
            "a timeout must not displace the loaded project view"
        );
        assert!(fixture.defer_bind().await);
        assert_eq!(fixture.finish_bind().await, Ok(true));
        assert!(fixture.written.try_recv().is_err());
    })
    .await
    .unwrap();
}

#[test]
fn settings_refused_refetch_keeps_the_projects_raised_sandbox() {
    use crate::config::SandboxMode;

    let codes = ["project_registry_unavailable", "module_ungated"];
    let logs = capture_runtime(async {
        let mut fixture = Fixture::new(true).await;
        fixture.session.user().replace(
            validate_user(&json!({
                "format_on_edit": false,
                "sandbox": {"profile": {"head": "relaxed"}},
            }))
            .unwrap(),
        );
        let head_mode = |settings: &ApprovedUser| {
            crate::config_resolve::resolve_config(&[settings.snapshot()])
                .config
                .sandbox
                .profile
                .head
        };
        assert_eq!(head_mode(fixture.session.user()), SandboxMode::Relaxed);

        assert!(fixture.defer_bind().await);
        let corr = fixture.get().await;
        let mut reply = Fixture::project_reply(ConfigStatus::Active, None);
        reply["active"]["effective"]["sandbox"] = json!({"profile": {"head": "strict"}});
        fixture.respond(corr, FrameType::Response, reply);
        assert_eq!(fixture.finish_bind().await, Ok(true));
        let project = fixture.session.projects.existing(PROJECT).unwrap();
        assert_eq!(head_mode(&project.settings), SandboxMode::Strict);

        let control = fixture.control.clone();
        let executor = fixture.executor.clone();
        for code in [codes[0], codes[1], codes[0]] {
            let (result, ()) = tokio::join!(project.fetch(&control, &executor, false), async {
                let corr = fixture.get().await;
                fixture.respond(
                    corr,
                    FrameType::Error,
                    json!(ErrorBody::new(code, "refetch refused")),
                );
            });
            assert!(result.is_ok());
            assert_eq!(
                head_mode(&project.settings),
                SandboxMode::Strict,
                "refetch refusal {code} loosened the sandbox"
            );
        }
        assert!(fixture.defer_bind().await);
        assert_eq!(fixture.finish_bind().await, Ok(true));
        assert!(
            fixture.written.try_recv().is_err(),
            "the next bind shares the retained view"
        );
    });
    let warnings: Vec<_> = logs
        .iter()
        .filter(|line| line.starts_with("WARN ") && line.contains(PROJECT))
        .collect();
    assert_eq!(warnings.len(), codes.len(), "{logs:?}");
    for code in codes {
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(&format!("({code}); keeping the current project view"))),
            "{logs:?}"
        );
    }
}
