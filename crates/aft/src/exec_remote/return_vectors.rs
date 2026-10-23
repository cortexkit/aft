//! The published chunked-return vectors from `cortexkit-exec-remote-types`
//! 0.2.10, vendored byte for byte under `fixtures/return-chunked/` (see
//! `fixtures/SOURCE.md`). Each case is checked against its published digest
//! before it is decoded.

use super::types::StreamRecord;
use serde_json::Value;

macro_rules! vectors {
    ($($name:literal),* $(,)?) => {
        /// Every case: name, `.jcs` bytes, `.sha256` text.
        pub(crate) const RETURN_VECTORS: &[(&str, &[u8], &str)] = &[$(
            (
                $name,
                include_bytes!(concat!("fixtures/return-chunked/", $name, ".jcs")),
                include_str!(concat!("fixtures/return-chunked/", $name, ".sha256")),
            ),
        )*];
    };
}

vectors!(
    "two-files-one-empty",
    "three-chunks-short-last",
    "failed-missing",
    "failed-not-regular",
    "failed-escape",
    "failed-over-cap",
    "read-error-after-chunks",
    "replay-retained",
    "replay-expired",
    "unknown-cause-discards-chunks",
    "refuse-offset-gap",
    "refuse-offset-overlap",
    "refuse-interleaved-paths",
    "refuse-chunk-too-large",
    "refuse-empty-chunk",
    "refuse-path-index-out-of-range",
    "refuse-digest-mismatch",
    "refuse-size-mismatch",
    "refuse-path-not-echoed",
    "refuse-chunks-before-over-cap",
    "refuse-total-over-cap",
);

/// One case, after checking its bytes against the published digest.
pub(crate) fn return_vector(name: &str) -> Value {
    use sha2::{Digest, Sha256};
    let (_, bytes, digest) = RETURN_VECTORS
        .iter()
        .find(|(case, _, _)| *case == name)
        .unwrap_or_else(|| panic!("no return-chunked vector {name}"));
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        digest.trim(),
        "{name}.jcs no longer matches its published digest"
    );
    serde_json::from_slice(bytes).unwrap()
}

/// The case's stream records, decoded with the locked crate.
pub(crate) fn return_vector_records(name: &str) -> Vec<StreamRecord> {
    serde_json::from_value(return_vector(name)["records"].clone()).unwrap()
}

/// The case's requested paths.
// Only the Unix returned-file receiver tests read them.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn return_vector_paths(name: &str) -> Vec<String> {
    serde_json::from_value(return_vector(name)["return_paths"].clone()).unwrap()
}

#[test]
fn every_vendored_return_vector_matches_its_digest_and_round_trips() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/exec_remote/fixtures/return-chunked");
    let files = std::fs::read_dir(directory).unwrap().count();
    assert_eq!(
        files,
        RETURN_VECTORS.len() * 2,
        "unchecked vendored vector files"
    );
    for (name, bytes, _) in RETURN_VECTORS {
        let vector = return_vector(name);
        for record in vector["records"].as_array().unwrap() {
            let decoded: StreamRecord = serde_json::from_value(record.clone()).unwrap();
            assert_eq!(
                &serde_json::to_value(&decoded).unwrap(),
                record,
                "{name}: a record changed in a decode and encode round trip"
            );
        }
        assert!(!bytes.is_empty());
    }
}

/// Records what the stream consumer hands a sink, by seq.
#[derive(Default)]
struct ReturnSink {
    chunks: Vec<u64>,
    manifests: Vec<u64>,
    lost: Vec<u64>,
    gaps: usize,
    verdicts: Vec<super::Verdict>,
}

