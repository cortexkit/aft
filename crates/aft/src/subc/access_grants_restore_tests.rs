use super::*;

// Wire fixtures update process-wide write counters. Give each its own process
// so other tests' health-report size budgets do not depend on fixture ordering.
fn in_isolated_process(test: &str) -> bool {
    if std::env::var("AFT_LAUNCH_RESTORE_TEST").as_deref() == Ok(test) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("subc::access_grants::tests::slow_restore::{test}"))
        .args(["--exact", "--nocapture"])
        .env("AFT_LAUNCH_RESTORE_TEST", test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{test}:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child must run one test: {stdout}"
    );
    false
}

struct WireRestore {
    daemon: tokio::io::DuplexStream,
    module: Option<std::thread::JoinHandle<Result<ModuleLoopExit, SubcError>>>,
    ctx: Arc<AppContext>,
    revived: RouteIdentity,
    target: PathBuf,
    _dir: tempfile::TempDir,
}

impl WireRestore {
    async fn new() -> Self {
        Self::with_future_target(false).await
    }

    async fn with_future_target(future: bool) -> Self {
        let (dir, ctx, head, _) = default_fixture();
        ctx.update_config(|config| config.experimental_bash_background = true);
        let worker = identity(&head.project_root, "worker", "worker", Some("head"));
        register(&worker, &ctx).unwrap();
        let target = policy_path(&dir.path().join("saved-launch"));
        if !future {
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("probe.txt"), "granted content").unwrap();
        }
        std::fs::write(head.project_root.join("probe.txt"), "checkout content").unwrap();
        let healthy = dir.path().join("healthy-launch");
        std::fs::create_dir_all(&healthy).unwrap();
        std::fs::write(healthy.join("probe.txt"), "independent content").unwrap();
        remote_policy::catalog(
            catalog_request(json!([
                {"path":healthy,"mode":"read"},
                {"path":target,"mode":"write"}
            ])),
            &worker,
            &ctx,
        )
        .unwrap();
        let revived = at_epoch(&worker, 8);
        register(&revived, &ctx).unwrap();
        let ctx = Arc::new(ctx);
        let executor = Arc::new(Executor::new());
        executor.register_actor(revived.root.clone(), ctx.clone());
        let (daemon, module) = tokio::io::duplex(64 * 1024);
        let app = ctx.app().clone();
        let path = dir.path().to_path_buf();
        let module = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (read, write) = tokio::io::split(module);
            runtime.block_on(run_module_loop(
                read,
                write,
                &path.join("absent-connection.json"),
                app,
                executor,
                |request, ctx| match request.command.as_str() {
                    "configure" => crate::commands::configure::handle_configure(&request, ctx),
                    "read" => crate::commands::read::handle_read(&request, ctx),
                    "write" => crate::commands::write::handle_write(&request, ctx),
                    "bash" => crate::commands::bash::handle(&request, ctx),
                    _ => Response::success(request.id, json!({"executed":true})),
                },
                Some(path.join("absent-config.json")),
                false,
                usize::MAX,
                None,
                &path.join("store"),
                QuiescedLsp::ReapInBackground,
                None,
                None,
            ))
        });
        let mut fixture = Self {
            daemon,
            module: Some(module),
            ctx,
            revived,
            target,
            _dir: dir,
        };
        let hello = read_frame(&mut fixture.daemon).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        fixture
            .send(FrameType::HelloAck, 0, HELLO_CORR, json!({}))
            .await;
        for (channel, scope) in [(41, fixture.revived.scope.clone()), (42, None)] {
            let bind = {
                let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
                    route_channel: channel,
                    epoch: 1,
                    target: RouteTarget::ToolProvider {
                        module_id: "aft".into(),
                    },
                    identity: subc_protocol::BindIdentity::new(
                        "runner",
                        if channel == 41 { "worker" } else { "healthy" },
                    ),
                    principal: Some(Principal::Reserved {
                        module_id: "broca".into(),
                    }),
                    consumer_capabilities: None,
                    admission_facts: Default::default(),
                    scope,
                    role_versions: None,
                    call_mode: None,
                })
                .unwrap();
                bind["identity"]["project_root"] = serde_json::json!(&fixture.revived.project_root);

                bind
            };
            fixture
                .send(
                    FrameType::Request,
                    0,
                    channel as u64,
                    serde_json::to_value(bind).unwrap(),
                )
                .await;
            let reply = fixture
                .reply(channel as u64, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(reply.header.ty, FrameType::Response, "{reply:?}");
        }
        fixture
    }

    async fn send(&mut self, ty: FrameType, channel: u16, corr: u64, body: Value) {
        let frame = Frame::build(
            ty,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            if matches!(ty, FrameType::Cancel | FrameType::Goodbye) {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        )
        .unwrap();
        write_frame(&mut self.daemon, &frame).await.unwrap();
    }

    async fn call(&mut self, corr: u64) {
        self.call_tool(corr, "read", json!({"path":self.target.join("probe.txt")}))
            .await;
    }

    async fn call_tool(&mut self, corr: u64, name: &str, arguments: Value) {
        self.call_tool_with_deadline(corr, name, arguments, 700)
            .await;
    }

    async fn call_tool_with_deadline(
        &mut self,
        corr: u64,
        name: &str,
        arguments: Value,
        deadline_ms: u64,
    ) {
        self.send(
            FrameType::Request,
            41,
            corr,
            json!({
                "name":name, "preset":"worker", "arguments":arguments,
                "deadline_ms":crate::bash_background::persistence::unix_millis() + deadline_ms,
            }),
        )
        .await;
    }

    async fn reply(&mut self, corr: u64, budget: Duration) -> Option<Frame> {
        tokio::time::timeout(budget, async {
            loop {
                let frame = read_frame(&mut self.daemon).await.unwrap().unwrap();
                if frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .ok()
    }

    fn assert_unrestored(&self) {
        let key = scope_key(&self.revived);
        assert!(!self.ctx.launch_grants_materialized(&key));
        let db = self.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert!(store::list(&conn, &key).unwrap().is_empty());
        let markers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_materialized WHERE epoch='8'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            markers, 0,
            "failed restoration must leave no materialization marker"
        );
    }

    async fn stop(&mut self) {
        self.send(FrameType::Goodbye, 0, 999, json!({})).await;
        self.module.take().unwrap().join().unwrap().unwrap();
    }

    fn install_shell_launcher(&self) -> Box<dyn std::any::Any> {
        let launcher = self._dir.path().join("sandbox-profile-capture");
        let profile = self.revived.project_root.join("last-profile.json");
        // Capture the normally resolved profile, then execute a real command.
        // Native sandbox enforcement belongs to the platform sandbox tests.
        crate::executable_cache::write_executable(&launcher, format!(
            "#!/bin/sh\nshift\n[ \"$1\" = --profile-json ] || exit 91\nprintf '%s' \"$2\" > '{}'\nshift 2\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n", profile.display()
        ).as_bytes()).unwrap();
        Box::new(crate::sandbox_spawn::install_sandbox_launcher_for_test(
            &self.revived.project_root,
            launcher,
        ))
    }
}

#[tokio::test]
async fn grants_future_write_target_survives_revive_and_creates_directory() {
    if !in_isolated_process("grants_future_write_target_survives_revive_and_creates_directory") {
        return;
    }
    let mut wire = WireRestore::with_future_target(true).await;
    let target = wire.target.clone();
    wire.call_tool_with_deadline(99, "read", json!({"path":"probe.txt"}), 30_000)
        .await;
    let revived = wire.reply(99, Duration::from_secs(5)).await.unwrap();
    let revived: Value = serde_json::from_slice(&revived.body).unwrap();
    assert_eq!(revived["structuredContent"]["success"], true, "{revived}");
    assert!(!target.exists(), "revival must not create the destination");
    wire.call_tool_with_deadline(
        100,
        "write",
        json!({
        "path":target.join("created.txt"), "content":"created",
        }),
        30_000,
    )
    .await;
    let reply = wire.reply(100, Duration::from_secs(5)).await.unwrap();
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
    assert_eq!(
        std::fs::read_to_string(target.join("created.txt")).unwrap(),
        "created"
    );
    wire.stop().await;
}

#[tokio::test]
async fn grants_restore_cannot_extend_caller_start_deadline() {
    if !in_isolated_process("grants_restore_cannot_extend_caller_start_deadline") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let _launcher = wire.install_shell_launcher();
    let marker = wire.revived.project_root.join("must-not-start");
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call_tool(
        100,
        "bash",
        json!({"command":format!("touch '{}'", marker.display())}),
    )
    .await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let reply = wire.reply(100, Duration::from_secs(5)).await.unwrap();
    drop(stall);
    wire.stop().await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(
        !marker.exists(),
        "command started after its caller's deadline: {body}"
    );
    assert_eq!(
        body["structuredContent"]["code"], "bash_start_deadline",
        "{body}"
    );
}

#[test]
fn grants_legacy_missing_target_remains_retryable() {
    let (_dir, ctx, head, worker) = default_fixture();
    let target = head.project_root.join("legacy-target");
    std::fs::create_dir(&target).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    {
        let db = ctx.db().unwrap();
        db.lock()
            .unwrap()
            .execute(
                "UPDATE access_launch_params SET params=?1",
                [json!([{"path":target,"mode":"write"}]).to_string()],
            )
            .unwrap();
    }
    std::fs::remove_dir(&target).unwrap();
    let revived = at_epoch(&worker, 8);
    let refused = restore_launch(&revived, &ctx, "worker").unwrap_err();
    assert_eq!(refused.detail.unwrap()["retryable"], true);
    assert!(!ctx.launch_grant_state.is_absent(
        &launch_key(&revived, "worker").unwrap(),
        &AccessGrant {
            path: target.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        }
    ));
    std::fs::create_dir(&target).unwrap();
    restore_launch(&revived, &ctx, "worker").unwrap();
    let db = ctx.db().unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&revived))
        .unwrap()
        .iter()
        .any(|grant| grant.path == target));
}

