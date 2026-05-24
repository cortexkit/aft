use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

use aft::semantic_index::{SemanticIndex, SemanticIndexFingerprint};
use log::{Level, LevelFilter, Log, Metadata, Record};

static TEST_LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static LOGGER_INIT: Once = Once::new();

struct TestLogger;

impl Log for TestLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Warn
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            TEST_LOGS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .expect("lock test logs")
                .push(format!("{}", record.args()));
        }
    }

    fn flush(&self) {}
}

fn init_test_logger() {
    LOGGER_INIT.call_once(|| {
        log::set_boxed_logger(Box::new(TestLogger)).expect("install test logger");
        log::set_max_level(LevelFilter::Warn);
    });
    clear_logs();
}

fn clear_logs() {
    TEST_LOGS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("lock test logs")
        .clear();
}

fn take_logs() -> Vec<String> {
    std::mem::take(
        &mut *TEST_LOGS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("lock test logs"),
    )
}

fn build_test_index(project_root: &Path) -> (SemanticIndex, PathBuf) {
    let source_file = project_root.join("src/lib.rs");
    fs::create_dir_all(source_file.parent().expect("source parent")).expect("create src dir");
    fs::write(
        &source_file,
        "pub fn handle_request(token: &str) -> bool {\n    !token.is_empty()\n}\n\npub fn normalize_user_id(input: &str) -> String {\n    input.trim().to_lowercase()\n}\n",
    )
    .expect("write source file");

    let files = vec![source_file.clone()];
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .map(|text| {
                    if text.contains("handle_request") {
                        vec![1.0, 0.0, 0.0, 0.0]
                    } else if text.contains("normalize_user_id") {
                        vec![0.0, 1.0, 0.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0, 0.0]
                    }
                })
                .collect(),
        )
    };

    let index = SemanticIndex::build(project_root, &files, &mut embed, 16)
        .expect("build semantic index with stub embeddings");

    (index, source_file)
}

fn push_string(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    buf.extend_from_slice(value.as_bytes());
}

fn build_v1_index_bytes(file: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    let file_str = file.to_string_lossy();

    bytes.push(1u8);
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());

    bytes.extend_from_slice(&1u32.to_le_bytes());
    push_string(&mut bytes, &file_str);
    bytes.extend_from_slice(&0u64.to_le_bytes());

    push_string(&mut bytes, &file_str);
    push_string(&mut bytes, "legacy_symbol");
    bytes.push(0u8);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.push(1u8);
    push_string(&mut bytes, "fn legacy_symbol() {}");
    push_string(
        &mut bytes,
        "file:src/lib.rs kind:function name:legacy_symbol",
    );
    for value in [0.1f32, 0.2, 0.3] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    bytes
}

#[test]
fn write_and_read_roundtrip_preserves_semantic_entries() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let (index, source_file) = build_test_index(project.path());

    index.write_to_disk(storage.path(), "roundtrip-project");

    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        "roundtrip-project",
        project.path(),
        false,
        None,
    )
    .expect("restore semantic index from disk");

    assert_eq!(restored.len(), index.len());
    assert_eq!(restored.dimension(), index.dimension());

    let request_results = restored.search(&[1.0, 0.0, 0.0, 0.0], 2);
    assert!(!request_results.is_empty());
    assert_eq!(request_results[0].name, "handle_request");
    assert_eq!(request_results[0].file, source_file);
    assert!(request_results[0].snippet.contains("handle_request"));

    let normalize_results = restored.search(&[0.0, 1.0, 0.0, 0.0], 2);
    assert!(!normalize_results.is_empty());
    assert_eq!(normalize_results[0].name, "normalize_user_id");
}

#[test]
fn read_from_nonexistent_path_returns_none() {
    let storage = tempfile::tempdir().expect("create storage dir");

    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        "missing-project",
        storage.path(),
        false,
        None,
    );

    assert!(restored.is_none());
}

