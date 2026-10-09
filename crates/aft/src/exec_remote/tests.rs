use super::*;
use cortexkit_exec_remote_types::*;

fn job_id() -> Uuid {
    "0192a64a-1234-7000-8000-000000000001".parse().unwrap()
}

#[test]
fn accepted_021_vector_and_single_locked_contract_are_pinned() {
    use sha2::{Digest, Sha256};
    let canonical = include_bytes!("fixtures/frames/accepted.jcs");
    assert_eq!(
        format!("{:x}", Sha256::digest(canonical)),
        include_str!("fixtures/frames/accepted.sha256")
    );
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/frames/accepted.json")).unwrap();
    // This vector contains only ASCII strings and small integers. Sorting its
    // object keys is the independent JCS step, regardless of serde map order.
    value.as_object_mut().unwrap().sort_keys();
    assert_eq!(serde_json::to_vec(&value).unwrap(), canonical);
    let record: StreamRecord = serde_json::from_value(value).unwrap();
    let StreamRecord::Accepted(accepted) = record else {
        panic!("accepted vector must decode as acceptance")
    };
    assert_eq!(accepted.env_not_forwarded, Some(Vec::new()));
    let lock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock: toml::Value = std::fs::read_to_string(lock).unwrap().parse().unwrap();
    let packages: Vec<_> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"].as_str() == Some("cortexkit-exec-remote-types"))
        .collect();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["version"].as_str(), Some("0.2.2"));
}

fn vector_cases(directory: &str) -> Vec<(String, serde_json::Value)> {
    use sha2::{Digest, Sha256};
    // Retain the published 0.2.0 grading corpus and its original digests;
    // the additive 0.2.1 accepted-frame contract is pinned separately above,
    // and the additive 0.2.2 run-report vectors are rendered by the remote
    // bash tests (see `fixtures/SOURCE.md`).
    // Runtime cargo metadata resolves unrelated target dependencies even in
    // offline mode, so unit tests embed the corpus rather than requiring those
    // packages in the caller's Cargo cache. Keep the singleton version fence
    // explicit when updating the dependency.
    let lock: toml::Value = toml::from_str(include_str!("../../../../Cargo.lock")).unwrap();
    let versions = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| package["name"].as_str() == Some("cortexkit-exec-remote-types"))
        .map(|package| package["version"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(versions, ["0.2.2"], "exactly one caller contract version");
    let vectors: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/exec-remote/published-v0.2.0.json"
    ))
    .unwrap();
    let mut cases = Vec::new();
    for (name, vector) in vectors[directory].as_object().unwrap() {
        let bytes = vector["jcs"].as_str().unwrap().as_bytes();
        assert_eq!(
            format!("{:x}", Sha256::digest(bytes)),
            vector["sha256"].as_str().unwrap(),
            "{directory}/{name}"
        );
        cases.push((name.clone(), serde_json::from_slice(bytes).unwrap()));
    }
    cases.sort_by(|a, b| a.0.cmp(&b.0));
    cases
}

