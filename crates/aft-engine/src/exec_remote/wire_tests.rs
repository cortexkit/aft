use super::{tests::MemorySink, types::*, wire_fixture::*, *};
use serde_json::json;
use std::time::Duration;
use subc_protocol::{BindIdentity, FrameType};
async fn client(daemon: &Daemon) -> ExecRemoteClient {
    ExecRemoteClient::connect(&daemon.connection, BindIdentity::new("aft-test", "session"))
        .await
        .unwrap()
}

async fn drain(stream: &mut RemoteStream, sink: &mut MemorySink) -> Result<Verdict, Error> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.next(sink).await? {
                StreamProgress::Record | StreamProgress::AdmissionProgress => {}
                StreamProgress::Complete(verdict) => return Ok(verdict),
            }
        }
    })
    .await
    .expect("the fixture stream must terminate")
}

#[tokio::test]
async fn daemon_route_uses_hyphenated_capability_and_dotted_operations() {
    for (script, expected) in [
        (Script::Lost, Verdict::OutcomeUnknown),
        (
            Script::Refused,
            Verdict::RunLocally {
                reason: RefusalReason::Unknown("future_refusal".into()),
                refusal_detail: None,
            },
        ),
    ] {
        let daemon = daemon(script, client::CAPABILITY).await;
        let client = client(&daemon).await;
        let mut stream = client
            .run(&RunRequest::new(
                "/workspace/task",
                "/src/repo",
                "/workspace/task",
                "cargo test",
            ))
            .await
            .unwrap();
        assert_eq!(
            drain(&mut stream, &mut MemorySink::default())
                .await
                .unwrap(),
            expected
        );
        assert!(client.status().await.unwrap().server_reachable);
        assert_eq!(client.cancel_job(id()).await.unwrap().job_id, id());
        let log = daemon.log.lock().unwrap();
        let open = log
            .iter()
            .find(|(_, body)| body["op"] == "route.open")
            .unwrap();
        assert_eq!(
            open.1["target"],
            serde_json::to_value(subc_protocol::RouteTarget::ManagementSurface {
                module_id: "executor-picked-by-capability".into()
            })
            .unwrap()
        );
        let operations: Vec<_> = log
            .iter()
            .filter_map(|(_, body)| body["method"].as_str())
            .collect();
        assert_eq!(operations, ["exec.run", "exec.status", "exec.cancel"]);
    }
}

#[tokio::test]
async fn remote_progress_is_requested_only_with_the_advertised_capability() {
    for supported in [false, true] {
        let claims = if supported {
            vec![client::CAPABILITY, PROGRESS_CAPABILITY]
        } else {
            vec![client::CAPABILITY]
        };
        let daemon = daemon_with_capabilities(Script::Utf8, &claims, 1).await;
        let client = client(&daemon).await;
        // Enable a default request on a new executor, but remove a saved
        // opt-in when today's executor no longer advertises progress.
        let request = RunRequest::new("/workspace", "/src/repo", "/workspace", "test")
            .with_progress(!supported);
        let mut stream = client.run(&request).await.unwrap();
        let mut sink = MemorySink::default();
        if supported {
            assert_eq!(
                stream.next(&mut sink).await.unwrap(),
                StreamProgress::AdmissionProgress
            );
            assert!(!stream.received_frame());
            assert_eq!(stream.resume_point(), None);
            assert!(sink.seqs.is_empty());
        }
        assert_eq!(
            drain(&mut stream, &mut sink).await.unwrap(),
            Verdict::Exited { code: 0 }
        );
        let log = daemon.log.lock().unwrap();
        let run = log
            .iter()
            .find(|(_, body)| body["method"] == "exec.run")
            .unwrap();
        assert_eq!(
            run.1["params"]["progress"].as_bool().unwrap_or(false),
            supported
        );
        if !supported {
            assert!(run.1["params"].get("progress").is_none());
        }
    }
}

