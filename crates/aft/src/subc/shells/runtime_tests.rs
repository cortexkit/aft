use super::*;

fn stamp(reference: &str, worker: bool) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":reference,"scope_epoch":1,"kind":if worker {"worker"} else {"head"},
        "owner_authorized":true
    }))
    .unwrap()
}

#[test]
fn shells_v1_holder_resolution_uses_registered_head_and_worker_parent() {
    let dir = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
    let head = stamp("head", false);
    let worker = stamp("worker", true);
    let head_key = ScopeKey::from_stamp(&head);
    let worker_key = ScopeKey::from_stamp(&worker);
    let config = crate::config::Config::default();
    crate::db::access_grants::register_scope(&conn, &head_key, None, &config).unwrap();
    crate::db::access_grants::register_agent(&conn, &head_key, "agent-alf", true).unwrap();
    crate::db::access_grants::register_scope(&conn, &worker_key, Some(&head_key), &config).unwrap();
    crate::db::access_grants::register_worker(&conn, &worker_key).unwrap();
    let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
        "bash-0123456789abcdef".into(),
        "bg_worker_cedar".into(),
        "true".into(),
        dir.path().to_owned(),
        None,
        None,
        false,
        false,
    );
    metadata.scope = Some(head);
    assert_eq!(
        holder(Some(&conn), &metadata).agent_id.as_deref(),
        Some("agent-alf")
    );
    metadata.scope = Some(worker);
    let resolved = holder(Some(&conn), &metadata);
    assert_eq!(resolved.kind, "worker");
    assert_eq!(resolved.agent_id.as_deref(), Some("agent-alf"));
    assert_eq!(resolved.scope_ref.as_deref(), Some("worker"));
    assert_eq!(resolved.execution_id.as_deref(), Some("bg_worker_cedar"));
    metadata.scope = None;
    metadata.harness = Some("opencode".into());
    let resolved = holder(Some(&conn), &metadata);
    assert_eq!(resolved.kind, "unattributed");
    assert_eq!(resolved.harness.as_deref(), Some("opencode"));
    assert!(resolved.agent_id.is_none());
}

#[test]
fn shells_v1_spawn_facts_persist_and_legacy_records_omit_unknowns() {
    use crate::bash_background::shell_metadata::{enter, ShellCall};
    let guard = enter(ShellCall {
        description: Some("Check API types".into()),
        session: "bg_worker_cedar".into(),
        call_id: None,
        kind: "foreground".into(),
    });
    let metadata = crate::bash_background::with_call_key(Some("call-1".into()), || {
        crate::bash_background::persistence::PersistedTask::starting(
            "bash-0123456789abcdef".into(),
            "bg_worker_cedar".into(),
            "true".into(),
            "/work".into(),
            None,
            None,
            false,
            false,
        )
    });
    let mut encoded = serde_json::to_value(&metadata).unwrap();
    drop(guard);
    let decoded: crate::bash_background::persistence::PersistedTask =
        serde_json::from_value(encoded.clone()).unwrap();
    let call = decoded.shell_call.unwrap();
    assert_eq!(call.description.as_deref(), Some("Check API types"));
    assert_eq!(call.session, "bg_worker_cedar");
    assert_eq!(call.call_id.as_deref(), Some("call-1"));
    assert!(crate::bash_background::shell_metadata::current().is_none());
    encoded.as_object_mut().unwrap().remove("shell_call");
    encoded.as_object_mut().unwrap().remove("shell_revision");
    let legacy: crate::bash_background::persistence::PersistedTask =
        serde_json::from_value(encoded).unwrap();
    assert!(legacy.shell_call.is_none());
    assert_eq!(legacy.shell_revision, 0);
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("shell_call")
        .is_none());
}

