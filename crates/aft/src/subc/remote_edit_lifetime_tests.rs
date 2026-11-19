use super::*;
use subc_protocol::{scope::ScopeStatus, session::ModuleControlResponseToModule};

struct CardRequest {
    operation: String,
    answer: oneshot::Sender<Value>,
}

struct Cards {
    opened: mpsc::UnboundedSender<CardRequest>,
    answers: StdMutex<HashMap<String, oneshot::Receiver<Value>>>,
    count: AtomicU64,
}

impl consent::ConsentTransport for Cards {
    fn call(&self, call: consent::ConsentCall) -> consent::ConsentFuture {
        match call.method {
            "consent.request" => {
                let (answer, receive) = oneshot::channel();
                let id = format!("acc_{}", self.count.fetch_add(1, Ordering::Relaxed));
                self.answers.lock().unwrap().insert(id.clone(), receive);
                self.opened
                    .send(CardRequest {
                        operation: call.params["operation"].as_str().unwrap().into(),
                        answer,
                    })
                    .unwrap();
                Box::pin(async move { Ok(json!({"elicitation_id":id})) })
            }
            "consent.await" => {
                let id = call.params["elicitation_id"].as_str().unwrap();
                let answer = self.answers.lock().unwrap().remove(id).unwrap();
                Box::pin(async move { Ok(answer.await.unwrap()) })
            }
            "consent.withdraw" => Box::pin(async { Ok(json!({})) }),
            _ => panic!("unexpected consent operation: {}", call.method),
        }
    }
}

struct Peer {
    daemon: tokio::io::DuplexStream,
    ends: bool,
    probes: HashSet<String>,
    waiting: Vec<Frame>,
}

impl Peer {
    async fn send(&mut self, channel: u16, corr: u64, body: Value) {
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        write_frame(&mut self.daemon, &frame).await.unwrap();
    }

    async fn describe(&mut self, frame: &Frame) -> bool {
        if frame.header.ty != FrameType::Request || frame.header.channel != 0 {
            return false;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        if body["op"] != "scope.describe" {
            return false;
        }
        let scope_ref = body["ref"].as_str().unwrap();
        self.probes.insert(scope_ref.into());
        let ended = self.ends && scope_ref == "remote-session";
        let answer = ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::NotLive,
            scope_epoch: None,
            daemon_incarnation: "remote-wire".into(),
            owner_synced: ended,
            owner_configured: true,
            scope: None,
        };
        write_frame(
            &mut self.daemon,
            &Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&answer).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        true
    }

    async fn receive(&mut self, channel: u16, corr: u64) -> Frame {
        let matches = |frame: &Frame| {
            frame.header.channel == channel
                && frame.header.corr == corr
                && frame.header.ty != FrameType::Request
        };
        if let Some(index) = self.waiting.iter().position(matches) {
            return self.waiting.remove(index);
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let frame = read_frame(&mut self.daemon).await.unwrap().unwrap();
                if matches(&frame) {
                    return frame;
                }
                if !self.describe(&frame).await {
                    self.waiting.push(frame);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no reply on channel {channel} for correlation {corr}"))
    }

    async fn call(&mut self, channel: u16, corr: u64, body: Value) -> Frame {
        self.send(channel, corr, body).await;
        self.receive(channel, corr).await
    }

    async fn saw_probe(&mut self, scope_ref: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !self.probes.contains(scope_ref) {
                let frame = read_frame(&mut self.daemon).await.unwrap().unwrap();
                if !self.describe(&frame).await {
                    self.waiting.push(frame);
                }
            }
        })
        .await
        .expect("scope was not enrolled");
    }
}

fn tool(name: &str, args: Value, corr: u64) -> Value {
    request(
        name,
        "head",
        args,
        &format!("key-{corr}"),
        pin(name, CatalogPreset::Head, false, false, None),
    )
}

fn worker_bind(channel: u16, pairing: &str, scope_ref: &str) -> Value {
    let mut bind = remote_bind(channel, pairing, &[HELD, RESULT, YIELD]);
    bind["scope"]["ref"] = json!(scope_ref);
    bind["scope"]["attributes"]["federated"]["remote_claim"] = json!({
        "kind":"worker", "worker_ref":"c".repeat(64), "owner_module_id":"worker-owner", "on_behalf_of":"remote-agent"
    });
    bind
}

