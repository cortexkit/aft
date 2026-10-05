use super::*;
use cortexkit_exec_remote_types::*;
use std::path::PathBuf;
use std::sync::OnceLock;

fn job_id() -> Uuid {
    "0192a64a-1234-7000-8000-000000000001".parse().unwrap()
}

// Resolve the locked, published package rather than a second copy of its corpus.
// Metadata runs offline, so these tests cannot silently download new goldens.
fn vectors() -> &'static std::path::Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let output = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--offline", "--locked", "--format-version", "1"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let packages: Vec<_> = metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["name"] == "cortexkit-exec-remote-types")
            .collect();
        assert_eq!(packages.len(), 1, "exactly one caller contract version");
        assert_eq!(packages[0]["version"], "0.1.0");
        PathBuf::from(packages[0]["manifest_path"].as_str().unwrap())
            .parent()
            .unwrap()
            .join("test-vectors/exec-remote-v1")
    })
}

fn vector_cases(directory: &str) -> Vec<(String, serde_json::Value)> {
    use sha2::{Digest, Sha256};
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(vectors().join(directory)).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "jcs") {
            let bytes = std::fs::read(&path).unwrap();
            let hash = std::fs::read_to_string(path.with_extension("sha256")).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                hash.trim(),
                "{}",
                path.display()
            );
            cases.push((
                path.file_stem().unwrap().to_str().unwrap().to_string(),
                serde_json::from_slice(&bytes).unwrap(),
            ));
        }
    }
    cases.sort_by(|a, b| a.0.cmp(&b.0));
    cases
}

#[test]
fn published_outcomes_have_explicit_grades() {
    let cases = vector_cases("outcomes");
    assert_eq!(cases.len(), 22);
    for (name, value) in cases {
        let _: RunRequest = serde_json::from_value(value["request"].clone()).unwrap();
        let records: Vec<StreamRecord> = serde_json::from_value(value["stream"].clone()).unwrap();
        let StreamRecord::Terminal(terminal) = records.last().unwrap() else {
            panic!("{name}")
        };
        let expected = match name.as_str() {
            "exit" | "pipestatus" => Verdict::Exited { code: 0 },
            "exit-nonzero" => Verdict::Exited { code: 100 },
            "signal" => Verdict::Signalled { signal: 15 },
            "cancelled" => Verdict::Cancelled,
            "killed-deadline" => Verdict::DeadlineKilled,
            "killed-cancel" => Verdict::CancelKilled,
            "outcome_unknown" | "outcome_unknown-queued" | "crate-local-unknown-outcome" => {
                Verdict::OutcomeUnknown
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
        let mut consumer = StreamConsumer::new();
        let mut sink = MemorySink::default();
        for record in records {
            consumer.consume(record, &mut sink).unwrap();
        }
        assert_eq!(consumer.finish().unwrap(), expected, "stream grade: {name}");
        assert_eq!(sink.terminals, [expected]);
    }
}

#[test]
fn control_outcome_unknown_never_reruns() {
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
fn control_history_expired_never_reruns() {
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
struct MemorySink {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    seqs: Vec<u64>,
    terminals: Vec<Verdict>,
    truncations: Vec<u64>,
}

impl OutputSink for MemorySink {
    fn unknown_output(&mut self, seq: u64, _bytes: &[u8]) -> std::io::Result<()> {
        self.seqs.push(seq);
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
fn control_split_utf8_is_reassembled_as_exact_bytes() {
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
fn control_seq_dedupe_keeps_first_record_in_sorted_order() {
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
    assert_eq!(cases.len(), 14);
    for (name, value) in cases {
        let (operation, expected) = match name.as_str() {
            "cancel" => ("exec.cancel", ReplyVerdict::CancelAcknowledged),
            "drop-existing" => ("workspace.drop", ReplyVerdict::Dropped { existed: true }),
            "drop-missing" => ("workspace.drop", ReplyVerdict::Dropped { existed: false }),
            "prepare-prepared" => ("workspace.prepare", ReplyVerdict::Prepared),
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
        ("SHELL_SECRET".into(), "executor filters this".into()),
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
    assert_eq!(request.env, env);
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