#[test]
fn grants_atomically_replaced_file_keeps_launch_path_authority() {
    use std::os::unix::fs::MetadataExt;

    for old_identity_field in [false, true] {
        let (_dir, ctx, head, worker) = default_fixture();
        let target = head.project_root.join("atomically-saved.txt");
        std::fs::write(&target, "original content").unwrap();
        let original = std::fs::metadata(&target).unwrap();
        remote_policy::catalog(
            catalog_request(json!([{"path":target,"mode":"write"}])),
            &worker,
            &ctx,
        )
        .unwrap();
        let db = ctx.db().unwrap();
        if old_identity_field {
            let conn = db.lock().unwrap();
            let mut saved = store::launch_params(&conn, &launch_key(&worker, "worker").unwrap())
                .unwrap()
                .unwrap();
            saved[0]["target"]["identity"] = json!([original.dev(), original.ino()]);
            conn.execute(
                "UPDATE access_launch_params SET params=?1",
                [saved.to_string()],
            )
            .unwrap();
        }
        let replacement = head.project_root.join("replacement.tmp");
        std::fs::write(&replacement, "atomically saved content").unwrap();
        std::fs::rename(&replacement, &target).unwrap();
        let current = std::fs::metadata(&target).unwrap();
        assert_ne!(
            (original.dev(), original.ino()),
            (current.dev(), current.ino())
        );
        let expected = vec![AccessGrant {
            path: target.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        }];
        let revived = at_epoch(&worker, 8);
        restore_launch(&revived, &ctx, "worker").unwrap();
        assert_eq!(
            store::list(&db.lock().unwrap(), &scope_key(&revived)).unwrap(),
            expected,
            "atomic file replacement lost path authority (old_identity_field={old_identity_field})"
        );
        let refetched = at_epoch(&worker, 9);
        remote_policy::catalog(json!({"preset":"worker"}), &refetched, &ctx).unwrap();
        assert_eq!(
            store::list(&db.lock().unwrap(), &scope_key(&refetched)).unwrap(),
            expected
        );
        let fresh = context(&head.project_root, &ctx.storage_dir());
        let restarted = at_epoch(&worker, 10);
        restore_launch(&restarted, &fresh, "worker").unwrap();
        let db = fresh.db().unwrap();
        assert_eq!(
            store::list(&db.lock().unwrap(), &scope_key(&restarted)).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "atomically saved content"
        );
    }
}