#[test]
fn published_outcomes_have_explicit_grades() {
    let cases = vector_cases("outcomes");
    assert_eq!(cases.len(), 26);
    for (name, value) in cases {
        let _: RunRequest = serde_json::from_value(value["request"].clone()).unwrap();
        let records: Vec<StreamRecord> = serde_json::from_value(value["stream"].clone()).unwrap();
        let StreamRecord::Terminal(terminal) = records.last().unwrap() else {
            panic!("{name}")
        };
        let expected = match name.as_str() {
            "exit" => Verdict::Exited { code: 0 },
            "pipestatus" => Verdict::Exited { code: 1 },
            "exit-nonzero" => Verdict::Exited { code: 100 },
            "signal" => Verdict::Signalled { signal: 15 },
            "cancelled" => Verdict::Cancelled,
            "killed-deadline" => Verdict::DeadlineKilled,
            "killed-cancel" => Verdict::CancelKilled,
            "outcome_unknown" | "outcome_unknown-queued" | "crate-local-unknown-outcome" => {
                Verdict::OutcomeUnknown
            }
            "crate-local-unknown-ran" | "crate-local-unknown-killed" => Verdict::OutcomeUnknown,
            "crate-local-unknown-stream-record" | "crate-local-unknown-output-stream" => {
                Verdict::Exited { code: 0 }
            }
            "history_expired" => Verdict::HistoryExpired,
            "bundle_rejected" => Verdict::RunLocally {
                reason: RefusalReason::BundleRejected,
            },
            "queue_wait_exceeded" => Verdict::RunLocally {
                reason: RefusalReason::QueueWaitExceeded,
            },
            "runner_full" => Verdict::RunLocally {
                reason: RefusalReason::RunnerFull,
            },
            "runner_version_mismatch" => Verdict::RunLocally {
                reason: RefusalReason::RunnerVersionMismatch,
            },
            "snapshot_failed" => Verdict::RunLocally {
                reason: RefusalReason::SnapshotFailed,
            },
            "transfer_interrupted" => Verdict::RunLocally {
                reason: RefusalReason::TransferInterrupted,
            },
            "tree_hash_mismatch" => Verdict::RunLocally {
                reason: RefusalReason::TreeHashMismatch,
            },
            "unreachable" => Verdict::RunLocally {
                reason: RefusalReason::Unreachable,
            },
            "workspace_key_rejected" => Verdict::RunLocally {
                reason: RefusalReason::WorkspaceKeyRejected,
            },
            "workspace_setup_failed" => Verdict::RunLocally {
                reason: RefusalReason::WorkspaceSetupFailed,
            },
            "crate-local-unknown-refusal" => Verdict::RunLocally {
                reason: RefusalReason::Unknown("future_refusal".into()),
            },
            _ => panic!("ungraded published case: {name}"),
        };
        assert_eq!(grade(terminal), expected, "{name}");
        // The two output/record future-tag goldens are attach excerpts: seq 7
        // without an accepted record. Resume from the prior retained cursor.
        let mut consumer = if matches!(
            name.as_str(),
            "crate-local-unknown-stream-record" | "crate-local-unknown-output-stream"
        ) {
            StreamConsumer::resume(ResumePoint {
                job_id: terminal.job_id,
                last_seq: Some(6),
                gap_recovery: None,
            })
        } else {
            StreamConsumer::new()
        };
        let mut sink = MemorySink::default();
        for record in records {
            consumer.consume(record, &mut sink).unwrap();
        }
        assert_eq!(consumer.finish().unwrap(), expected, "stream grade: {name}");
        assert_eq!(sink.terminals, [expected]);
        if name == "crate-local-unknown-stream-record" {
            assert_eq!(
                consumer
                    .resume_point()
                    .unwrap()
                    .attach_request()
                    .unwrap()
                    .from_seq,
                8
            );
            assert_eq!(sink.unknown, [(7, vec![])]);
        }
        if name == "crate-local-unknown-output-stream" {
            assert_eq!(
                consumer
                    .resume_point()
                    .unwrap()
                    .attach_request()
                    .unwrap()
                    .from_seq,
                8
            );
            assert_eq!(sink.unknown, [(7, vec![0xe2])]);
        }
    }
}

#[test]
fn control_known_outcome_unknown_never_reruns() {
    assert_eq!(
        grade(&TerminalRecord::new(
            job_id(),
            Outcome::OutcomeUnknown,
            0,
            0,
            0
        )),
        Verdict::OutcomeUnknown
    );
}

#[test]
fn control_known_history_expired_never_reruns() {
    assert_eq!(
        grade(&TerminalRecord::new(
            job_id(),
            Outcome::HistoryExpired,
            0,
            0,
            0
        )),
        Verdict::HistoryExpired
    );
}