/// One submission checks several capabilities of the executor (submission
/// keys, progress, the return cap). A route reads the daemon catalog once for
/// all of them; a reopened route reads it again.
#[tokio::test]
async fn capability_checks_read_the_catalog_once_per_route() {
    let daemon = daemon_with_capabilities(
        Script::Utf8,
        &[
            client::CAPABILITY,
            PROGRESS_CAPABILITY,
            SUBMISSION_KEY_CAPABILITY,
        ],
        1,
    )
    .await;
    let catalog_lists = || {
        daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["op"] == "catalog.list")
            .count()
    };
    let client = client(&daemon).await;
    // Connecting resolves the executor through the catalog.
    let connected = catalog_lists();
    for _ in 0..3 {
        assert!(client.supports_submission_key().await.unwrap());
        assert_eq!(
            client.return_max_bytes().await.unwrap(),
            super::return_max_bytes(),
            "the executor does not advertise large returns"
        );
    }
    let request = RunRequest::new("/workspace", "/src/repo", "/workspace", "test")
        .with_submission_key("aft:bash-0000000000000001");
    let mut stream = client.run(&request).await.unwrap();
    assert_eq!(
        drain(&mut stream, &mut MemorySink::default())
            .await
            .unwrap(),
        Verdict::Exited { code: 0 }
    );
    let run = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .find(|(_, body)| body["method"] == "exec.run")
        .map(|(_, body)| body["params"].clone())
        .unwrap();
    assert_eq!(
        run["progress"], true,
        "the cached catalog still enables progress"
    );
    assert_eq!(catalog_lists(), connected + 1);
    #[cfg(unix)]
    {
        client.reopen_route().await.unwrap();
        assert!(client.supports_submission_key().await.unwrap());
        // The reopened route resolves the executor and reads its capabilities.
        assert_eq!(catalog_lists(), connected + 3);
    }
}

#[tokio::test]
async fn stale_runon_route_reopens_once_for_concurrent_submissions() {
    let daemon = daemon_with_route_restart(
        Script::ReturnVector("two-files-one-empty"),
        &[client::CAPABILITY, RETURN_CHUNKED_CAPABILITY],
        1,
        Some(RouteRestart::BeforeRun),
    )
    .await;
    let client = client(&daemon).await;
    let other = client.clone();
    let request = |command| {
        RunRequest::new(
            "/workspace/task",
            "/workspace/repo",
            "/workspace/task",
            command,
        )
        .with_return_paths(vec!["dist/hello.txt".into(), "dist/empty.txt".into()])
        .with_return_chunked(true)
        .with_queue_wait_limit_s(5)
    };
    let (first, second) = (request("first"), request("second"));
    let (first, second) = tokio::join!(client.run(&first), other.run(&second));
    for stream in [first, second] {
        let mut sink = MemorySink::default();
        let verdict = drain(&mut stream.unwrap(), &mut sink).await.unwrap();
        assert_eq!(verdict, Verdict::Exited { code: 0 });
        assert_eq!(sink.terminals, [verdict]);
    }
    let log = daemon.log.lock().unwrap();
    // Initial route, a warming refusal, and one successful replacement.
    assert_eq!(
        log.iter()
            .filter(|(_, body)| body["op"] == "route.open")
            .count(),
        3
    );
    let mut commands = Vec::new();
    for (header, body) in log.iter().filter(|(_, body)| body["method"] == "exec.run") {
        assert_eq!(header.epoch, 2);
        assert_eq!(
            body["params"]["return_paths"],
            json!(["dist/hello.txt", "dist/empty.txt"])
        );
        commands.push(body["params"]["command"].as_str().unwrap());
    }
    commands.sort_unstable();
    assert_eq!(commands, ["first", "second"]);
}