#[test]
fn shells_v1_status_unknown_counts_are_omitted_individually() {
    let ready = Readiness {
        state: "ready".into(),
        reason: None,
    };
    let root = RootStatus {
        root: "/work".into(),
        agent_ids: vec![],
        text_index: ready.clone(),
        call_graph: ready.clone(),
        meaning: ready,
        lsp: vec![],
        errors: None,
        warnings: None,
        todos: Some(7),
    };
    let value = serde_json::to_value(root).unwrap();
    assert!(value.get("errors").is_none());
    assert!(value.get("warnings").is_none());
    assert_eq!(value["todos"], 7);
    assert_eq!(readiness(&json!({"status":"building"})).state, "building");
    assert_eq!(
        readiness(&json!({"status":"unavailable","reason":"index_missing"}))
            .reason
            .as_deref(),
        Some("index_missing")
    );
}

fn captured(
    dir: &std::path::Path,
    bytes: &[u8],
    pty: bool,
    ended: bool,
) -> (
    crate::bash_background::registry::ShellSnapshot,
    ShellReader,
    crate::bash_background::persistence::ResolvedTask,
) {
    use crate::bash_background::persistence::{allocate_task_layout, write_task_at, PersistedTask};
    let task = allocate_task_layout(dir, "plain-session").unwrap();
    let mut metadata = PersistedTask::starting(
        task.paths.task_id.clone(),
        "plain-session".into(),
        "printf progress".into(),
        dir.to_owned(),
        Some(dir.to_owned()),
        None,
        false,
        false,
    );
    metadata.harness = Some("test".into());
    metadata.completion_has_secret_placeholders = Some(false);
    metadata.completion_has_env_secrets = Some(false);
    metadata.completion_has_non_ticket_env_secrets = Some(false);
    if pty {
        metadata.mode = BgMode::Pty;
        metadata.pty_rows = Some(3);
        metadata.pty_cols = Some(20);
    }
    if ended {
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    } else {
        metadata.status = BgTaskStatus::Running;
    }
    write_task_at(&task, &metadata).unwrap();
    std::fs::write(&task.paths.stdout, if pty { &[] } else { bytes }).unwrap();
    std::fs::write(&task.paths.stderr, []).unwrap();
    std::fs::write(&task.paths.pty, if pty { bytes } else { &[] }).unwrap();
    let stored = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let (snapshot, reader) = ShellReader::persisted(stored).unwrap();
    (snapshot, reader, task)
}

#[test]
fn shells_v1_persisted_observation_settles_lost_without_adopting_and_keeps_revision() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, reader, task) = captured(dir.path(), b"last output\n", false, false);
    assert_eq!(snapshot.metadata.status, BgTaskStatus::FateUnknown);
    let row = project_row(&snapshot, None);
    assert_eq!(row.end_reason.as_deref(), Some("lost"));
    assert_eq!(reader.shell_revision(&row.task_id, "first").unwrap(), 1);
    let metadata = crate::bash_background::persistence::read_task_at(&task).unwrap();
    assert_eq!(metadata.status, BgTaskStatus::FateUnknown);
    assert!(metadata.finished_at.is_some());
    let stored = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let (_, reader) = ShellReader::persisted(stored).unwrap();
    assert_eq!(reader.shell_revision(&row.task_id, "first").unwrap(), 1);
    assert_eq!(reader.shell_revision(&row.task_id, "changed").unwrap(), 2);
}

#[test]
fn shells_v1_remote_without_live_registry_keeps_durable_pending_state() {
    use crate::bash_background::persistence::{read_task_at, write_task_at};
    let dir = tempfile::tempdir().unwrap();
    let (_, _, task) = captured(dir.path(), b"", false, false);
    let mut metadata = read_task_at(&task).unwrap();
    let job = "0b816da2-78cb-4fef-8abc-0123456789ab";
    metadata.remote=Some(serde_json::from_value(json!({"harness":"test","session":"plain-session","job_id":job,"stdout_len":0,"stderr_len":0,"cancel_requested":false})).unwrap());
    write_task_at(&task, &metadata).unwrap();
    let stored = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let (snapshot, reader) = ShellReader::persisted(stored).unwrap();
    assert!(!snapshot.metadata.is_terminal());
    let row = project_row(&snapshot, None);
    assert_eq!(row.state, "queued");
    assert_eq!(row.remote.unwrap().job_id, job);
    assert!(row.end_reason.is_none());
    reader.shell_revision(&row.task_id, "pending").unwrap();
    let unchanged = read_task_at(&task).unwrap();
    assert_eq!(unchanged.status, BgTaskStatus::Running);
    assert!(unchanged.finished_at.is_none());
}