#[derive(Default)]
pub(super) struct MemorySink {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) seqs: Vec<u64>,
    terminals: Vec<Verdict>,
    pub(super) truncations: Vec<u64>,
    unknown: Vec<(u64, Vec<u8>)>,
}

impl OutputSink for MemorySink {
    fn unknown_output(&mut self, seq: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.seqs.push(seq);
        self.unknown.push((seq, bytes.to_vec()));
        Ok(())
    }
    fn accepted(&mut self, _accepted: &Accepted) -> std::io::Result<()> {
        Ok(())
    }
    fn output(&mut self, seq: u64, stream: OutputStream, bytes: &[u8]) -> std::io::Result<()> {
        self.seqs.push(seq);
        match stream {
            OutputStream::Stdout => self.stdout.extend_from_slice(bytes),
            OutputStream::Stderr => self.stderr.extend_from_slice(bytes),
            _ => panic!("unexpected stream in known-output test"),
        }
        Ok(())
    }
    fn truncated(&mut self, seq: u64) -> std::io::Result<()> {
        self.truncations.push(seq);
        Ok(())
    }
    fn terminal(&mut self, _record: &TerminalRecord, verdict: &Verdict) -> std::io::Result<()> {
        self.terminals.push(verdict.clone());
        Ok(())
    }
}

fn accepted() -> StreamRecord {
    StreamRecord::Accepted(Accepted::new(job_id(), 1))
}
fn output(seq: u64, stream: OutputStream, bytes: &[u8]) -> StreamRecord {
    StreamRecord::Output(Output::new(seq, stream, BytePayload(bytes.to_vec())))
}
fn terminal(outcome: Outcome) -> StreamRecord {
    StreamRecord::Terminal(TerminalRecord::new(job_id(), outcome, 1, 0, 0))
}

#[test]
fn control_known_split_utf8_is_reassembled_as_exact_bytes() {
    let mut consumer = StreamConsumer::new();
    let mut sink = MemorySink::default();
    consumer.consume(accepted(), &mut sink).unwrap();
    // Three-byte Euro symbol split across non-adjacent stdout/stderr chunks.
    for record in [
        output(0, OutputStream::Stdout, b"A\xe2"),
        output(1, OutputStream::Stderr, b"\xfferr"),
        output(2, OutputStream::Stdout, b"\x82"),
        output(3, OutputStream::Stdout, b"\xacZ"),
    ] {
        consumer.consume(record, &mut sink).unwrap();
    }
    consumer
        .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
        .unwrap();
    assert_eq!(sink.stdout, "A€Z".as_bytes());
    assert_eq!(sink.stderr, b"\xfferr");
}

#[test]
fn control_known_seq_dedupe_keeps_first_record_in_sorted_order() {
    let mut consumer = StreamConsumer::new();
    let mut sink = MemorySink::default();
    consumer.consume(accepted(), &mut sink).unwrap();
    for record in [
        output(1, OutputStream::Stdout, b"B"),
        output(1, OutputStream::Stderr, b"wrong pending duplicate"),
        output(0, OutputStream::Stdout, b"A"),
        output(0, OutputStream::Stdout, b"wrong delivered duplicate"),
        output(2, OutputStream::Stderr, b"C"),
    ] {
        consumer.consume(record, &mut sink).unwrap();
    }
    consumer
        .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
        .unwrap();
    assert_eq!(sink.stdout, b"AB");
    assert_eq!(sink.stderr, b"C");
    assert_eq!(sink.seqs, [0, 1, 2]);
}