#[tokio::test]
async fn stale_runon_route_reattaches_accepted_job_without_resubmitting() {
    let daemon = daemon_with_route_restart(
        Script::Restart,
        &[client::CAPABILITY],
        1,
        Some(RouteRestart::AfterAccepted),
    )
    .await;
    let client = client(&daemon).await;
    let request = RunRequest::new("/workspace", "/src/repo", "/workspace", "test");
    let mut stream = client.run(&request).await.unwrap();
    let mut sink = MemorySink::default();
    let Err(Error::RecoveryRequired {
        resume: Some(point),
        ..
    }) = drain(&mut stream, &mut sink).await
    else {
        panic!("route close should preserve the accepted job's resume point");
    };
    assert_eq!(point.job_id, id());
    assert_eq!(point.last_seq, Some(1));
    let mut stream = client.attach(point).await.unwrap();
    let verdict = drain(&mut stream, &mut sink).await.unwrap();
    assert_eq!(verdict, Verdict::Exited { code: 0 });
    assert_eq!(sink.stdout, b"ABCD");
    assert_eq!(sink.terminals, [verdict]);

    let log = daemon.log.lock().unwrap();
    let operations: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"].is_string())
        .collect();
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0].0.epoch, 1);
    assert_eq!(operations[0].1["method"], "exec.run");
    assert_eq!(operations[1].0.epoch, 2);
    assert_eq!(operations[1].1["method"], "exec.attach");
    assert_eq!(operations[1].1["params"]["job_id"], json!(id()));
    assert_eq!(operations[1].1["params"]["from_seq"], 2);
}

#[tokio::test]
async fn dotted_capability_is_not_a_discovery_claim() {
    let daemon = daemon(Script::Lost, "exec.remote/v1").await;
    let result =
        ExecRemoteClient::connect(&daemon.connection, BindIdentity::new("aft-test", "session"))
            .await;
    assert!(matches!(
        result,
        Err(Error::Transport(
            subc_client_rs::CallError::CapabilityUnprovided { .. }
        ))
    ));
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .all(|(_, body)| body["op"] != "route.open"));
}