#[tokio::test]
async fn grants_stalled_restore_allows_checkout_read() {
    if !in_isolated_process("grants_stalled_restore_allows_checkout_read") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    // Let restoration exhaust its own budget before the client stops waiting;
    // an already-expired queued read must be discarded rather than executed.
    wire.call_tool_with_deadline(100, "read", json!({"path":"probe.txt"}), 30_000)
        .await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let reply = wire.reply(100, Duration::from_secs(15)).await.unwrap();
    wire.assert_unrestored();
    drop(stall);
    wire.stop().await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
    assert!(body.to_string().contains("checkout content"), "{body}");
}

#[tokio::test]
async fn grants_stalled_restore_allows_independently_granted_read() {
    if !in_isolated_process("grants_stalled_restore_allows_independently_granted_read") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let healthy = policy_path(&wire._dir.path().join("healthy-launch"));
    {
        let db = wire.ctx.db().unwrap();
        store::insert(
            &db.lock().unwrap(),
            &scope_key(&wire.revived),
            &AccessGrant {
                path: healthy.clone(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call_tool_with_deadline(
        100,
        "read",
        json!({"path":healthy.join("probe.txt")}),
        30_000,
    )
    .await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let reply = wire.reply(100, Duration::from_secs(15)).await.unwrap();
    assert!(!wire
        .ctx
        .launch_grants_materialized(&scope_key(&wire.revived)));
    drop(stall);
    wire.stop().await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
    assert!(body.to_string().contains("independent content"), "{body}");
}

#[tokio::test]
async fn grants_stalled_restore_runs_bash_with_reduced_profile() {
    if !in_isolated_process("grants_stalled_restore_runs_bash_with_reduced_profile") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let _launcher = wire.install_shell_launcher();
    let marker = wire.revived.project_root.join("bash-ran");
    let independent = policy_path(&wire._dir.path().join("independent-write"));
    std::fs::create_dir_all(&independent).unwrap();
    {
        let db = wire.ctx.db().unwrap();
        store::insert(
            &db.lock().unwrap(),
            &scope_key(&wire.revived),
            &AccessGrant {
                path: independent.clone(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Write,
            },
        )
        .unwrap();
    }
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    // Restoration may use its full internal wait while this caller still has
    // enough absolute time to receive the shell's task handle.
    wire.call_tool_with_deadline(
        100,
        "bash",
        json!({"command":format!("touch '{}'", marker.display())}),
        30_000,
    )
    .await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let reply = wire.reply(100, Duration::from_secs(15)).await.unwrap();
    assert!(!wire
        .ctx
        .launch_grants_materialized(&scope_key(&wire.revived)));
    let target = wire.target.clone();
    let ran = marker.exists();
    let profile = ran.then(|| {
        serde_json::from_slice::<crate::sandbox_profile::SandboxProfile>(
            &std::fs::read(wire.revived.project_root.join("last-profile.json")).unwrap(),
        )
        .unwrap()
    });
    drop(stall);
    wire.stop().await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(ran, "checkout bash did not run: {body}");
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
    let notices = body["structuredContent"]["unrestored_grants"]
        .as_array()
        .expect("structured grant notice");
    let grant = notices
        .iter()
        .find(|grant| grant["path"] == json!(target))
        .unwrap();
    assert_eq!(grant["mode"], "write");
    assert_eq!(grant["retryable"], true);
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Unrestored launch grants:"), "{text}");
    assert!(text.contains(&target.display().to_string()), "{text}");
    assert!(text.contains("temporarily unavailable"), "{text}");
    assert!(text.contains("permission error"), "{text}");
    let profile = profile.unwrap();
    assert!(!profile.writable_roots.contains(&target));
    assert!(!profile.read_allow.contains(&target));
    assert!(profile.writable_roots.contains(&independent), "{profile:?}");
}

#[tokio::test]
async fn grants_deleted_target_is_not_reprobed_or_regranted() {
    if !in_isolated_process("grants_deleted_target_is_not_reprobed_or_regranted") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let _launcher = wire.install_shell_launcher();
    let target = wire.target.clone();
    std::fs::remove_dir_all(&target).unwrap();
    let marker = wire.revived.project_root.join("bash-ran");
    // The assertions below need each shell to finish inside its foreground
    // reply. With the fixture's default 700 ms caller deadline, a slow sandbox
    // launcher (seen on macOS CI) gets the call promoted to a background task
    // before `touch` runs, and the reply arrives with no marker on disk.
    let bash_deadline_ms = 30_000;
    wire.call_tool_with_deadline(
        100,
        "bash",
        json!({"command":format!("touch '{}'", marker.display())}),
        bash_deadline_ms,
    )
    .await;
    let first = wire.reply(100, Duration::from_secs(15)).await.unwrap();
    let first: Value = serde_json::from_slice(&first.body).unwrap();
    assert!(marker.exists(), "{first}");
    assert_eq!(first["structuredContent"]["success"], true, "{first}");
    let notices = first["structuredContent"]["unrestored_grants"]
        .as_array()
        .unwrap();
    let missing = notices
        .iter()
        .find(|grant| grant["path"] == json!(target))
        .unwrap();
    assert_eq!(missing["mode"], "write");
    assert_eq!(missing["retryable"], false, "{first}");
    assert!(first["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("no longer exists"));
    assert_eq!(wire.ctx.launch_grant_state.probe_count(&target), 1);

    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("probe.txt"), "replacement object").unwrap();
    std::fs::remove_file(&marker).unwrap();
    wire.call_tool_with_deadline(
        102,
        "bash",
        json!({"command":format!("touch '{}'", marker.display())}),
        bash_deadline_ms,
    )
    .await;
    let second = wire.reply(102, Duration::from_secs(15)).await.unwrap();
    let second: Value = serde_json::from_slice(&second.body).unwrap();
    assert!(marker.exists(), "{second}");
    assert_eq!(second["structuredContent"]["success"], true, "{second}");
    assert!(
        second["structuredContent"]["unrestored_grants"].is_null(),
        "notice must appear once: {second}"
    );
    assert_eq!(
        wire.ctx.launch_grant_state.probe_count(&target),
        1,
        "a terminally missing saved target must not be probed again"
    );
    wire.call(103).await;
    let read = wire.reply(103, Duration::from_secs(3)).await.unwrap();
    let read: Value = serde_json::from_slice(&read.body).unwrap();
    assert_eq!(
        read["structuredContent"]["code"], "grants_unavailable",
        "{read}"
    );
    assert_eq!(
        read["structuredContent"]["detail"]["retryable"], false,
        "{read}"
    );
    let next_epoch = at_epoch(&wire.revived, 9);
    restore_launch(&next_epoch, &wire.ctx, "worker").unwrap();
    assert_eq!(wire.ctx.launch_grant_state.probe_count(&target), 1);
    {
        let db = wire.ctx.db().unwrap();
        let restored = store::list(&db.lock().unwrap(), &scope_key(&next_epoch)).unwrap();
        assert!(
            !restored.iter().any(|grant| grant.path == target),
            "a new route epoch must not grant a replacement object"
        );
    }
    wire.stop().await;
}

#[test]
fn grants_saved_target_with_non_directory_parent_is_terminal() {
    let (_dir, ctx, head, worker) = default_fixture();
    let parent = head.project_root.join("grant-parent");
    let target = parent.join("child");
    std::fs::create_dir_all(&target).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    std::fs::remove_dir_all(&parent).unwrap();
    std::fs::write(&parent, "not a directory").unwrap();
    let revived = at_epoch(&worker, 8);
    restore_launch(&revived, &ctx, "worker").unwrap();
    restore_launch(&revived, &ctx, "worker").unwrap();
    assert_eq!(ctx.launch_grant_state.probe_count(&target), 1);
    let _scope = crate::access_policy::scope_guard(Some(scope_key(&revived)));
    let _ask = crate::access_policy::ask_route_guard(Some(ask_route(&revived)));
    let unavailable = ctx.unrestored_launch_grants("enotdir").unwrap();
    assert_eq!(unavailable.len(), 1);
    assert_eq!(unavailable[0].path, target);
    assert!(!unavailable[0].retryable);
    let db = ctx.db().unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&revived))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn grants_stalled_restore_keeps_other_routes_and_cancel_live() {
    if !in_isolated_process("grants_stalled_restore_keeps_other_routes_and_cancel_live") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call(100).await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    wire.send(
        FrameType::Request,
        42,
        101,
        json!({"name":"read", "arguments":{}}),
    )
    .await;
    let healthy = wire.reply(101, Duration::from_millis(300)).await;
    wire.send(FrameType::Cancel, 41, 100, json!({})).await;
    let cancelled = wire.reply(100, Duration::from_millis(300)).await;
    drop(stall);
    wire.stop().await;
    assert!(
        healthy.is_some(),
        "another route stopped answering while launch restoration stalled"
    );
    let cancelled = cancelled.expect("Cancel stopped answering while launch restoration stalled");
    let body: Value = serde_json::from_slice(&cancelled.body).unwrap();
    assert_eq!(body["code"], "cancelled");
}

#[tokio::test]
async fn grants_stalled_restore_keeps_drain_live() {
    if !in_isolated_process("grants_stalled_restore_keeps_drain_live") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call(100).await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    wire.send(
        FrameType::Push,
        0,
        103,
        serde_json::to_value(ModuleControlCommand::Draining {
            reason: subc_protocol::RouteCloseReason::Reload,
            deadline_ms: 1_000,
        })
        .unwrap(),
    )
    .await;
    let drained = wire.reply(100, Duration::from_millis(300)).await;
    drop(stall);
    wire.stop().await;
    let drained = drained.expect("drain stopped answering while launch restoration stalled");
    let body: Value = serde_json::from_slice(&drained.body).unwrap();
    assert_eq!(body["code"], "module_reloading");
}

#[test]
fn grants_stalled_restore_worker_exits_before_path_recovers() {
    if !in_isolated_process("grants_stalled_restore_worker_exits_before_path_recovers") {
        return;
    }
    let (_dir, ctx, head, worker) = default_fixture();
    let target = head.project_root.join("saved-launch");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(head.project_root.join("healthy-launch")).unwrap();
    remote_policy::catalog(
        catalog_request(json!([
            {"path":head.project_root.join("healthy-launch"),"mode":"read"},
            {"path":target,"mode":"write"}
        ])),
        &worker,
        &ctx,
    )
    .unwrap();
    let revived = at_epoch(&worker, 8);
    let ctx = Arc::new(ctx);
    let stall = crate::bounded_io::stall_running_helper(&target);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker_ctx = ctx.clone();
    let worker_identity = revived.clone();
    let progress = Arc::new(StdMutex::new(target.clone()));
    let thread = std::thread::spawn(move || {
        let result = restore_launch_before(
            &worker_identity,
            &worker_ctx,
            "worker",
            Instant::now() + Duration::from_millis(500),
            progress,
        );
        tx.send(result).unwrap();
    });
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = rx.recv_timeout(Duration::from_secs(2));
    drop(stall);
    thread.join().unwrap();
    let refused = result
        .expect("restoration worker outlived its filesystem deadline")
        .unwrap_err();
    assert_eq!(refused.code, "grants_unavailable");
    let detail = refused.detail.unwrap();
    assert_eq!(detail["path"], json!(target));
    assert_eq!(detail["retryable"], true);
    let db = ctx.db().unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&revived))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn grants_stalled_restore_refuses_within_deadline_without_partial_grants() {
    if !in_isolated_process("grants_stalled_restore_refuses_within_deadline_without_partial_grants")
    {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call_tool_with_deadline(
        100,
        "read",
        json!({"path":wire.target.join("probe.txt")}),
        30_000,
    )
    .await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let reply = wire.reply(100, Duration::from_secs(15)).await;
    wire.assert_unrestored();
    drop(stall);
    let target = wire.target.clone();
    wire.stop().await;
    let reply = reply.expect("stalled launch restoration did not refuse within its deadline");
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    let response = &body["structuredContent"];
    assert_eq!(response["success"], false, "{body}");
    assert_eq!(response["code"], "grants_unavailable", "{body}");
    assert_eq!(response["detail"]["retryable"], true, "{body}");
    assert_eq!(response["detail"]["path"], json!(target), "{body}");
}

#[tokio::test]
async fn grants_stalled_restore_retries_after_path_recovers() {
    if !in_isolated_process("grants_stalled_restore_retries_after_path_recovers") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call_tool_with_deadline(
        100,
        "read",
        json!({"path":wire.target.join("probe.txt")}),
        30_000,
    )
    .await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let refused = wire.reply(100, Duration::from_secs(15)).await;
    drop(stall);
    if refused.is_some() {
        wire.assert_unrestored();
    }
    wire.call(102).await;
    let recovered = wire.reply(102, Duration::from_secs(3)).await.unwrap();
    let body: Value = serde_json::from_slice(&recovered.body).unwrap();
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
    assert!(body.to_string().contains("granted content"), "{body}");
    let db = wire.ctx.db().unwrap();
    let grants = store::list(&db.lock().unwrap(), &scope_key(&wire.revived)).unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.contains(&AccessGrant {
        path: wire.target.clone(),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Write,
    }));
    assert_eq!(
        wire.ctx.launch_restore_attempts(),
        2,
        "a failed restoration must not be memoized"
    );
    wire.stop().await;
    let refused =
        refused.expect("the stalled attempt never refused, so recovery was not a fresh admission");
    let body: Value = serde_json::from_slice(&refused.body).unwrap();
    let response = &body["structuredContent"];
    assert_eq!(response["code"], "grants_unavailable", "{body}");
    assert_eq!(response["detail"]["retryable"], true, "{body}");
}