impl super::OutputSink for ReturnSink {
    fn accepted(&mut self, _: &super::types::Accepted) -> std::io::Result<()> {
        Ok(())
    }
    fn output(&mut self, _: u64, _: super::types::OutputStream, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn truncated(&mut self, before_seq: u64) -> std::io::Result<()> {
        self.lost.push(before_seq);
        Ok(())
    }
    fn gap_recovery(&mut self, _: &super::GapRecovery) -> std::io::Result<()> {
        self.gaps += 1;
        Ok(())
    }
    fn unknown_output(&mut self, _: u64, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn return_chunk(&mut self, chunk: &super::types::ReturnChunk) -> std::io::Result<()> {
        self.chunks.push(chunk.seq);
        Ok(())
    }
    fn return_manifest(&mut self, manifest: &super::types::ReturnManifest) -> std::io::Result<()> {
        self.manifests.push(manifest.seq);
        Ok(())
    }
    fn terminal(
        &mut self,
        _: &super::types::TerminalRecord,
        verdict: &super::Verdict,
    ) -> std::io::Result<()> {
        self.verdicts.push(verdict.clone());
        Ok(())
    }
}

/// Feed a replay vector's records to a consumer that re-attaches after seq 0,
/// the record before the replayed part. An attach replays sequenced records
/// only, so the vector's acceptance is not fed again.
fn consume(records: Vec<StreamRecord>) -> (Result<(), super::Error>, ReturnSink) {
    let mut consumer = super::StreamConsumer::resume(super::ResumePoint {
        job_id: "0192a64a-1234-7000-8000-000000000001".parse().unwrap(),
        last_seq: Some(0),
        gap_recovery: None,
    });
    let mut sink = ReturnSink::default();
    for record in records {
        if matches!(record, StreamRecord::Accepted(_)) {
            continue;
        }
        let bytes = serde_json::to_vec(&record).unwrap();
        if let Err(error) = consumer.consume_bytes(&bytes, &mut sink) {
            return (Err(error), sink);
        }
    }
    (consumer.finish().map(|_| ()), sink)
}

#[test]
fn expired_replay_accepts_the_chunk_gap_before_its_manifest() {
    // Output at seq 2, then the manifest at its original seq 6: the chunks
    // the runner no longer keeps were at seqs 3 to 5.
    let (result, sink) = consume(return_vector_records("replay-expired"));
    result.expect("the expired replay completes without re-attaching");
    assert_eq!(sink.manifests, [6]);
    assert!(sink.chunks.is_empty());
    assert_eq!(sink.gaps, 0, "the chunk gap is not an output gap");
    assert!(sink.lost.is_empty(), "no output is reported lost");
    assert_eq!(sink.verdicts, [super::Verdict::Exited { code: 0 }]);
}

#[test]
fn retained_replay_delivers_chunks_then_manifest_in_seq_order() {
    let (result, sink) = consume(return_vector_records("replay-retained"));
    result.unwrap();
    assert_eq!(sink.chunks, [3, 4, 5]);
    assert_eq!(sink.manifests, [6]);
    assert_eq!(sink.gaps, 0);
}

#[test]
fn missing_chunks_before_a_manifest_that_returns_bytes_need_recovery() {
    // The same retained replay with its chunks lost in transit: those seqs
    // held returned bytes, so the gap is recovered by attaching again, never
    // skipped like an expired replay's.
    let records = return_vector_records("replay-retained")
        .into_iter()
        .filter(|record| !matches!(record, StreamRecord::ReturnChunk(_)))
        .collect();
    let (result, sink) = consume(records);
    assert!(
        matches!(result, Err(super::Error::RecoveryRequired { .. })),
        "{result:?}"
    );
    assert!(sink.manifests.is_empty());
    assert_eq!(sink.gaps, 1);
}

#[test]
fn output_received_inside_the_gap_keeps_ordinary_gap_recovery() {
    // Seq 2 is output that arrived while seq 1 did not: the hole lies before
    // an output record, not directly before the manifest, so it is never
    // skipped as an expired replay's chunk gap.
    use super::types::*;
    let job_id: Uuid = "0192a64a-1234-7000-8000-000000000001".parse().unwrap();
    let output = |seq, bytes: &[u8]| {
        StreamRecord::Output(Output::new(
            seq,
            OutputStream::Stdout,
            BytePayload(bytes.to_vec()),
        ))
    };
    let entry =
        ReturnManifestEntry::failed("dist/app.bin", ReturnPathsFailureCause::Expired).with_size(10);
    let records = vec![
        StreamRecord::Accepted(Accepted::new(job_id, 1)),
        output(0, b"a"),
        output(2, b"c"),
        StreamRecord::ReturnManifest(ReturnManifest::new(
            job_id,
            4,
            vec![entry],
            0,
            RETURN_CHUNKED_MAX_BYTES_CAP,
        )),
        StreamRecord::Terminal(TerminalRecord::new(
            job_id,
            Outcome::Exit { code: 0 },
            1,
            0,
            0,
        )),
    ];
    let mut consumer = super::StreamConsumer::new();
    let mut sink = ReturnSink::default();
    let mut result = Ok(());
    for record in records {
        let bytes = serde_json::to_vec(&record).unwrap();
        result = consumer.consume_bytes(&bytes, &mut sink);
        if result.is_err() {
            break;
        }
    }
    assert!(
        matches!(result, Err(super::Error::RecoveryRequired { .. })),
        "{result:?}"
    );
    assert_eq!(sink.gaps, 1);
    assert!(sink.manifests.is_empty(), "the manifest waits for the hole");
}

#[test]
fn motor_return_paths_unsupported_terminal_is_a_final_refusal_before_start() {
    use super::types::*;
    let record: StreamRecord =
        serde_json::from_str(include_str!("fixtures/motor/return-paths-unsupported.json")).unwrap();
    let StreamRecord::Terminal(terminal) = record else {
        panic!("the pinned record is a terminal")
    };
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms, None);
    assert_eq!(
        super::grade(&terminal),
        super::Verdict::RunLocally {
            reason: RefusalReason::RequestRejected,
            refusal_detail: Some("return_paths_unsupported".into()),
        }
    );
}

/// A live stream: acceptance, output at seq 0, output at seq 1 lost in
/// transit, then a manifest at seq 2 holding `entries` and the terminal.
fn lost_output_before_manifest(
    entries: Vec<super::types::ReturnManifestEntry>,
) -> (Result<(), super::Error>, ReturnSink) {
    use super::types::*;
    let job_id: Uuid = "0192a64a-1234-7000-8000-000000000001".parse().unwrap();
    let records = vec![
        StreamRecord::Accepted(Accepted::new(job_id, 1)),
        StreamRecord::Output(Output::new(
            0,
            OutputStream::Stdout,
            BytePayload(b"a".to_vec()),
        )),
        StreamRecord::ReturnManifest(ReturnManifest::new(
            job_id,
            2,
            entries,
            0,
            RETURN_CHUNKED_MAX_BYTES_CAP,
        )),
        StreamRecord::Terminal(TerminalRecord::new(
            job_id,
            Outcome::Exit { code: 0 },
            1,
            0,
            0,
        )),
    ];
    let mut consumer = super::StreamConsumer::new();
    let mut sink = ReturnSink::default();
    for record in records {
        let bytes = serde_json::to_vec(&record).unwrap();
        if let Err(error) = consumer.consume_bytes(&bytes, &mut sink) {
            return (Err(error), sink);
        }
    }
    (consumer.finish().map(|_| ()), sink)
}

#[test]
fn a_lost_output_record_before_a_manifest_without_expired_files_is_recovered() {
    // No requested file expired, so no chunk can have been dropped: the gap
    // at seq 1 is lost output, and the ordinary gap recovery re-attaches.
    use super::types::*;
    for entries in [
        vec![ReturnManifestEntry::failed(
            "dist/absent.txt",
            ReturnPathsFailureCause::Missing,
        )],
        vec![ReturnManifestEntry::returned(
            "dist/empty.txt",
            0,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )],
    ] {
        let (result, sink) = lost_output_before_manifest(entries);
        assert!(
            matches!(result, Err(super::Error::RecoveryRequired { .. })),
            "{result:?}"
        );
        assert_eq!(sink.gaps, 1);
        assert!(sink.manifests.is_empty(), "the manifest waits for the hole");
    }
}

#[test]
fn a_gap_longer_than_the_expired_files_could_fill_is_recovered() {
    // One expired file of 1 byte can account for one chunk at most, so a
    // gap of two records before the manifest is not all expired chunks.
    use super::types::*;
    let job_id: Uuid = "0192a64a-1234-7000-8000-000000000001".parse().unwrap();
    let mut consumer = super::StreamConsumer::new();
    let mut sink = ReturnSink::default();
    let entry =
        ReturnManifestEntry::failed("dist/app.bin", ReturnPathsFailureCause::Expired).with_size(1);
    let mut result = Ok(());
    for record in [
        StreamRecord::Accepted(Accepted::new(job_id, 1)),
        StreamRecord::Output(Output::new(
            0,
            OutputStream::Stdout,
            BytePayload(b"a".to_vec()),
        )),
        StreamRecord::ReturnManifest(ReturnManifest::new(
            job_id,
            3,
            vec![entry],
            0,
            RETURN_CHUNKED_MAX_BYTES_CAP,
        )),
        StreamRecord::Terminal(TerminalRecord::new(
            job_id,
            Outcome::Exit { code: 0 },
            1,
            0,
            0,
        )),
    ] {
        let bytes = serde_json::to_vec(&record).unwrap();
        result = consumer.consume_bytes(&bytes, &mut sink);
        if result.is_err() {
            break;
        }
    }
    assert!(
        matches!(result, Err(super::Error::RecoveryRequired { .. })),
        "{result:?}"
    );
    assert!(sink.manifests.is_empty());
}

#[test]
fn a_lost_output_record_that_never_comes_back_is_reported_lost() {
    // Every re-attach replays the same records without seq 1. After the
    // usual re-attaches the output record is reported lost, then the
    // manifest and terminal are delivered.
    use super::types::*;
    let job_id: Uuid = "0192a64a-1234-7000-8000-000000000001".parse().unwrap();
    let entry = ReturnManifestEntry::failed("dist/absent.txt", ReturnPathsFailureCause::Missing);
    let replayed = vec![
        StreamRecord::Output(Output::new(
            0,
            OutputStream::Stdout,
            BytePayload(b"a".to_vec()),
        )),
        StreamRecord::ReturnManifest(ReturnManifest::new(
            job_id,
            2,
            vec![entry],
            0,
            RETURN_CHUNKED_MAX_BYTES_CAP,
        )),
        StreamRecord::Terminal(TerminalRecord::new(
            job_id,
            Outcome::Exit { code: 0 },
            1,
            0,
            0,
        )),
    ];
    let mut sink = ReturnSink::default();
    let mut consumer = super::StreamConsumer::new();
    let accepted = serde_json::to_vec(&StreamRecord::Accepted(Accepted::new(job_id, 1))).unwrap();
    consumer.consume_bytes(&accepted, &mut sink).unwrap();
    for attempt in 0..10 {
        let mut result = Ok(());
        for record in &replayed {
            result = consumer.consume_bytes(&serde_json::to_vec(record).unwrap(), &mut sink);
            if result.is_err() {
                break;
            }
        }
        match result {
            Ok(()) => break,
            Err(super::Error::RecoveryRequired { resume, .. }) => {
                assert!(attempt < 9, "the gap must end in reported loss");
                consumer = super::StreamConsumer::resume(resume.unwrap());
            }
            Err(error) => panic!("{error}"),
        }
    }
    consumer.finish().unwrap();
    assert_eq!(sink.lost, [2], "seq 1 is reported as never delivered");
    assert_eq!(sink.manifests, [2]);
    assert_eq!(sink.verdicts, [super::Verdict::Exited { code: 0 }]);
}