#[test]
fn read_from_corrupt_file_returns_none_and_logs_warning() {
    init_test_logger();

    let storage = tempfile::tempdir().expect("create storage dir");
    let semantic_dir = storage.path().join("semantic").join("corrupt-project");
    fs::create_dir_all(&semantic_dir).expect("create semantic dir");
    let semantic_file = semantic_dir.join("semantic.bin");
    fs::write(&semantic_file, b"corrupt").expect("write corrupt semantic file");

    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        "corrupt-project",
        storage.path(),
        false,
        None,
    );

    assert!(restored.is_none());
    assert!(
        !semantic_file.exists(),
        "corrupt semantic file should be removed after read failure"
    );

    let logs = take_logs();
    assert!(
        logs.iter()
            .any(|line| line.contains("corrupt semantic index")),
        "expected corrupt-index warning, got {logs:?}"
    );
}

#[test]
fn semantic_cache_inconsistent_lengths_rebuilds() {
    init_test_logger();

    let storage = tempfile::tempdir().expect("create storage dir");
    let semantic_dir = storage.path().join("semantic").join("drift-project");
    fs::create_dir_all(&semantic_dir).expect("create semantic dir");
    let semantic_file = semantic_dir.join("semantic.bin");
    let source = storage.path().join("src/lib.rs");

    let mut bytes = Vec::new();
    bytes.push(6u8);
    bytes.extend_from_slice(&1u32.to_le_bytes()); // dimension
    bytes.extend_from_slice(&1u32.to_le_bytes()); // one entry
    bytes.extend_from_slice(&0u32.to_le_bytes()); // no fingerprint
    bytes.extend_from_slice(&0u32.to_le_bytes()); // zero file metadata rows
    push_string(&mut bytes, &source.to_string_lossy());
    push_string(&mut bytes, "drift_symbol");
    bytes.push(0u8);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(1u8);
    push_string(&mut bytes, "fn drift_symbol() {}");
    push_string(
        &mut bytes,
        "file:src/lib.rs kind:function name:drift_symbol",
    );
    bytes.extend_from_slice(&1.0f32.to_le_bytes());
    fs::write(&semantic_file, bytes).expect("write inconsistent semantic cache");

    assert!(SemanticIndex::read_from_disk(
        storage.path(),
        "drift-project",
        storage.path(),
        false,
        None
    )
    .is_none());
    assert!(
        !semantic_file.exists(),
        "bad semantic cache should be removed"
    );
}

#[test]
fn stale_file_detected_after_deletion() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let (index, source_file) = build_test_index(project.path());

    index.write_to_disk(storage.path(), "stale-project");
    fs::remove_file(&source_file).expect("remove indexed source file");

    let restored =
        SemanticIndex::read_from_disk(storage.path(), "stale-project", project.path(), false, None)
            .expect("restore semantic index from disk");

    // After deletion, the single indexed file must be stale.
    assert!(
        restored.is_file_stale(&source_file),
        "deleted file should be detected as stale"
    );
}

#[test]
fn read_from_disk_rebuilds_v1_cache_when_fingerprint_is_expected() {
    let storage = tempfile::tempdir().expect("create storage dir");
    let legacy_file = storage.path().join("src/lib.rs");
    fs::create_dir_all(legacy_file.parent().expect("legacy parent")).expect("create src dir");
    fs::write(&legacy_file, "pub fn legacy_symbol() {}\n").expect("write legacy source file");

    let v1_bytes = build_v1_index_bytes(&legacy_file);
    let restored = SemanticIndex::from_bytes(&v1_bytes, storage.path())
        .expect("parse v1 semantic index bytes");
    assert!(restored.fingerprint().is_none());

    let semantic_dir = storage.path().join("semantic").join("v1-project");
    fs::create_dir_all(&semantic_dir).expect("create semantic dir");
    let semantic_file = semantic_dir.join("semantic.bin");
    fs::write(&semantic_file, &v1_bytes).expect("write v1 semantic index file");

    let expected_fingerprint = SemanticIndexFingerprint {
        backend: "fastembed".to_string(),
        model: "all-MiniLM-L6-v2".to_string(),
        base_url: "none".to_string(),
        dimension: 3,
        chunking_version: 2,
    }
    .as_string();

    assert!(SemanticIndex::read_from_disk(
        storage.path(),
        "v1-project",
        storage.path(),
        false,
        Some(&expected_fingerprint)
    )
    .is_none());
    assert!(
        !semantic_file.exists(),
        "legacy semantic cache should be deleted after fingerprint mismatch"
    );
}