#[tokio::test]
async fn interrupted_call_reattaches_with_next_durable_seq_without_resubmission() {
    let daemon = daemon(Script::Restart, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut sink = MemorySink::default();
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let Err(Error::RecoveryRequired {
        resume: Some(point),
        ..
    }) = drain(&mut stream, &mut sink).await
    else {
        panic!("missing terminal requires recovery")
    };
    assert_eq!(point.last_seq, Some(1));
    let mut resumed = client.attach(point).await.unwrap();
    assert_eq!(
        drain(&mut resumed, &mut sink).await.unwrap(),
        Verdict::Exited { code: 0 }
    );
    assert_eq!(sink.stdout, b"ABCD");
    let log = daemon.log.lock().unwrap();
    let attach = log
        .iter()
        .find(|(_, body)| body["method"] == "exec.attach")
        .unwrap();
    assert_eq!(attach.1["params"]["from_seq"], 2);
    assert_eq!(
        log.iter()
            .filter(|(_, body)| body["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn accepted_transport_loss_reconnects_and_replays_terminal_exactly_once() {
    let daemon = outage_daemon().await;
    let identity = subc_protocol::BindIdentity::new("test", "session");
    let client = ExecRemoteClient::connect(&daemon.connection, identity.clone())
        .await
        .unwrap();
    let request = RunRequest::new(
        "/workspace/task",
        "/src/repo",
        "/workspace/task",
        "cargo test",
    )
    .with_submission_key("aft:accepted-transport-loss");
    let mut stream = client.run(&request).await.unwrap();
    let mut sink = MemorySink::default();
    let Err(Error::RecoveryRequired {
        resume: Some(point),
        ..
    }) = drain(&mut stream, &mut sink).await
    else {
        panic!("transport loss after acceptance must preserve its job ID")
    };
    assert_eq!(point.job_id, id());
    assert_eq!(point.last_seq, Some(1));
    assert_eq!(sink.stdout, b"A");
    assert!(sink.terminals.is_empty());
    daemon.restore();
    let reconnected = ExecRemoteClient::connect(&daemon.connection, identity)
        .await
        .unwrap();
    let mut attached = reconnected.attach(point).await.unwrap();
    let verdict = drain(&mut attached, &mut sink).await.unwrap();
    assert_eq!(verdict, Verdict::Exited { code: 0 });
    assert_eq!(sink.stdout, b"AB");
    assert_eq!(sink.seqs, vec![0, 1, 2]);
    assert_eq!(sink.terminals, vec![verdict]);
    assert!(matches!(
        attached.next(&mut sink).await.unwrap(),
        StreamProgress::Complete(Verdict::Exited { code: 0 })
    ));
    assert_eq!(sink.terminals.len(), 1);
    assert_eq!(daemon.methods("exec.run").len(), 1);
    assert_eq!(daemon.methods("exec.attach")[0]["params"]["from_seq"], 2);
}

#[tokio::test]
async fn persistent_terminal_gap_completes_after_three_attaches() {
    let daemon = daemon(Script::PersistentTerminalGap, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut sink = MemorySink::default();
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let verdict = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match drain(&mut stream, &mut sink).await {
                Ok(verdict) => return verdict,
                Err(Error::RecoveryRequired {
                    resume: Some(point),
                    ..
                }) => {
                    stream = client.attach(point).await.unwrap();
                }
                other => panic!("unexpected gap recovery: {other:?}"),
            }
        }
    })
    .await
    .expect("a repeated terminal gap must not reattach forever");
    assert_eq!(verdict, Verdict::Exited { code: 7 });
    assert_eq!(sink.stdout, b"BC");
    assert_eq!(sink.seqs, [1, 2]);
    assert_eq!(sink.truncations, [1]);
    let log = daemon.log.lock().unwrap();
    let from: Vec<_> = log
        .iter()
        .filter(|(_, b)| b["method"] == "exec.attach")
        .map(|(_, b)| b["params"]["from_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(from, [0, 0, 0]);
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn transient_terminal_gap_fills_on_next_attach_without_truncation() {
    let daemon = daemon(Script::TransientTerminalGap, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut sink = MemorySink::default();
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let Err(Error::RecoveryRequired {
        resume: Some(point),
        ..
    }) = drain(&mut stream, &mut sink).await
    else {
        panic!("the first stream must have a gap");
    };
    assert!(sink.stdout.is_empty());
    let mut resumed = client.attach(point).await.unwrap();
    assert_eq!(
        drain(&mut resumed, &mut sink).await.unwrap(),
        Verdict::Exited { code: 7 }
    );
    assert_eq!(sink.stdout, b"ABC");
    assert_eq!(sink.seqs, [0, 1, 2]);
    assert!(sink.truncations.is_empty());
}

#[tokio::test]
async fn held_bash_contract_request_cancel_detaches_exec_observation() {
    let daemon = daemon(Script::DetachSafeStages { position: 1 }, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let mut sink = MemorySink::default();
    assert_eq!(
        stream.next(&mut sink).await.unwrap(),
        StreamProgress::Record
    );
    // This API unsubscribes the request's stream and then attaches. Unlike exec.cancel,
    // the request-level frame must leave the job's outcome unchanged.
    let mut reattached = client.cancel(&stream).await.unwrap();
    assert_eq!(
        drain(&mut reattached, &mut sink).await.unwrap(),
        Verdict::Exited { code: 0 }
    );
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(header, _)| header.ty == FrameType::Cancel)
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|(_, body)| body["method"] == "exec.cancel")
            .count(),
        0
    );
    assert_eq!(
        log.iter()
            .filter(|(_, body)| body["method"] == "exec.attach")
            .count(),
        1
    );
}

#[tokio::test]
async fn active_cancel_uses_call_key_then_attach_observes_cancel_not_deadline() {
    let daemon = daemon(Script::Cancel, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let mut sink = MemorySink::default();
    assert_eq!(
        stream.next(&mut sink).await.unwrap(),
        StreamProgress::Record
    );
    let mut cancelled = client.cancel(&stream).await.unwrap();
    assert_eq!(
        drain(&mut cancelled, &mut sink).await.unwrap(),
        Verdict::CancelKilled
    );
    let log = daemon.log.lock().unwrap();
    let run = log
        .iter()
        .find(|(_, body)| body["method"] == "exec.run")
        .unwrap()
        .0;
    let cancel = log
        .iter()
        .find(|(header, _)| header.ty == FrameType::Cancel)
        .unwrap()
        .0;
    assert_eq!(
        (cancel.channel, cancel.epoch, cancel.corr),
        (run.channel, run.epoch, run.corr)
    );
}

#[tokio::test]
async fn end_without_terminal_returns_recovery_promptly() {
    let daemon = daemon(Script::MissingTerminal, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    assert!(matches!(
        drain(&mut stream, &mut MemorySink::default()).await,
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(client.status().await.unwrap().server_reachable);
}

#[tokio::test]
async fn empty_attach_reports_no_frames_and_keeps_accepted_resume_point() {
    let daemon = daemon(Script::MissingTerminal, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let mut sink = MemorySink::default();
    assert!(!stream.received_frame());
    assert!(matches!(
        drain(&mut stream, &mut sink).await,
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(
        stream.received_frame(),
        "acceptance is a frame even without output"
    );
    let point = stream.resume_point().unwrap();
    assert_eq!(point.job_id, id());
    let mut attached = client.attach(point.clone()).await.unwrap();
    assert!(matches!(
        drain(&mut attached, &mut sink).await,
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(
        !attached.received_frame(),
        "StreamEnd alone must not reset the empty-attach budget"
    );
    assert_eq!(attached.resume_point(), Some(point));
}

#[tokio::test]
async fn return_cap_defaults_to_10_mib_without_chunked_returns() {
    let daemon = daemon(
        Script::ReturnVector("two-files-one-empty"),
        client::CAPABILITY,
    )
    .await;
    let client = client(&daemon).await;
    assert_eq!(client.return_max_bytes().await.unwrap(), 10_485_760);
}

#[tokio::test]
async fn return_cap_selection_obeys_the_selected_providers_v3_claim() {
    for (claims, expected) in [
        (
            vec![client::CAPABILITY, RETURN_CHUNKED_CAPABILITY],
            10_485_760,
        ),
        (
            vec![
                client::CAPABILITY,
                RETURN_CHUNKED_CAPABILITY,
                RETURN_LARGE_CAPABILITY,
            ],
            134_217_728,
        ),
    ] {
        let daemon =
            daemon_with_capabilities(Script::ReturnVector("two-files-one-empty"), &claims, 1).await;
        let client = client(&daemon).await;
        assert_eq!(client.return_max_bytes().await.unwrap(), expected);
        let request = RunRequest::new(
            "/workspace/task",
            "/workspace/repo",
            "/workspace/task",
            "build",
        )
        .with_return_paths(vec!["dist/hello.txt".into(), "dist/empty.txt".into()])
        .with_return_chunked(true)
        .with_return_max_bytes(RETURN_LARGE_MAX_BYTES_CAP);
        let mut stream = client.run(&request).await.unwrap();
        let mut sink = MemorySink::default();
        loop {
            if matches!(
                stream.next(&mut sink).await.unwrap(),
                client::StreamProgress::Complete(_)
            ) {
                break;
            }
        }
        let runs: Vec<_> = daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["method"] == "exec.run")
            .map(|(_, body)| body["params"].clone())
            .collect();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["return_max_bytes"], expected);
    }
}

#[tokio::test]
async fn return_cap_refusal_resubmits_once_and_no_other_refusal_is_retried() {
    let cases = [
        (
            "invalid_params",
            "return_max_bytes",
            Some(20 * 1024 * 1024),
            false,
            false,
            2,
            true,
        ),
        (
            "invalid_params",
            "return_max_bytes",
            Some(20 * 1024 * 1024),
            true,
            false,
            2,
            false,
        ),
        (
            "invalid_params",
            "return_max_bytes",
            None,
            false,
            false,
            1,
            false,
        ),
        (
            "invalid_params",
            "return_paths",
            Some(10_485_760),
            false,
            false,
            1,
            false,
        ),
        (
            "not_ready",
            "return_max_bytes",
            Some(10_485_760),
            false,
            false,
            1,
            false,
        ),
        (
            "invalid_params",
            "return_max_bytes",
            Some(134_217_728),
            false,
            false,
            1,
            false,
        ),
        (
            "invalid_params",
            "return_max_bytes",
            Some(10_485_760),
            false,
            true,
            1,
            false,
        ),
    ];
    for (code, field, max_bytes, always, after_accepted, count, completes) in cases {
        let daemon = daemon_with_capabilities(
            Script::ReturnCapRefusal {
                code,
                field,
                max_bytes,
                always,
                after_accepted,
            },
            &[
                client::CAPABILITY,
                RETURN_CHUNKED_CAPABILITY,
                RETURN_LARGE_CAPABILITY,
            ],
            1,
        )
        .await;
        let client = client(&daemon).await;
        let request = RunRequest::new(
            "/workspace/task",
            "/workspace/repo",
            "/workspace/task",
            "build",
        )
        .with_return_paths(vec!["dist/hello.txt".into(), "dist/empty.txt".into()])
        .with_return_chunked(true)
        .with_return_max_bytes(RETURN_LARGE_MAX_BYTES_CAP);
        let mut stream = client.run(&request).await.unwrap();
        let mut sink = MemorySink::default();
        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match stream.next(&mut sink).await {
                    Ok(client::StreamProgress::Complete(_)) => break true,
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("{code}/{field}/{max_bytes:?}: {error:?}");
                        break false;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(completed, completes, "{code}/{field}/{max_bytes:?}");
        let runs: Vec<_> = daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["method"] == "exec.run")
            .map(|(_, body)| body["params"].clone())
            .collect();
        assert_eq!(runs.len(), count, "{code}/{field}/{max_bytes:?}");
        if count == 2 {
            let mut expected = runs[0].clone();
            expected["return_max_bytes"] = json!(max_bytes.unwrap());
            assert_eq!(
                runs[1], expected,
                "only the cap changes on the safe resubmit"
            );
        }
    }
}

#[tokio::test]
async fn return_cap_refusal_preserves_memory_size_on_the_single_resubmit() {
    let daemon = daemon_with_capabilities(
        Script::ReturnCapRefusal {
            code: "invalid_params",
            field: "return_max_bytes",
            max_bytes: Some(20 * 1024 * 1024),
            always: false,
            after_accepted: false,
        },
        &[
            client::CAPABILITY,
            MEMORY_CAPABILITY,
            RETURN_CHUNKED_CAPABILITY,
            RETURN_LARGE_CAPABILITY,
        ],
        1,
    )
    .await;
    let client = client(&daemon).await;
    let request = RunRequest::new(
        "/workspace/task",
        "/workspace/repo",
        "/workspace/task",
        "build",
    )
    .with_weight_hint(2)
    .with_memory_mib(16_384)
    .with_return_paths(vec!["dist/hello.txt".into(), "dist/empty.txt".into()])
    .with_return_chunked(true)
    .with_return_max_bytes(RETURN_LARGE_MAX_BYTES_CAP);
    let mut stream = client.run(&request).await.unwrap();
    let mut sink = MemorySink::default();
    let verdict = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let client::StreamProgress::Complete(verdict) = stream.next(&mut sink).await.unwrap()
            {
                break verdict;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(verdict, Verdict::Exited { code: 0 });
    let runs: Vec<_> = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.run")
        .map(|(_, body)| body["params"].clone())
        .collect();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["return_max_bytes"], 134_217_728);
    assert_eq!(runs[1]["return_max_bytes"], 20_971_520);
    for run in &runs {
        assert_eq!(run["memory_mib"], 16_384);
    }
    let mut expected = runs[0].clone();
    expected["return_max_bytes"] = json!(20_971_520);
    assert_eq!(
        runs[1], expected,
        "memory and every other identity field stay unchanged"
    );
}