fn edit_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    match request.command.as_str() {
        "configure" => crate::commands::configure::handle_configure(&request, ctx),
        "read" => crate::commands::read::handle_read(&request, ctx),
        "write" => crate::commands::write::handle_write(&request, ctx),
        "edit_match" => crate::commands::edit_match::handle_edit_match(&request, ctx),
        "batch" => crate::commands::batch::handle_batch(&request, ctx),
        "apply_patch" => crate::commands::apply_patch::handle_apply_patch(&request, ctx),
        "move_file" => crate::commands::move_file::handle_move_file(&request, ctx),
        _ => panic!("unexpected dispatch: {}", request.command),
    }
}

async fn card(
    cards: &mut mpsc::UnboundedReceiver<CardRequest>,
    operation: &str,
) -> oneshot::Sender<Value> {
    let card = tokio::time::timeout(Duration::from_secs(15), cards.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(card.operation, operation);
    card.answer
}

fn allow(answer: oneshot::Sender<Value>) {
    answer
        .send(json!({"state":"settled", "answered_by":"user", "outcome":"allow_scope"}))
        .unwrap();
}

#[test]
fn every_remote_edit_spelling_requires_read_write() {
    for (name, canonical) in [
        ("write", "write"),
        ("edit", "edit"),
        ("apply_patch", "apply_patch"),
        ("delete", "aft_delete"),
        ("move", "aft_move"),
        ("import", "aft_import"),
        ("ast_replace", "ast_grep_replace"),
    ] {
        for spelling in [name, canonical] {
            assert_eq!(dispatch::operation(spelling), Some((name, Mode::ReadWrite)));
        }
    }
}

#[cfg(unix)]
async fn exercise_external_tools(
    peer: &mut Peer,
    cards: &mut mpsc::UnboundedReceiver<CardRequest>,
    ctx: &AppContext,
    root: &Path,
) {
    use crate::sandbox_spawn::ToolLaunchPolicy;

    let path = root.join("fixture.ts");
    let bin = root.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(&path, "export const value = 0;\n").unwrap();
    std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
    let formatter = root.join("formatter-ran");
    let checker = root.join("checker-ran");
    for (program, marker) in [("prettier", &formatter), ("tsc", &checker)] {
        // A marker records a launch without running a real compiler or formatter.
        let script = format!("#!/bin/sh\nprintf 'launched' > '{}'\n", marker.display());
        crate::executable_cache::write_executable(&bin.join(program), script.as_bytes()).unwrap();
    }
    ctx.update_config(|config| {
        config.format_on_edit = true;
        config.validate_on_edit = Some("full".into());
        config
            .formatter
            .insert("typescript".into(), "prettier".into());
        config.checker.insert("typescript".into(), "tsc".into());
    });
    let args =
        json!({"path":"fixture.ts", "edits":[{"oldString":"value = 0", "newString":"value = 1"}]});
    peer.send(4, 725, tool("edit", args, 725)).await;
    allow(card(cards, "write").await);
    let answer = reply(peer.receive(4, 725).await);
    assert_eq!(
        answer["is_error"].as_bool().unwrap_or(false),
        false,
        "{answer}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "export const value = 1;\n"
    );
    assert!(
        !formatter.exists() && !checker.exists(),
        "remote edits must not start formatters or checkers"
    );

    let local = serde_json::from_value(
        json!({"id":"local-edit", "command":"edit_match", "params": {
            "file": path, "match":"value = 1", "replacement":"value = 2", "validate":"full"
        }}),
    )
    .unwrap();
    let local = ToolLaunchPolicy::with_current(
        Some(ToolLaunchPolicy::unconfined_for_test().shared()),
        || edit_dispatch(local, ctx),
    );
    assert!(local.success, "{local:?}");
    assert_eq!(local.data["formatted"], true);
    assert!(
        formatter.exists() && checker.exists(),
        "the same project must discover and run both local tools"
    );
}

#[tokio::test]
async fn remote_edit_and_scope_end_enforce_attributed_authority() {
    let _env = crate::test_env::process_env_lock();
    let _transport = RestoreTransport;
    let (opened, mut cards) = mpsc::unbounded_channel();
    let transport = Arc::new(Cards {
        opened,
        answers: StdMutex::default(),
        count: AtomicU64::new(0),
    });
    consent::install_test_transport(Some(transport.clone()));
    let storage = tempfile::tempdir().unwrap();
    let (_project, root) = test_root("remote-edit-on-b");
    let original = root.as_path().join("in.txt");
    std::fs::write(&original, "original\n").unwrap();
    let outside = storage.path().join("out.txt");
    std::fs::write(&outside, "outside\n").unwrap();
    let app = App::default_shared();
    let db = app.open_db(&storage.path().join("aft.db")).unwrap();
    let key = |pairing: &str| grants::Key {
        pairing_id: pairing.into(),
        caller_id: "remote-agent".into(),
        project_id: "B-project".into(),
        root_kind: "label",
        root_key: "B-root".into(),
    };
    let folder = root.as_path().to_str().unwrap();
    grants::save(
        &db.lock().unwrap(),
        &key("pairing-saved"),
        folder,
        Mode::Read,
        "Remote on B",
        "acc_seed",
    )
    .unwrap();
    let entries = registry(root.as_path());
    let executor = Arc::new(Executor::new());
    let (daemon, stream) = tokio::io::duplex(1024 * 1024);
    let mut peer = Peer {
        daemon,
        ends: false,
        probes: HashSet::new(),
        waiting: vec![],
    };
    let (read, write) = tokio::io::split(stream);
    let connection = storage.path().join("connection.json");
    let module = scope_describe::remote::TEST_POLL_INTERVAL.scope(
        Duration::from_millis(10),
        TEST_REGISTRY.scope(
            Arc::new(move || entries.clone()),
            run_module_loop(
                read,
                write,
                &connection,
                app,
                executor.clone(),
                edit_dispatch,
                Some(bind_config_without_indexes(storage.path())),
                false,
                usize::MAX,
                None,
                storage.path(),
                QuiescedLsp::ReapInBackground,
                None,
                None,
                None,
            ),
        ),
    );
    let exercise = async {
        hello(
            &mut peer.daemon,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        )
        .await;
        let mut local = bind(90, "opencode", Value::Null, root.as_path(), "tool_provider");
        local["principal"] = json!({"kind":"direct"});
        reply(peer.call(0, 700, local).await);
        reply(
            peer.call(0, 701, worker_bind(1, "pairing-A", "remote-session"))
                .await,
        );
        reply(
            peer.call(0, 702, worker_bind(2, "pairing-saved", "remote-session"))
                .await,
        );
        reply(
            peer.call(
                0,
                703,
                remote_bind(3, "pairing-saved", &[HELD, RESULT, YIELD]),
            )
            .await,
        );
        let mut run = remote_bind(4, "pairing-run", &[HELD, RESULT, YIELD]);
        run["scope"]["ref"] = json!("run-session");
        run["scope"]["attributes"]["federated"]["remote_claim"] =
            json!({"kind":"run", "agent_id":"remote-agent", "run_id":"run-remote"});
        reply(peer.call(0, 704, run).await);
        reply(
            peer.call(0, 705, remote_bind(5, "pairing-A", &[HELD, RESULT, YIELD]))
                .await,
        );
        peer.saw_probe("remote-session").await;
        peer.saw_probe("run-session").await;
        let read = |corr| tool("read", json!({"path":"in.txt"}), corr);
        let edit = |corr| {
            tool(
                "edit",
                json!({"path":"in.txt", "edits":[{"oldString":"original", "newString":"changed"}]}),
                corr,
            )
        };
        let unavailable = refusal(
            peer.call(2, 710, read(710)).await,
            "remote_call_not_admitted",
        );
        assert_eq!(unavailable.message, admission_refusal().message);
        refusal(
            peer.call(3, 711, edit(711)).await,
            "remote_call_not_admitted",
        );
        assert_eq!(transport.count.load(Ordering::Relaxed), 0);
        assert!(text(&reply(peer.call(3, 712, read(712)).await)).contains("original"));

        let ctx = executor.actor_context(&root).unwrap();
        ctx.update_config(|config| config.disabled_tools.clear());
        for (corr, name, args) in [
            (
                713,
                "move",
                json!({"path":outside,"destination":"moved.txt"}),
            ),
            (714, "move", json!({"path":"in.txt","destination":outside})),
            (
                715,
                "apply_patch",
                json!({"patchText":format!("*** Begin Patch\n*** Add File: inside.txt\n+inside\n*** Update File: {}\n@@\n-outside\n+changed\n*** End Patch", outside.display())}),
            ),
            (
                716,
                "apply_patch",
                json!({"patchText":format!("*** Begin Patch\n*** Update File: in.txt\n*** Move to: {}\n@@\n-original\n+changed\n*** End Patch", outside.display())}),
            ),
        ] {
            let frame = peer.call(1, corr, tool(name, args, corr)).await;
            assert_eq!(
                frame.header.ty,
                FrameType::Error,
                "{name} escaped preflight"
            );
            let error: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
            assert_eq!(error.code, "path_outside_root", "{name}: {error:?}");
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "outside\n");
        assert!(!root.as_path().join("inside.txt").exists());
        #[cfg(unix)]
        {
            let absent_outside = storage.path().join("must-not-create.txt");
            std::os::unix::fs::symlink(&absent_outside, root.as_path().join("dangling.txt"))
                .unwrap();
            refusal(
                peer.call(
                    1,
                    719,
                    tool(
                        "write",
                        json!({"path":"dangling.txt", "content":"escaped"}),
                        719,
                    ),
                )
                .await,
                "path_outside_root",
            );
            assert!(!absent_outside.exists());
        }
        ctx.update_config(|config| config.disabled_tools.push("edit".into()));
        refusal(
            peer.call(1, 717, edit(717)).await,
            "remote_tool_not_available",
        );
        assert_eq!(transport.count.load(Ordering::Relaxed), 0);
        ctx.update_config(|config| config.disabled_tools.clear());

        peer.send(1, 720, read(720)).await;
        allow(card(&mut cards, "read").await);
        assert!(text(&reply(peer.receive(1, 720).await)).contains("original"));
        peer.probes.remove("remote-session");
        peer.saw_probe("remote-session").await;
        assert!(text(&reply(peer.call(1, 721, read(721)).await)).contains("original"));
        assert_eq!(
            transport.count.load(Ordering::Relaxed),
            1,
            "unsynced NotLive must retain worker approval"
        );

        peer.send(1, 722, edit(722)).await;
        let late = card(&mut cards, "write").await;
        peer.send(5, 726, read(726)).await;
        let late_save = card(&mut cards, "read").await;
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original\n");
        peer.ends = true;
        refusal(peer.receive(1, 722).await, "remote_access_denied");
        let _ =
            late.send(json!({"state":"settled", "answered_by":"user", "outcome":"allow_scope"}));
        refusal(peer.receive(5, 726).await, "remote_access_denied");
        let _ = late_save
            .send(json!({"state":"settled", "answered_by":"user", "outcome":"allow_scope"}));
        refusal(peer.call(1, 723, read(723)).await, "remote_access_denied");
        assert!(text(&reply(peer.call(3, 724, read(724)).await)).contains("original"));
        assert_eq!(transport.count.load(Ordering::Relaxed), 3);
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original\n");
        assert!(
            grants::find(&db.lock().unwrap(), &key("pairing-A"), folder, Mode::Read)
                .unwrap()
                .is_none()
        );
        let collision_refusals: u32 = db.lock().unwrap().query_row(
            "SELECT count(*) FROM remote_tool_audit WHERE pairing_id='pairing-saved' AND decision='refused' AND outcome='remote_call_not_admitted'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(collision_refusals, 2);
        grants::save(
            &db.lock().unwrap(),
            &key("pairing-saved"),
            folder,
            Mode::ReadWrite,
            "Remote on B",
            "acc_seed",
        )
        .unwrap();
        ctx.update_config(|config| config.disabled_tools.push("edit".into()));
        refusal(
            peer.call(3, 727, edit(727)).await,
            "remote_tool_not_available",
        );
        assert_eq!(transport.count.load(Ordering::Relaxed), 3);
        ctx.update_config(|config| config.disabled_tools.clear());
        let hashline = request(
            "edit",
            "head",
            json!({"patch":"@@ in.txt\n-1:99|original\n+changed"}),
            "hashline-edit",
            tool_provider::remote_pins::hashline_edit_pin(),
        );
        refusal(
            peer.call(3, 729, hashline).await,
            "remote_tool_not_available",
        );
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original\n");
        reply(peer.call(3, 728, tool("apply_patch", json!({"patchText":"*** Begin Patch\n*** Add File: inside.txt\n+inside\n*** End Patch"}), 728)).await);
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("inside.txt")).unwrap(),
            "inside\n"
        );
        reply(
            peer.call(
                3,
                730,
                tool(
                    "move",
                    json!({"path":"inside.txt", "destination":"new-dir/moved.txt"}),
                    730,
                ),
            )
            .await,
        );
        assert!(!root.as_path().join("inside.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("new-dir/moved.txt")).unwrap(),
            "inside\n"
        );
        #[cfg(unix)]
        exercise_external_tools(&mut peer, &mut cards, &ctx, root.as_path()).await;
        write_frame(
            &mut peer.daemon,
            &Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 799, vec![]).unwrap(),
        )
        .await
        .unwrap();
    };
    let (end, ()) = tokio::join!(module, exercise);
    end.unwrap();
}