#[test]
fn restart_attach_after_n_chunks_has_no_duplicate_or_gap() {
    for n in 0..=6 {
        let mut consumer = StreamConsumer::new();
        let mut sink = MemorySink::default();
        consumer.consume(accepted(), &mut sink).unwrap();
        for seq in 0..n {
            consumer
                .consume(output(seq, OutputStream::Stdout, &[seq as u8]), &mut sink)
                .unwrap();
        }
        let resume = consumer.resume_point().unwrap();
        assert_eq!(resume.attach_request().unwrap().from_seq, n);
        let mut restored = StreamConsumer::resume(resume);
        // Replay an old chunk too, exercising dedupe across the durable cursor.
        for seq in n.saturating_sub(1)..6 {
            restored
                .consume(output(seq, OutputStream::Stdout, &[seq as u8]), &mut sink)
                .unwrap();
        }
        restored
            .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
            .unwrap();
        assert_eq!(sink.stdout, [0, 1, 2, 3, 4, 5], "after {n} chunks");
    }
}

#[test]
fn published_unary_replies_have_explicit_grades() {
    let cases = vector_cases("replies");
    assert_eq!(cases.len(), 16);
    for (name, value) in cases {
        let (operation, expected) = match name.as_str() {
            "cancel" => ("exec.cancel", ReplyVerdict::CancelAcknowledged),
            "drop-existing" => ("workspace.drop", ReplyVerdict::Dropped { existed: true }),
            "drop-missing" => ("workspace.drop", ReplyVerdict::Dropped { existed: false }),
            "prepare-prepared" => ("workspace.prepare", ReplyVerdict::Prepared),
            "crate-local-unknown-prepare-outcome" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared { reason: None },
            ),
            "crate-local-unknown-rebuild-result" => (
                "exec.status",
                ReplyVerdict::Status {
                    reachable: true,
                    has_unknown_rebuild: true,
                },
            ),
            "prepare-bundle_rejected" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::BundleRejected),
                },
            ),
            "prepare-runner_version_mismatch" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::RunnerVersionMismatch),
                },
            ),
            "prepare-snapshot_failed" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::SnapshotFailed),
                },
            ),
            "prepare-transfer_interrupted" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::TransferInterrupted),
                },
            ),
            "prepare-unreachable" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::Unreachable),
                },
            ),
            "prepare-workspace_key_rejected" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::WorkspaceKeyRejected),
                },
            ),
            "prepare-workspace_setup_failed" => (
                "workspace.prepare",
                ReplyVerdict::WorkspaceUnprepared {
                    reason: Some(RefusalReason::WorkspaceSetupFailed),
                },
            ),
            "status" | "status-cold" => (
                "exec.status",
                ReplyVerdict::Status {
                    reachable: true,
                    has_unknown_rebuild: false,
                },
            ),
            "status-unreachable" => (
                "exec.status",
                ReplyVerdict::Status {
                    reachable: false,
                    has_unknown_rebuild: false,
                },
            ),
            _ => panic!("ungraded published reply: {name}"),
        };
        let reply = decode_reply(operation, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(grade_reply(&reply), expected, "{name}");
    }
}

#[test]
fn request_copies_shell_inputs_without_executor_metadata() {
    let root = tempfile::tempdir().unwrap();
    let worktree = root.path().canonicalize().unwrap();
    let cwd = worktree.join("subdir");
    std::fs::create_dir(&cwd).unwrap();
    let env = std::collections::BTreeMap::from([
        ("RUSTFLAGS".into(), "-Dwarnings".into()),
        ("SHELL_SECRET".into(), "must stay on caller".into()),
    ]);
    let preset = PresetParams {
        siblings: vec![worktree.to_str().unwrap().into()],
        weight_hint: Some(16),
        queue_wait_limit_s: Some(0),
    };
    let request = build_request(
        &worktree,
        &worktree,
        &cwd,
        "cargo test",
        env.clone(),
        Some(23),
        &preset,
    )
    .unwrap();
    assert_eq!(
        request.env,
        std::collections::BTreeMap::from([("RUSTFLAGS".into(), "-Dwarnings".into())])
    );
    assert_eq!(request.workspace_key, worktree.to_str().unwrap());
    assert_eq!(request.cwd, cwd.to_str().unwrap());
    assert_eq!(request.timeout, Some(23));
    assert_eq!(request.queue_wait_limit_s, Some(0));
    assert_eq!(request.weight_hint, Some(16));
    assert_eq!(request.siblings, preset.siblings);
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire.as_object().unwrap().len(), 9);
    for key in [
        "job_id",
        "bundles",
        "snapshot",
        "tree_hash",
        "integration_tip",
    ] {
        assert!(wire.get(key).is_none());
    }
    assert!(build_request(
        &worktree,
        &worktree,
        worktree.parent().unwrap(),
        "cargo test",
        env,
        None,
        &preset
    )
    .is_err());
    assert!(build_request(
        std::path::Path::new("relative"),
        &worktree,
        &cwd,
        "cargo test",
        Default::default(),
        None,
        &preset
    )
    .is_err());
}

