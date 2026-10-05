use super::*;
use std::path::PathBuf;
use std::sync::OnceLock;
use types::*;

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
            "exit-nonzero" => Verdict::Exited { code: 101 },
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