#[tokio::test]
async fn shells_v1_output_real_files_default_backfill_exact_resume_and_end_record() {
    let dir = tempfile::tempdir().unwrap();
    let raw = "o\n".repeat(35_000);
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let row = project_row(&snapshot, None);
    let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
    service
        .state
        .write()
        .await
        .rows
        .insert(row.task_id.clone(), row.clone());
    service
        .sources
        .write()
        .await
        .insert(row.task_id.clone(), reader);
    let metrics = DispatchPathMetrics::new();
    let (tx, mut output) = tokio::sync::mpsc::channel(16);
    let frame = Frame::build(
        FrameType::Request,
        subc_protocol::Flags(0),
        41,
        1,
        7,
        b"{}".to_vec(),
    )
    .unwrap();
    let began = tokio::time::Instant::now();
    assert!(service
        .output(
            &tx,
            &metrics,
            &frame,
            OutputParams {
                task_id: row.task_id.clone(),
                ..Default::default()
            },
            &CancellationToken::new(),
            &CancellationToken::new()
        )
        .await
        .unwrap());
    assert!(
        began.elapsed() >= Duration::from_secs(1),
        "four backfill chunks must use separate 250 ms ticks"
    );
    let mut text = String::new();
    let mut end = raw.len() as u64 - BACKFILL_BYTES;
    let mut chunks = 0;
    let first = output.try_recv().unwrap();
    let first: OutputFrame = serde_json::from_slice(first.body()).unwrap();
    assert!(
        matches!(first,OutputFrame::Start{truncated_before:Some(Offsets{stdout,stderr:0}),..} if stdout==end)
    );
    let mut ended = false;
    while let Ok(frame) = output.try_recv() {
        assert_eq!((frame.header.channel, frame.header.corr), (41, 7));
        match serde_json::from_slice::<OutputFrame>(frame.body()).unwrap() {
            OutputFrame::Chunk {
                stream,
                offset,
                end_offset,
                text: part,
            } => {
                assert_eq!(stream, "stdout");
                assert_eq!(offset, end);
                end = end_offset;
                text.push_str(&part);
                chunks += 1;
            }
            OutputFrame::End { row: ended_row } => {
                assert_eq!(ended_row.end_reason.as_deref(), Some("exited"));
                ended = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(chunks, 4);
    assert!(ended);
    assert_eq!(end, raw.len() as u64);
    assert_eq!(text, &raw[raw.len() - BACKFILL_BYTES as usize..]);
    let (tx, mut output) = tokio::sync::mpsc::channel(16);
    service
        .output(
            &tx,
            &metrics,
            &frame,
            OutputParams {
                task_id: row.task_id,
                from_offset: Some(Offsets {
                    stdout: end - 2,
                    stderr: 0,
                }),
                ..Default::default()
            },
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut resumed = String::new();
    while let Ok(frame) = output.try_recv() {
        if let OutputFrame::Chunk {
            offset,
            end_offset,
            text,
            ..
        } = serde_json::from_slice(frame.body()).unwrap()
        {
            assert_eq!((offset, end_offset), (end - 2, end));
            resumed.push_str(&text);
        }
    }
    assert_eq!(resumed, "o\n");
    service.stop();
}

#[tokio::test]
async fn shells_v1_output_pty_screen_uses_masked_renderer_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, reader, _task) = captured(dir.path(), b"hello\rworld\n", true, true);
    let row = project_row(&snapshot, None);
    let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
    service
        .state
        .write()
        .await
        .rows
        .insert(row.task_id.clone(), row.clone());
    service
        .sources
        .write()
        .await
        .insert(row.task_id.clone(), reader);
    let (tx, mut output) = tokio::sync::mpsc::channel(16);
    let metrics = DispatchPathMetrics::new();
    let frame = Frame::build(
        FrameType::Request,
        subc_protocol::Flags(0),
        41,
        1,
        7,
        b"{}".to_vec(),
    )
    .unwrap();
    service
        .output(
            &tx,
            &metrics,
            &frame,
            OutputParams {
                task_id: row.task_id,
                ..Default::default()
            },
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut screen = None;
    while let Ok(frame) = output.try_recv() {
        if let OutputFrame::Screen {
            rows,
            cols,
            lines,
            cursor,
        } = serde_json::from_slice(frame.body()).unwrap()
        {
            screen = Some((rows, cols, lines, cursor));
        }
    }
    let (rows, cols, lines, cursor) = screen.unwrap();
    assert_eq!((rows, cols), (3, 20));
    assert_eq!(lines, vec!["world".to_owned()]);
    assert_eq!((cursor.row, cursor.col), (1, 5));
    service.stop();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shells_v1_list_pages_fifty_running_first_and_cursor_is_filter_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _reader, _task) = captured(dir.path(), b"", false, true);
    let base = project_row(&snapshot, None);
    let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
    for index in 0..51 {
        let mut row = base.clone();
        row.task_id = format!("bash-{index:016x}");
        row.started_at_ms = index;
        if index == 0 {
            row.state = "running".into();
            row.ended_at_ms = None;
        }
        service
            .state
            .write()
            .await
            .rows
            .insert(row.task_id.clone(), row);
    }
    let first = service.list(ListParams::default()).await.unwrap();
    assert_eq!(first.rows.len(), 50);
    assert_eq!(first.rows[0].task_id, "bash-0000000000000000");
    assert_eq!(first.rows[1].started_at_ms, 50);
    let cursor = first.next_cursor.unwrap();
    let second = service
        .list(ListParams {
            cursor: Some(cursor.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(second.rows.len(), 1);
    assert!(second.next_cursor.is_none());
    assert!(first
        .rows
        .iter()
        .all(|row| row.task_id != second.rows[0].task_id));
    assert_eq!(
        service
            .list(ListParams {
                cursor: Some(cursor),
                states: Some(vec!["running".into()]),
                ..Default::default()
            })
            .await
            .err()
            .unwrap()
            .code,
        "invalid_params"
    );
    service.stop();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shells_v1_real_route_refusals_unknown_agent_watch_cancel_and_drain() {
    use super::super::super::{
        control_flags, read_frame, run_module_loop, write_frame, ModuleControlCommand,
        ModuleControlRequest, QuiescedLsp, RouteTarget, HELLO_CORR,
    };

    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("shells route response timed out")
    }
    async fn write(
        daemon: &mut tokio::io::DuplexStream,
        ty: FrameType,
        channel: u16,
        corr: u64,
        body: Value,
    ) {
        let frame = Frame::build(
            ty,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            if ty == FrameType::Cancel {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        )
        .unwrap();
        write_frame(daemon, &frame).await.unwrap();
    }
    async fn request(
        daemon: &mut tokio::io::DuplexStream,
        channel: u16,
        corr: u64,
        body: Value,
    ) -> Frame {
        write(daemon, FrameType::Request, channel, corr, body).await;
        receive(daemon, channel, corr).await
    }
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("module-storage");
    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let module_app = app.clone();
    let module_executor = executor.clone();
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let module_storage = storage.clone();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (read, write) = tokio::io::split(module);
        runtime.block_on(run_module_loop(
            read,
            write,
            &module_storage.join("absent-connection.json"),
            module_app,
            module_executor,
            |_, _| panic!("shells routes must never execute an agent command"),
            None,
            false,
            usize::MAX,
            None,
            &module_storage,
            QuiescedLsp::ReapInBackground,
            None,
            None,
        ))
    });
    assert_eq!(
        receive(&mut daemon, 0, HELLO_CORR).await.header.ty,
        FrameType::Hello
    );
    write(&mut daemon, FrameType::HelloAck, 0, HELLO_CORR, json!({})).await;
    std::fs::create_dir_all(&storage).unwrap();
    app.open_db(&storage.join("aft.db")).unwrap();
    let bind = |channel: u16, harness: &str, principal: subc_protocol::Principal| {
        serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(harness, format!("shells:{channel}")),
            principal: Some(principal),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        })
        .unwrap()
    };
    let callosum = || subc_protocol::Principal::Reserved {
        module_id: "callosum".into(),
    };
    for (channel, harness, admitted) in [
        (41, "fed-phone:person", true),
        (42, "fed-sim:person", true),
        (43, "fed-computer:person", false),
        (44, "fed-unknown:person", false),
        (45, "callosum-self", false),
    ] {
        let ack = request(
            &mut daemon,
            0,
            channel as u64,
            bind(channel, harness, callosum()),
        )
        .await;
        assert_eq!(
            ack.header.ty,
            if admitted {
                FrameType::Response
            } else {
                FrameType::Error
            },
            "{harness}: {ack:?}"
        );
        if !admitted {
            assert_eq!(
                serde_json::from_slice::<Value>(&ack.body).unwrap()["code"],
                "shells_operator_required"
            );
        }
    }
    let bound = request(
        &mut daemon,
        0,
        46,
        bind(
            46,
            "prefrontal-core",
            subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
        ),
    )
    .await;
    assert_eq!(bound.header.ty, FrameType::Response);
    let refused = request(
        &mut daemon,
        46,
        1,
        json!({"method":"shells.list","params":{}}),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.body).unwrap()["code"],
        "shells_operator_required"
    );
    let bound = request(
        &mut daemon,
        0,
        47,
        bind(47, "operator", subc_protocol::Principal::Direct),
    )
    .await;
    assert_eq!(bound.header.ty, FrameType::Response);
    let status = request(
        &mut daemon,
        47,
        1,
        json!({"method":"aft.status","params":{}}),
    )
    .await;
    assert_eq!(status.header.ty, FrameType::Response);
    assert_eq!(
        serde_json::from_slice::<Value>(&status.body).unwrap()["result"]["aft"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    let invalid=request(&mut daemon,47,2,json!({"method":"shells.output","params":{"task_id":"bash-0123456789abcdef","max_chunk_bytes":63}})).await;
    assert_eq!(invalid.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&invalid.body).unwrap()["code"],
        "invalid_params"
    );
    let list = request(
        &mut daemon,
        41,
        1,
        json!({"method":"shells.list","params":{"agent_ids":["agent-never-seen"]}}),
    )
    .await;
    assert_eq!(list.header.ty, FrameType::Response);
    let value: Value = serde_json::from_slice(&list.body).unwrap();
    assert_eq!(value["result"]["rows"], json!([]));
    assert_eq!(
        value["result"]["unknown_agent_ids"],
        json!(["agent-never-seen"])
    );
    let refused = request(
        &mut daemon,
        41,
        2,
        json!({"method":"permissions.list","params":{}}),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.body).unwrap()["code"],
        "route_not_admitted"
    );
    let initial = request(
        &mut daemon,
        41,
        3,
        json!({"method":"shells.watch","params":{"status":true}}),
    )
    .await;
    assert_eq!(initial.header.ty, FrameType::StreamData);
    assert_eq!(
        serde_json::from_slice::<Value>(&initial.body).unwrap()["type"],
        "resync"
    );
    write(&mut daemon, FrameType::Cancel, 41, 3, json!({})).await;
    assert_eq!(
        receive(&mut daemon, 41, 3).await.header.ty,
        FrameType::StreamEnd
    );
    let initial = request(
        &mut daemon,
        42,
        4,
        json!({"method":"shells.watch","params":{}}),
    )
    .await;
    assert_eq!(initial.header.ty, FrameType::StreamData);
    write(
        &mut daemon,
        FrameType::Push,
        0,
        99,
        serde_json::to_value(ModuleControlCommand::Draining {
            reason: subc_protocol::RouteCloseReason::Reload,
            deadline_ms: 1000,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(
        receive(&mut daemon, 42, 4).await.header.ty,
        FrameType::StreamEnd
    );
    drop(daemon);
    thread.join().unwrap().unwrap();
}