#[test]
fn request_strips_secret_shaped_and_control_environment_names() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let denied = [
        "MY_TOKEN",
        "SHELL_SECRET",
        "PASSWORD",
        "DB_PASSWD",
        "PRIVATE_KEY_FILE",
        "SERVICE_API_KEY",
        "USER_CREDENTIAL",
        "AWS_REGION",
        "AWS_ACCESS_KEY_ID",
        "GH_REPO",
        "GITHUB_TOKEN",
        "NPM_TOKEN",
        "CARGO_REGISTRY_TOKEN",
        "SSH_AUTH_SOCK",
        "NEXTEST_TEST_THREADS",
        "RUST_TEST_THREADS",
        "AFT_STORAGE_DIR",
        "AFT_TEST_CONTROL",
        "CORTEXKIT_CONTROL_SOCKET",
        "CK_CONTROL_PATH",
        "SUBC_MODULE_ID",
        "SUBC_LAUNCH_NONCE",
    ];
    for name in denied {
        for spelling in [name.to_owned(), name.to_ascii_lowercase()] {
            let env = std::collections::BTreeMap::from([
                (spelling.clone(), "sensitive-fixture-value".into()),
                ("FOO".into(), "ordinary".into()),
                ("RUSTFLAGS".into(), "-Dwarnings".into()),
            ]);
            let request = build_request(
                &root,
                &root,
                &root,
                "cargo test",
                env,
                None,
                &PresetParams::default(),
            )
            .unwrap();
            assert!(
                !request.env.contains_key(&spelling),
                "{spelling} left the caller"
            );
            assert_eq!(request.env["FOO"], "ordinary");
            assert_eq!(request.env["RUSTFLAGS"], "-Dwarnings");
            assert!(!serde_json::to_string(&request)
                .unwrap()
                .contains("sensitive-fixture-value"));
        }
    }
}

#[cfg(unix)]
#[test]
fn cwd_symlink_escape_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let worktree = root.path().canonicalize().unwrap();
    let link = worktree.join("escape");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    assert!(build_request(
        &worktree,
        &worktree,
        &link,
        "cargo test",
        Default::default(),
        None,
        &PresetParams::default()
    )
    .is_err());
}

#[test]
fn absent_metadata_stays_absent_and_remote_changes_stay_listed() {
    let before = TerminalRecord::new(
        job_id(),
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::SnapshotFailed,
        },
        0,
        0,
        0,
    );
    let mut consumer = StreamConsumer::new();
    let mut sink = RecordSink::default();
    consumer
        .consume(StreamRecord::Terminal(before.clone()), &mut sink)
        .unwrap();
    assert_eq!(sink.record, Some(before));
    let after = TerminalRecord::new(job_id(), Outcome::Exit { code: 0 }, 1, 2, 3)
        .with_ran(Ran::Remote)
        .with_tree_hash("hash")
        .with_workspace_changes(vec!["result.txt".into()]);
    let mut consumer = StreamConsumer::new();
    consumer.consume(accepted(), &mut sink).unwrap();
    consumer
        .consume(StreamRecord::Terminal(after.clone()), &mut sink)
        .unwrap();
    assert_eq!(sink.record, Some(after));
}