/// Regression: v0.15.2 — semantic index mtime precision.
///
/// Before v0.15.2, the on-disk format stored file mtimes as whole seconds
/// (`Duration::as_secs()`), while live mtimes from `fs::metadata().modified()`
/// carry subsecond precision on macOS APFS, ext4 with nsec, and NTFS. The
/// equality comparison in `is_file_stale()` therefore reported every file as
/// stale on every restart, triggering a ~500-file fastembed rebuild at
/// ~800% CPU for 30-50s on every opencode restart.
///
/// This test asserts the round-trip preserves subsecond mtimes and the
/// staleness check survives it.
#[test]
fn write_roundtrip_preserves_subsecond_mtime_precision() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let (index, source_file) = build_test_index(project.path());

    // Sanity: the live file must actually have subsecond mtime for this
    // test to be meaningful (CI filesystems like tmpfs can lose nanos;
    // APFS/ext4 with nsec/NTFS do not).
    let live_mtime = fs::metadata(&source_file)
        .expect("stat source file")
        .modified()
        .expect("read live mtime");
    let live_nanos = live_mtime
        .duration_since(std::time::UNIX_EPOCH)
        .expect("mtime >= epoch")
        .subsec_nanos();
    if live_nanos == 0 {
        eprintln!(
            "skipping subsecond roundtrip assertion: filesystem does not report subsecond mtime \
             (live nanos == 0). Test still validates staleness on whole-second mtimes."
        );
    }

    index.write_to_disk(storage.path(), "subsec-project");

    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        "subsec-project",
        project.path(),
        false,
        None,
    )
    .expect("restore semantic index from disk");

    // The source file has not been touched since index construction, so
    // after round-trip it MUST NOT be flagged as stale. This is the
    // actual regression: pre-v0.15.2, this assertion failed on any
    // filesystem with subsecond mtime precision.
    assert!(
        !restored.is_file_stale(&source_file),
        "unchanged file flagged stale after disk round-trip — mtime precision lost"
    );
    assert!(
        !restored.is_file_stale(&source_file),
        "no file should be stale after a fresh round-trip"
    );
}

