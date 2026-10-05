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