#[derive(Default)]
struct RecordSink {
    record: Option<TerminalRecord>,
}
impl OutputSink for RecordSink {
    fn accepted(&mut self, _: &Accepted) -> std::io::Result<()> {
        Ok(())
    }
    fn output(&mut self, _: u64, _: OutputStream, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn truncated(&mut self, _: u64) -> std::io::Result<()> {
        Ok(())
    }
    fn unknown_output(&mut self, _: u64, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn terminal(&mut self, record: &TerminalRecord, _: &Verdict) -> std::io::Result<()> {
        self.record = Some(record.clone());
        Ok(())
    }
}

#[test]
fn gaps_and_missing_terminal_require_attach_not_a_local_run() {
    let mut consumer = StreamConsumer::new();
    let mut sink = MemorySink::default();
    consumer.consume(accepted(), &mut sink).unwrap();
    consumer
        .consume(output(1, OutputStream::Stdout, b"B"), &mut sink)
        .unwrap();
    assert_eq!(consumer.resume_point().unwrap().last_seq, None);
    assert!(matches!(
        consumer.consume(terminal(Outcome::Exit { code: 0 }), &mut sink),
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(matches!(
        consumer.finish(),
        Err(Error::RecoveryRequired { .. })
    ));
    let point = consumer.resume_point().unwrap();
    let mut replay = StreamConsumer::resume(point);
    replay
        .consume(output(0, OutputStream::Stdout, b"A"), &mut sink)
        .unwrap();
    replay
        .consume(output(1, OutputStream::Stdout, b"B"), &mut sink)
        .unwrap();
    replay
        .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
        .unwrap();
    assert_eq!(sink.stdout, b"AB");
    assert_eq!(replay.finish().unwrap(), Verdict::Exited { code: 0 });
}

#[test]
fn retained_history_truncation_is_explicit_and_replay_continues() {
    let mut consumer = StreamConsumer::resume(ResumePoint {
        job_id: job_id(),
        last_seq: Some(0),
        gap_recovery: None,
    });
    let mut sink = MemorySink::default();
    consumer
        .consume(
            StreamRecord::Output(
                Output::new(5, OutputStream::Stdout, BytePayload(b"F".to_vec()))
                    .with_truncated_before_seq(5),
            ),
            &mut sink,
        )
        .unwrap();
    consumer
        .consume(output(6, OutputStream::Stdout, b"G"), &mut sink)
        .unwrap();
    assert_eq!(sink.truncations, [5]);
    assert_eq!(sink.stdout, b"FG");
    assert_eq!(
        consumer
            .resume_point()
            .unwrap()
            .attach_request()
            .unwrap()
            .from_seq,
        7
    );
}

#[test]
fn multiple_terminals_and_cross_job_records_are_rejected() {
    let mut consumer = StreamConsumer::new();
    let mut sink = MemorySink::default();
    consumer.consume(accepted(), &mut sink).unwrap();
    let wrong = TerminalRecord::new(Uuid::nil(), Outcome::Exit { code: 0 }, 1, 0, 0);
    assert!(consumer
        .consume(StreamRecord::Terminal(wrong), &mut sink)
        .is_err());
    consumer
        .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
        .unwrap();
    assert!(consumer
        .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
        .is_err());
    assert_eq!(sink.terminals.len(), 1);
}

#[test]
fn control_unknown_ran_never_proves_no_start() {
    let mut wire = serde_json::to_value(TerminalRecord::new(
        job_id(),
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable,
        },
        0,
        0,
        0,
    ))
    .unwrap();
    wire["ran"] = serde_json::json!("future_location");
    let terminal: TerminalRecord = serde_json::from_value(wire).unwrap();
    assert_eq!(grade(&terminal), Verdict::OutcomeUnknown);
}

#[test]
fn control_unknown_record_seq_advances_resume() {
    let mut consumer = StreamConsumer::new();
    let mut sink = MemorySink::default();
    consumer.consume(accepted(), &mut sink).unwrap();
    consumer
        .consume_bytes(
            br#"{"type":"future_record","seq":0,"future":true}"#,
            &mut sink,
        )
        .unwrap();
    assert_eq!(
        consumer
            .resume_point()
            .unwrap()
            .attach_request()
            .unwrap()
            .from_seq,
        1
    );
    consumer
        .consume(output(1, OutputStream::Stdout, b"B"), &mut sink)
        .unwrap();
    consumer
        .consume(terminal(Outcome::Exit { code: 0 }), &mut sink)
        .unwrap();
    assert_eq!(consumer.finish().unwrap(), Verdict::Exited { code: 0 });
    assert_eq!(sink.stdout, b"B");
    assert_eq!(sink.unknown, [(0, vec![])]);
}

#[test]
fn unknown_killed_is_not_an_ordinary_signal_or_cancel() {
    let mut wire = serde_json::to_value(TerminalRecord::new(
        job_id(),
        Outcome::Signal { signal: 15 },
        1,
        0,
        0,
    ))
    .unwrap();
    wire["killed"] = serde_json::json!("future_kill");
    let terminal: TerminalRecord = serde_json::from_value(wire).unwrap();
    assert_eq!(grade(&terminal), Verdict::OutcomeUnknown);
}

#[test]
fn unknown_output_stream_preserves_bytes_and_sequence_without_misattribution() {
    let mut consumer = StreamConsumer::new();
    let mut sink = MemorySink::default();
    consumer.consume(accepted(), &mut sink).unwrap();
    consumer
        .consume_bytes(
            br#"{"type":"output","seq":0,"stream":"future_fd","bytes":"/w=="}"#,
            &mut sink,
        )
        .unwrap();
    consumer
        .consume(output(1, OutputStream::Stderr, b"error"), &mut sink)
        .unwrap();
    assert_eq!(sink.unknown, [(0, vec![255])]);
    assert!(sink.stdout.is_empty());
    assert_eq!(sink.stderr, b"error");
    assert_eq!(
        consumer
            .resume_point()
            .unwrap()
            .attach_request()
            .unwrap()
            .from_seq,
        2
    );
}

#[test]
fn unknown_prepare_outcome_does_not_establish_a_workspace() {
    let bytes = serde_json::to_vec(
        &serde_json::json!({"transfer_id":job_id(), "outcome":{"type":"future_prepare"}}),
    )
    .unwrap();
    let reply = decode_reply("workspace.prepare", &bytes).unwrap();
    assert_eq!(
        grade_reply(&reply),
        ReplyVerdict::WorkspaceUnprepared { reason: None }
    );
}

#[test]
fn unknown_rebuild_result_is_not_a_successful_warm_generation() {
    let bytes = serde_json::to_vec(&serde_json::json!({"queue_depth":0,"running_jobs":[],"server_reachable":true,"rustc_version":"rustc test",
        "repositories":[{"repository_root":"/src/repo", "warm_target_age_s":null,"published_commit":null,"last_rebuild_result":"future_rebuild"}]})).unwrap();
    let reply = decode_reply("exec.status", &bytes).unwrap();
    assert_eq!(
        grade_reply(&reply),
        ReplyVerdict::Status {
            reachable: true,
            has_unknown_rebuild: true
        }
    );
}

#[test]
fn unknown_records_without_a_terminal_require_recovery() {
    let mut consumer = StreamConsumer::resume(ResumePoint {
        job_id: job_id(),
        last_seq: None,
        gap_recovery: None,
    });
    let mut sink = MemorySink::default();
    consumer
        .consume_bytes(br#"{"type":"future_terminal","seq":0}"#, &mut sink)
        .unwrap();
    assert!(matches!(
        consumer.finish(),
        Err(Error::RecoveryRequired {
            resume: Some(ResumePoint {
                last_seq: Some(0),
                ..
            }),
            ..
        })
    ));
}

#[test]
fn terminal_gap_progress_resets_the_bound_for_a_new_cursor() {
    let mut sink = MemorySink::default();
    let mut consumer = StreamConsumer::new();
    consumer.consume(accepted(), &mut sink).unwrap();
    for attempt in 0..3 {
        consumer
            .consume(output(1, OutputStream::Stdout, b"B"), &mut sink)
            .unwrap();
        assert!(matches!(
            consumer.consume(terminal(Outcome::Exit { code: 7 }), &mut sink),
            Err(Error::RecoveryRequired { .. })
        ));
        let point = consumer.resume_point().unwrap();
        assert_eq!(point.gap_recovery.as_ref().unwrap().reattaches, attempt);
        consumer = StreamConsumer::resume(point);
    }
    consumer
        .consume(output(0, OutputStream::Stdout, b"A"), &mut sink)
        .unwrap();
    consumer
        .consume(output(3, OutputStream::Stdout, b"D"), &mut sink)
        .unwrap();
    assert!(matches!(
        consumer.consume(terminal(Outcome::OutcomeUnknown), &mut sink),
        Err(Error::RecoveryRequired { .. })
    ));
    let point = consumer.resume_point().unwrap();
    let gap = point.gap_recovery.as_ref().unwrap();
    assert_eq!(gap.from_seq, 2);
    assert_eq!(gap.reattaches, 0);
    assert_eq!(sink.stdout, b"AB");
    assert!(sink.truncations.is_empty());
    consumer = StreamConsumer::resume(point);
    consumer
        .consume(output(2, OutputStream::Stdout, b"C"), &mut sink)
        .unwrap();
    assert_eq!(
        consumer.finish_recovery(&mut sink).unwrap(),
        Verdict::Exited { code: 7 }
    );
    assert_eq!(sink.stdout, b"ABCD");
    assert!(sink.truncations.is_empty());
}

#[test]
fn output_gaps_without_terminal_proof_remain_recoverable() {
    let mut sink = MemorySink::default();
    let mut consumer = StreamConsumer::new();
    consumer.consume(accepted(), &mut sink).unwrap();
    for _ in 0..6 {
        consumer
            .consume(output(1, OutputStream::Stdout, b"B"), &mut sink)
            .unwrap();
        assert!(matches!(
            consumer.finish_recovery(&mut sink),
            Err(Error::RecoveryRequired { .. })
        ));
        let point = consumer.resume_point().unwrap();
        assert!(point.gap_recovery.is_none());
        assert_eq!(point.last_seq, None);
        consumer = StreamConsumer::resume(point);
    }
    assert!(sink.stdout.is_empty());
    assert!(sink.truncations.is_empty());
    assert!(sink.terminals.is_empty());
}

#[test]
fn terminal_gap_loses_each_missing_range_but_drains_later_records_in_order() {
    let mut sink = MemorySink::default();
    let mut consumer = StreamConsumer::new();
    consumer.consume(accepted(), &mut sink).unwrap();
    consumer
        .consume(output(4, OutputStream::Stdout, b"E"), &mut sink)
        .unwrap();
    consumer
        .consume(output(1, OutputStream::Stdout, b"B"), &mut sink)
        .unwrap();
    assert!(matches!(
        consumer.consume(terminal(Outcome::Exit { code: 7 }), &mut sink),
        Err(Error::RecoveryRequired { .. })
    ));
    for attempt in 1..=3 {
        consumer = StreamConsumer::resume(consumer.resume_point().unwrap());
        let result = consumer.finish_recovery(&mut sink);
        if attempt < 3 {
            assert!(matches!(result, Err(Error::RecoveryRequired { .. })));
        } else {
            assert_eq!(result.unwrap(), Verdict::Exited { code: 7 });
        }
    }
    assert_eq!(sink.stdout, b"BE");
    assert_eq!(sink.seqs, [1, 4]);
    assert_eq!(sink.truncations, [1, 4]);
}