/// Migration: V2 caches must be discarded on disk so persisted snippets are
/// rebuilt with V4 range handling. `from_bytes` still parses V2 for low-level
/// compatibility, but `read_from_disk` rejects old cache files before serving
/// stale embeddings.
#[test]
fn read_from_disk_rebuilds_v2_cache_for_v4_snippets() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let (index, source_file) = build_test_index(project.path());

    // Construct a V2 blob by hand (matches pre-v0.15.2 serialisation).
    let fingerprint = SemanticIndexFingerprint {
        backend: "fastembed".to_string(),
        model: "all-MiniLM-L6-v2".to_string(),
        base_url: "none".to_string(),
        dimension: 4,
        chunking_version: 2,
    };
    let fp_str = fingerprint.as_string();
    let fp_bytes = fp_str.as_bytes();

    let mut bytes = Vec::new();
    bytes.push(2u8); // V2
    bytes.extend_from_slice(&4u32.to_le_bytes()); // dimension
    bytes.extend_from_slice(&(index.len() as u32).to_le_bytes()); // entry_count
    bytes.extend_from_slice(&(fp_bytes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(fp_bytes);

    // Mtime table — 1 entry, whole seconds only (V2 layout).
    bytes.extend_from_slice(&1u32.to_le_bytes());
    push_string(&mut bytes, &source_file.to_string_lossy());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // secs=0, no nanos field

    // Reuse V3-written entries from the real index — the entry layout
    // is identical across V1/V2/V3.
    let v3_bytes = index.to_bytes();
    // Skip V3 header to find where its mtime table ends and entries begin.
    // Simpler: just append a single hand-rolled entry for one symbol.
    push_string(&mut bytes, &source_file.to_string_lossy());
    push_string(&mut bytes, "legacy_sym");
    bytes.push(0u8); // SymbolKind::Function
    bytes.extend_from_slice(&1u32.to_le_bytes()); // start_line
    bytes.extend_from_slice(&3u32.to_le_bytes()); // end_line
    bytes.push(1u8); // exported
    push_string(&mut bytes, "fn legacy_sym() {}");
    push_string(&mut bytes, "file:src kind:function name:legacy_sym");
    for value in [0.1f32, 0.2, 0.3, 0.4] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    // Overwrite entry count to 1 since we only wrote one entry.
    bytes[5..9].copy_from_slice(&1u32.to_le_bytes());
    let _ = v3_bytes; // keep the binding quiet for clippy

    let semantic_dir = storage.path().join("semantic").join("v2-project");
    fs::create_dir_all(&semantic_dir).expect("create semantic dir");
    fs::write(semantic_dir.join("semantic.bin"), &bytes).expect("write v2 cache");

    assert!(SemanticIndex::read_from_disk(
        storage.path(),
        "v2-project",
        project.path(),
        false,
        Some(&fp_str)
    )
    .is_none());
    assert!(!semantic_dir.join("semantic.bin").exists());
}

/// Hardening: corrupt / malicious V3 caches must be rejected cleanly,
/// not crash the aft process.
///
/// Pre-v0.15.2 hardening, `Duration::new(secs, nanos)` could panic if the
/// nanosecond carry overflowed `secs`, and `SystemTime + Duration` could
/// panic on carry past the platform's upper bound. A corrupted semantic.bin
/// on disk (bit-flip, truncated download, hostile extension) could therefore
/// kill every tool call until the user manually deleted the cache.
///
/// v0.15.2 adds explicit validation:
///   - nanos >= 1_000_000_000 → Err("invalid semantic mtime: nanos ...")
///   - secs/nanos combo overflows SystemTime → Err(".. overflows SystemTime")
///
/// Both surfaces are covered here via `from_bytes` (bypasses the on-disk
/// rename dance, lets us hand-roll corrupt payloads).
#[test]
fn from_bytes_rejects_corrupt_v3_cache_payloads() {
    // Shared helper: build a V3 blob with a single mtime entry using
    // the supplied secs/nanos, then no vector entries (entry_count=0).
    fn build_v3_with_mtime(secs: u64, nanos: u32) -> Vec<u8> {
        let fingerprint = SemanticIndexFingerprint {
            backend: "fastembed".to_string(),
            model: "all-MiniLM-L6-v2".to_string(),
            base_url: "none".to_string(),
            dimension: 4,
            chunking_version: 2,
        };
        let fp_bytes = fingerprint.as_string().into_bytes();
        let mut bytes = Vec::new();
        bytes.push(3u8); // V3
        bytes.extend_from_slice(&4u32.to_le_bytes()); // dimension
        bytes.extend_from_slice(&0u32.to_le_bytes()); // entry_count
        bytes.extend_from_slice(&(fp_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&fp_bytes);
        // Mtime table: 1 entry
        bytes.extend_from_slice(&1u32.to_le_bytes());
        push_string(&mut bytes, "/tmp/corrupt.rs");
        bytes.extend_from_slice(&secs.to_le_bytes());
        bytes.extend_from_slice(&nanos.to_le_bytes());
        bytes
    }

    // Case 1: nanos >= 1e9 → reject with a specific message.
    let bad_nanos = build_v3_with_mtime(0, 2_000_000_000);
    let err = SemanticIndex::from_bytes(&bad_nanos, Path::new("/"))
        .expect_err("V3 with nanos >= 1e9 must be rejected");
    assert!(
        err.contains("nanos") && err.contains("1_000_000_000"),
        "nanos-overflow error should explain the rejection: {err}"
    );

    // Case 2: secs close to u64::MAX → SystemTime overflow rejected without
    // panicking. We pick secs = u64::MAX so adding any Duration carries past
    // the platform's representable range on every target.
    let overflow = build_v3_with_mtime(u64::MAX, 0);
    let err = SemanticIndex::from_bytes(&overflow, Path::new("/"))
        .expect_err("V3 with secs=u64::MAX must be rejected");
    assert!(
        err.contains("overflows SystemTime"),
        "SystemTime-overflow error should explain the rejection: {err}"
    );

    // Case 3: valid V3 payload with nanos = 999_999_999 (max valid) loads
    // cleanly — proves the boundary is strictly < 1e9, not <=.
    let boundary = build_v3_with_mtime(1_700_000_000, 999_999_999);
    let _ = SemanticIndex::from_bytes(&boundary, Path::new("/"))
        .expect("V3 with nanos=999_999_999 must load cleanly");
}
