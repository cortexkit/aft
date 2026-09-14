use std::fs;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use aft::cache_freshness::{self, FreshnessVerdict};
use aft::semantic_index::{SemanticIndex, SemanticIndexFingerprint};

// Warn-level log capture is shared across all integration test modules via a
// single process-global, thread-local-capturing logger. See test_helpers.
// `init_test_logger()` also clears the current thread's buffer, so the old
// local `clear_logs()` is no longer needed.
use crate::test_helpers::{init_test_logger, take_logs};

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

#[cfg(target_os = "macos")]
fn darwin_write_usage() -> (u64, u64) {
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }

    let mut buffer = [0_u8; 512];
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            4,
            buffer.as_mut_ptr().cast(),
        )
    };
    assert_eq!(result, 0, "Darwin write accounting is required");
    let counter = |field: usize| {
        u64::from_ne_bytes(
            buffer[16 + field * 8..16 + (field + 1) * 8]
                .try_into()
                .unwrap(),
        )
    };
    (counter(17), counter(27))
}

/// Offline probe for the semantic persistence before/after tables in the disk-write hunt.
/// The input must be a copied production semantic.bin; the probe never opens its live source.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "offline probe copies a large production semantic artifact"]
fn bench_semantic_refresh_persistence_writes() {
    let input = PathBuf::from(
        std::env::var_os("AFT_SEMANTIC_DELTA_INPUT")
            .expect("set AFT_SEMANTIC_DELTA_INPUT to a copied semantic.bin"),
    );
    let artifact = fs::read(&input).expect("read copied semantic artifact");

    for changed_files in [1_usize, 10, 100] {
        let temp = tempfile::tempdir_in(input.parent().expect("artifact parent"))
            .expect("create measurement directory beside copied artifact");
        let root = temp.path().join("project");
        fs::create_dir(&root).expect("create rerooted project");
        let root = root.canonicalize().expect("canonicalize rerooted project");
        let storage = temp.path().join("storage");
        let semantic_dir = storage.join("semantic").join("measured");
        fs::create_dir_all(&semantic_dir).expect("create copied artifact directory");
        let data_path = semantic_dir.join("semantic.bin");
        fs::write(&data_path, &artifact).expect("copy semantic artifact");
        fs::File::open(&data_path)
            .expect("open copied artifact")
            .sync_all()
            .expect("sync copied artifact");

        let mut index = SemanticIndex::read_from_disk(&storage, "measured", &root, false, None)
            .expect("load copied semantic artifact");
        let dimension = index.dimension();
        let mut selected = Vec::with_capacity(changed_files);
        let mut seen = HashSet::new();
        for result in index.search(&vec![0.0; dimension], index.len()) {
            if result
                .file
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("rs")
                && seen.insert(result.file.clone())
            {
                selected.push(result.file);
                if selected.len() == changed_files {
                    break;
                }
            }
        }
        assert_eq!(
            selected.len(),
            changed_files,
            "copied artifact needs enough Rust files"
        );
        for (ordinal, path) in selected.iter().enumerate() {
            fs::create_dir_all(path.parent().expect("selected file parent"))
                .expect("create selected file parent");
            fs::write(
                path,
                format!("pub fn measured_refresh_{ordinal}() -> usize {{ {ordinal} }}\n"),
            )
            .expect("write changed source");
        }
        let mut embed = |texts: Vec<String>| {
            Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|_| vec![1.0; dimension]).collect())
        };
        let mut progress = |_done: usize, _total: usize| {};
        let update = index
            .refresh_invalidated_files(&root, &selected, &mut embed, 64, usize::MAX, &mut progress)
            .expect("refresh selected files");
        assert_eq!(
            update.summary.changed, changed_files,
            "every selected production-artifact file should be replaced"
        );

        let before = darwin_write_usage();
        let started = Instant::now();
        assert!(index.write_to_disk(&storage, "measured"));
        let after = darwin_write_usage();
        eprintln!(
            "semantic-delta-measure changed_files={changed_files} elapsed_ms={} physical_bytes={} logical_bytes={} artifact_bytes={}",
            started.elapsed().as_millis(),
            after.0.saturating_sub(before.0),
            after.1.saturating_sub(before.1),
            fs::metadata(&data_path).expect("measure artifact").len(),
        );

        if changed_files == 100 {
            let before = darwin_write_usage();
            let started = Instant::now();
            assert!(index.compact_to_disk_for_test(&storage, "measured"));
            let after = darwin_write_usage();
            eprintln!(
                "semantic-delta-compaction elapsed_ms={} physical_bytes={} logical_bytes={} artifact_bytes={}",
                started.elapsed().as_millis(),
                after.0.saturating_sub(before.0),
                after.1.saturating_sub(before.1),
                fs::metadata(&data_path).expect("measure compacted artifact").len(),
            );
        }
    }
}

const DELTA_PROJECT_KEY: &str = "delta-project";

fn delta_fingerprint() -> SemanticIndexFingerprint {
    SemanticIndexFingerprint {
        backend: "test".to_string(),
        model: "deterministic".to_string(),
        base_url: "none".to_string(),
        dimension: 4,
        chunking_version: 7,
        ..Default::default()
    }
}

fn write_delta_fixture(path: &Path, ordinal: usize, version: usize) {
    fs::create_dir_all(path.parent().expect("delta fixture parent"))
        .expect("create delta fixture parent");
    fs::write(
        path,
        format!(
            "pub fn symbol_{ordinal}_v{version}() -> usize {{\n    {}\n}}\n",
            ordinal + version
        ),
    )
    .expect("write delta fixture");
}

fn delta_vector(text: &str) -> Vec<f32> {
    let hash = blake3::hash(text.as_bytes());
    hash.as_bytes()[..4]
        .iter()
        .map(|byte| (*byte as f32 + 1.0) / 256.0)
        .collect()
}

fn build_delta_index(root: &Path, files: &[PathBuf]) -> SemanticIndex {
    let existing = files
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(texts.iter().map(|text| delta_vector(text)).collect())
    };
    let mut index = SemanticIndex::build(root, &existing, &mut embed, 64)
        .expect("build deterministic semantic index");
    index.set_fingerprint(delta_fingerprint());
    index
}

fn refresh_delta_paths(index: &mut SemanticIndex, root: &Path, paths: &[PathBuf]) {
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(texts.iter().map(|text| delta_vector(text)).collect())
    };
    let mut progress = |_done: usize, _total: usize| {};
    index
        .refresh_invalidated_files(root, paths, &mut embed, 64, usize::MAX, &mut progress)
        .expect("refresh deterministic semantic paths");
}

fn load_delta_index(storage: &Path, root: &Path) -> SemanticIndex {
    SemanticIndex::read_from_disk(
        storage,
        DELTA_PROJECT_KEY,
        root,
        false,
        Some(&delta_fingerprint().as_string()),
    )
    .expect("load semantic base plus segments")
}

fn assert_delta_structural_parity(
    storage: &Path,
    root: &Path,
    files: &[PathBuf],
    expected_live: &SemanticIndex,
) {
    let loaded = load_delta_index(storage, root);
    let whole_rewrite = build_delta_index(root, files);
    assert_eq!(
        loaded.to_bytes(),
        expected_live.to_bytes(),
        "base plus ordered segments must equal the live refreshed index"
    );
    assert_eq!(
        loaded.to_bytes(),
        whole_rewrite.to_bytes(),
        "base plus ordered segments must equal a whole-rewrite snapshot"
    );
}

fn seed_delta_fixture(root: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|ordinal| {
            let path = root.join(format!("src/file_{ordinal:03}.rs"));
            write_delta_fixture(&path, ordinal, 0);
            path
        })
        .collect()
}

#[test]
fn semantic_delta_sequence_matches_whole_rewrite_after_every_step() {
    let project = tempfile::tempdir().expect("create delta project");
    let storage = tempfile::tempdir().expect("create delta storage");
    let files = seed_delta_fixture(project.path(), 130);
    let mut live = build_delta_index(project.path(), &files);

    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    assert_delta_structural_parity(storage.path(), project.path(), &files, &live);

    write_delta_fixture(&files[0], 0, 1);
    refresh_delta_paths(&mut live, project.path(), &files[..1]);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    assert_delta_structural_parity(storage.path(), project.path(), &files, &live);

    for (ordinal, path) in files[1..11].iter().enumerate() {
        write_delta_fixture(path, ordinal + 1, 1);
    }
    refresh_delta_paths(&mut live, project.path(), &files[1..11]);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    assert_delta_structural_parity(storage.path(), project.path(), &files, &live);

    for path in &files[11..14] {
        fs::remove_file(path).expect("delete indexed delta fixture");
    }
    refresh_delta_paths(&mut live, project.path(), &files[11..14]);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    assert_delta_structural_parity(storage.path(), project.path(), &files, &live);

    for (ordinal, path) in files[20..120].iter().enumerate() {
        write_delta_fixture(path, ordinal + 20, 2);
    }
    refresh_delta_paths(&mut live, project.path(), &files[20..120]);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    assert_delta_structural_parity(storage.path(), project.path(), &files, &live);

    assert!(live.compact_to_disk_for_test(storage.path(), DELTA_PROJECT_KEY));
    assert_eq!(
        SemanticIndex::persistence_stats_for_test(
            storage.path(),
            DELTA_PROJECT_KEY,
            project.path()
        ),
        Some((live.to_bytes().len(), 0, 0)),
        "compaction should fold all segments into one canonical base"
    );
    assert_delta_structural_parity(storage.path(), project.path(), &files, &live);
}

#[test]
fn semantic_segment_tombstone_masks_deleted_file() {
    let project = tempfile::tempdir().expect("create tombstone project");
    let storage = tempfile::tempdir().expect("create tombstone storage");
    let files = seed_delta_fixture(project.path(), 20);
    let deleted = files[3].clone();
    let mut live = build_delta_index(project.path(), &files);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));

    fs::remove_file(&deleted).expect("delete semantic fixture");
    refresh_delta_paths(&mut live, project.path(), std::slice::from_ref(&deleted));
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));

    let loaded = load_delta_index(storage.path(), project.path());
    let results = loaded.search(&[1.0, 0.0, 0.0, 0.0], loaded.len());
    assert!(
        results.iter().all(|result| result.file != deleted),
        "a deleted file's base chunks must stay masked by its segment tombstone"
    );
    assert_eq!(loaded.to_bytes(), live.to_bytes());
}

#[test]
fn semantic_segments_apply_in_refresh_order() {
    let project = tempfile::tempdir().expect("create ordered segment project");
    let storage = tempfile::tempdir().expect("create ordered segment storage");
    let files = seed_delta_fixture(project.path(), 20);
    let changed = files[0].clone();
    let mut live = build_delta_index(project.path(), &files);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));

    for version in 1..=2 {
        write_delta_fixture(&changed, 0, version);
        refresh_delta_paths(&mut live, project.path(), std::slice::from_ref(&changed));
        assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    }

    assert_eq!(
        SemanticIndex::persistence_stats_for_test(
            storage.path(),
            DELTA_PROJECT_KEY,
            project.path()
        )
        .map(|(_, segments, _)| segments),
        Some(2)
    );
    let loaded = load_delta_index(storage.path(), project.path());
    let names = loaded
        .search(&[1.0, 0.0, 0.0, 0.0], loaded.len())
        .into_iter()
        .filter(|result| result.file.file_name() == changed.file_name())
        .map(|result| result.name)
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name == "symbol_0_v2"),
        "latest segment should win for the changed file: {names:?}"
    );
    assert!(!names.iter().any(|name| name == "symbol_0_v1"));
    assert_eq!(loaded.to_bytes(), live.to_bytes());
}

#[test]
fn semantic_compaction_folds_superseded_chunks_once() {
    let project = tempfile::tempdir().expect("create compaction project");
    let storage = tempfile::tempdir().expect("create compaction storage");
    let files = seed_delta_fixture(project.path(), 20);
    let changed = files[0].clone();
    let mut live = build_delta_index(project.path(), &files);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));

    for version in 1..=2 {
        write_delta_fixture(&changed, 0, version);
        refresh_delta_paths(&mut live, project.path(), std::slice::from_ref(&changed));
        assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    }
    let compacted = live.compact_to_disk_for_test(storage.path(), DELTA_PROJECT_KEY);
    let stats = SemanticIndex::persistence_stats_for_test(
        storage.path(),
        DELTA_PROJECT_KEY,
        project.path(),
    );
    assert!(
        compacted || stats.map(|(_, segments, _)| segments) == Some(0),
        "explicit compaction should win unless off-path compaction already did: {stats:?}"
    );

    let loaded = load_delta_index(storage.path(), project.path());
    let names = loaded
        .search(&[1.0, 0.0, 0.0, 0.0], loaded.len())
        .into_iter()
        .filter(|result| result.file.file_name() == changed.file_name())
        .map(|result| result.name)
        .collect::<Vec<_>>();
    assert_eq!(
        names.iter().filter(|name| *name == "symbol_0_v2").count(),
        1,
        "compaction must not duplicate the latest chunk identity"
    );
    assert!(!names.iter().any(|name| name == "symbol_0_v0"));
    assert!(!names.iter().any(|name| name == "symbol_0_v1"));
    assert_eq!(loaded.to_bytes(), live.to_bytes());
}

#[test]
fn semantic_compaction_starts_after_sixty_four_segments() {
    let project = tempfile::tempdir().expect("create bounded compaction project");
    let storage = tempfile::tempdir().expect("create bounded compaction storage");
    let files = seed_delta_fixture(project.path(), 1_000);
    let changed = files[0].clone();
    let mut live = build_delta_index(project.path(), &files);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));

    for version in 1..=64 {
        write_delta_fixture(&changed, 0, version);
        refresh_delta_paths(&mut live, project.path(), std::slice::from_ref(&changed));
        assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    }
    assert_eq!(
        SemanticIndex::persistence_stats_for_test(
            storage.path(),
            DELTA_PROJECT_KEY,
            project.path()
        )
        .map(|(_, segments, _)| segments),
        Some(64),
        "the segment-count bound is exclusive"
    );

    write_delta_fixture(&changed, 0, 65);
    refresh_delta_paths(&mut live, project.path(), std::slice::from_ref(&changed));
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    let started = std::time::Instant::now();
    loop {
        let segment_count = SemanticIndex::persistence_stats_for_test(
            storage.path(),
            DELTA_PROJECT_KEY,
            project.path(),
        )
        .map(|(_, segments, _)| segments);
        if segment_count == Some(0) {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "off-path compaction did not fold the 65th segment: {segment_count:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        load_delta_index(storage.path(), project.path()).to_bytes(),
        live.to_bytes()
    );
}

#[cfg(unix)]
const SEGMENT_TEAR_CHILD_TEST: &str = "semantic_disk_test::semantic_segment_sigkill_child";
#[cfg(unix)]
const COMPACTION_SWAP_CHILD_TEST: &str = "semantic_disk_test::semantic_compaction_swap_child";

#[cfg(unix)]
fn wait_for_test_seam(child: &mut std::process::Child, ready: &Path) {
    let started = std::time::Instant::now();
    while !ready.is_file() {
        if let Some(status) = child.try_wait().expect("poll semantic child") {
            panic!("semantic persistence child exited before test seam: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for semantic persistence child seam"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
#[ignore]
fn semantic_segment_sigkill_child() {
    let root = PathBuf::from(std::env::var_os("AFT_TEST_SEMANTIC_ROOT").expect("child root"));
    let storage =
        PathBuf::from(std::env::var_os("AFT_TEST_SEMANTIC_STORAGE").expect("child storage"));
    let changed =
        PathBuf::from(std::env::var_os("AFT_TEST_SEMANTIC_CHANGED").expect("changed path"));
    let mut index = load_delta_index(&storage, &root);
    write_delta_fixture(&changed, 0, 1);
    refresh_delta_paths(&mut index, &root, std::slice::from_ref(&changed));
    assert!(index.write_to_disk(&storage, DELTA_PROJECT_KEY));
}

#[cfg(unix)]
#[test]
fn semantic_segment_sigkill_preserves_previous_state_and_retry_recovers() {
    use std::os::unix::process::ExitStatusExt;

    let project = tempfile::tempdir().expect("create SIGKILL project");
    let storage = tempfile::tempdir().expect("create SIGKILL storage");
    let files = seed_delta_fixture(project.path(), 20);
    let changed = files[0].clone();
    let mut original = build_delta_index(project.path(), &files);
    assert!(original.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    let ready = storage.path().join("segment-tear.ready");

    let mut child = Command::new(std::env::current_exe().expect("semantic test executable"))
        .args([
            "--exact",
            SEGMENT_TEAR_CHILD_TEST,
            "--ignored",
            "--nocapture",
        ])
        .env("AFT_TEST_SEMANTIC_ROOT", project.path())
        .env("AFT_TEST_SEMANTIC_STORAGE", storage.path())
        .env("AFT_TEST_SEMANTIC_CHANGED", &changed)
        .env("AFT_TEST_SEMANTIC_SEGMENT_TEAR_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn semantic tear child");
    wait_for_test_seam(&mut child, &ready);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    let status = child.wait().expect("reap semantic tear child");
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let torn_bytes = fs::metadata(
        storage
            .path()
            .join("semantic")
            .join(DELTA_PROJECT_KEY)
            .join("semantic.bin"),
    )
    .expect("stat torn semantic artifact")
    .len();
    let borrowed_after_kill = SemanticIndex::read_from_disk(
        storage.path(),
        DELTA_PROJECT_KEY,
        project.path(),
        true,
        Some(&delta_fingerprint().as_string()),
    )
    .expect("borrowed reader should retain the last complete segment boundary");
    assert_eq!(borrowed_after_kill.to_bytes(), original.to_bytes());
    assert_eq!(
        fs::metadata(
            storage
                .path()
                .join("semantic")
                .join(DELTA_PROJECT_KEY)
                .join("semantic.bin")
        )
        .expect("restat torn semantic artifact")
        .len(),
        torn_bytes,
        "a borrowed reader must not truncate its owner's torn tail"
    );

    let after_kill = load_delta_index(storage.path(), project.path());
    assert_eq!(
        after_kill.to_bytes(),
        original.to_bytes(),
        "a torn final segment must expose the previous committed state"
    );

    write_delta_fixture(&changed, 0, 1);
    refresh_delta_paths(
        &mut original,
        project.path(),
        std::slice::from_ref(&changed),
    );
    assert!(original.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    assert_eq!(
        load_delta_index(storage.path(), project.path()).to_bytes(),
        original.to_bytes(),
        "the next writer must truncate the torn tail before appending its retry"
    );
}

#[cfg(unix)]
#[test]
#[ignore]
fn semantic_compaction_swap_child() {
    let root = PathBuf::from(std::env::var_os("AFT_TEST_SEMANTIC_ROOT").expect("child root"));
    let storage =
        PathBuf::from(std::env::var_os("AFT_TEST_SEMANTIC_STORAGE").expect("child storage"));
    let index = load_delta_index(&storage, &root);
    assert!(index.compact_to_disk_for_test(&storage, DELTA_PROJECT_KEY));
}

#[cfg(unix)]
#[test]
fn semantic_reader_open_during_compaction_sees_complete_generation() {
    let project = tempfile::tempdir().expect("create reader project");
    let storage = tempfile::tempdir().expect("create reader storage");
    let files = seed_delta_fixture(project.path(), 20);
    let changed = files[0].clone();
    let mut live = build_delta_index(project.path(), &files);
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    write_delta_fixture(&changed, 0, 1);
    refresh_delta_paths(&mut live, project.path(), std::slice::from_ref(&changed));
    assert!(live.write_to_disk(storage.path(), DELTA_PROJECT_KEY));
    let expected = live.to_bytes();

    let ready = storage.path().join("compaction-swap.ready");
    let release = ready.with_extension("release");
    let mut child = Command::new(std::env::current_exe().expect("semantic test executable"))
        .args([
            "--exact",
            COMPACTION_SWAP_CHILD_TEST,
            "--ignored",
            "--nocapture",
        ])
        .env("AFT_TEST_SEMANTIC_ROOT", project.path())
        .env("AFT_TEST_SEMANTIC_STORAGE", storage.path())
        .env("AFT_TEST_SEMANTIC_COMPACTION_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn semantic compaction child");
    wait_for_test_seam(&mut child, &ready);

    let during = SemanticIndex::read_from_disk(
        storage.path(),
        DELTA_PROJECT_KEY,
        project.path(),
        true,
        Some(&delta_fingerprint().as_string()),
    )
    .expect("borrowed reader opens old complete inode during compaction");
    assert_eq!(during.to_bytes(), expected);

    fs::write(&release, b"release").expect("release compaction swap");
    assert!(child.wait().expect("wait for compaction child").success());
    let after = load_delta_index(storage.path(), project.path());
    assert_eq!(after.to_bytes(), expected);
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
fn mismatched_load_preserves_cache_until_replacement_is_written() {
    init_test_logger();

    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let (mut index, _source_file) = build_test_index(project.path());
    let project_key = "mismatch-project";
    let semantic_file = storage
        .path()
        .join("semantic")
        .join(project_key)
        .join("semantic.bin");

    index.set_fingerprint(SemanticIndexFingerprint {
        backend: "openai_compatible".to_string(),
        model: "cached-model".to_string(),
        base_url: "http://127.0.0.1:1234/v1".to_string(),
        dimension: index.dimension(),
        chunking_version: 2,
        ..Default::default()
    });
    index.write_to_disk(storage.path(), project_key);
    let original_bytes = fs::read(&semantic_file).expect("read original semantic cache");

    let replacement_fingerprint = SemanticIndexFingerprint {
        backend: "ollama".to_string(),
        model: "replacement-model".to_string(),
        base_url: "http://127.0.0.1:11434".to_string(),
        dimension: index.dimension(),
        chunking_version: 3,
        ..Default::default()
    };
    assert!(SemanticIndex::read_from_disk(
        storage.path(),
        project_key,
        project.path(),
        false,
        Some(&replacement_fingerprint.as_string())
    )
    .is_none());
    assert_eq!(
        fs::read(&semantic_file).expect("re-read semantic cache after mismatch"),
        original_bytes,
        "mismatched load must leave the shared cache file byte-identical"
    );

    let logs = take_logs();
    assert!(
        logs.iter().any(|line| {
            line.contains("fingerprint mismatch")
                && line.contains("backend kind")
                && line.contains("model")
                && line.contains("base_url host")
                && line.contains("chunking version")
        }),
        "expected detailed fingerprint mismatch warning, got {logs:?}"
    );

    index.set_fingerprint(replacement_fingerprint.clone());
    index.write_to_disk(storage.path(), project_key);
    let replaced_bytes = fs::read(&semantic_file).expect("read replaced semantic cache");
    assert_ne!(replaced_bytes, original_bytes);

    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        project_key,
        project.path(),
        false,
        Some(&replacement_fingerprint.as_string()),
    )
    .expect("load replacement semantic cache");
    assert_eq!(
        restored
            .fingerprint()
            .expect("replacement fingerprint")
            .as_string(),
        replacement_fingerprint.as_string()
    );
}

#[test]
fn read_only_mismatch_returns_none_without_touching_shared_cache() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let (mut index, _source_file) = build_test_index(project.path());
    let project_key = "readonly-mismatch-project";
    let semantic_file = storage
        .path()
        .join("semantic")
        .join(project_key)
        .join("semantic.bin");

    index.set_fingerprint(SemanticIndexFingerprint {
        backend: "openai_compatible".to_string(),
        model: "cached-model".to_string(),
        base_url: "http://127.0.0.1:1234/v1".to_string(),
        dimension: index.dimension(),
        chunking_version: 2,
        ..Default::default()
    });
    index.write_to_disk(storage.path(), project_key);
    let original_bytes = fs::read(&semantic_file).expect("read original semantic cache");

    let mismatched = SemanticIndexFingerprint {
        backend: "ollama".to_string(),
        model: "replacement-model".to_string(),
        base_url: "http://127.0.0.1:11434".to_string(),
        dimension: index.dimension(),
        chunking_version: 3,
        ..Default::default()
    };
    assert!(SemanticIndex::read_from_disk(
        storage.path(),
        project_key,
        project.path(),
        true,
        Some(&mismatched.as_string())
    )
    .is_none());
    assert_eq!(
        fs::read(&semantic_file).expect("re-read semantic cache after read-only mismatch"),
        original_bytes
    );
}

#[test]
fn live_refresh_retries_deferred_new_file_after_deletion_frees_capacity() {
    let project = tempfile::tempdir().expect("create project dir");
    let old_file = project.path().join("src/old.rs");
    let new_file = project.path().join("src/new.rs");
    fs::create_dir_all(old_file.parent().expect("source parent")).expect("create src dir");
    fs::write(&old_file, "pub fn old_anchor() -> usize { 1 }\n").expect("write old file");
    fs::write(&new_file, "pub fn new_anchor() -> usize { 2 }\n").expect("write new file");

    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .map(|text| {
                    if text.contains("new_anchor") {
                        vec![0.0, 1.0, 0.0, 0.0]
                    } else {
                        vec![1.0, 0.0, 0.0, 0.0]
                    }
                })
                .collect(),
        )
    };
    let mut index = SemanticIndex::build(
        project.path(),
        std::slice::from_ref(&old_file),
        &mut embed,
        16,
    )
    .expect("build initial semantic index");
    assert_eq!(index.indexed_file_count(), 1);

    let mut progress = |_done: usize, _total: usize| {};
    index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&new_file),
            &mut embed,
            16,
            1,
            &mut progress,
        )
        .expect("defer new file at cap");
    let deferred_results = index.search(&[0.0, 1.0, 0.0, 0.0], 5);
    assert!(
        deferred_results
            .iter()
            .all(|result| result.name != "new_anchor"),
        "new file should be deferred while the cap is full: {deferred_results:?}"
    );

    fs::remove_file(&old_file).expect("delete old file");
    index
        .refresh_invalidated_files(
            project.path(),
            std::slice::from_ref(&old_file),
            &mut embed,
            16,
            1,
            &mut progress,
        )
        .expect("retry deferred file after deletion");

    let results = index.search(&[0.0, 1.0, 0.0, 0.0], 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file, new_file);
    assert!(
        results[0].snippet.contains("new_anchor"),
        "deferred file should be indexed after capacity frees: {results:?}"
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
fn semantic_stale_check_detects_same_mtime_same_size_content_change() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let source_file = project.path().join("src/lib.rs");
    fs::create_dir_all(source_file.parent().expect("source parent")).expect("create src dir");
    fs::write(
        &source_file,
        "pub fn handle_request(token: &str) -> bool {
    !token.is_empty()
}
",
    )
    .expect("write source file");
    let fixed_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 123_000_000);
    filetime::set_file_mtime(&source_file, fixed_mtime).expect("set fixed mtime");

    let files = vec![source_file.clone()];
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .map(|_| vec![1.0, 0.0, 0.0, 0.0])
                .collect(),
        )
    };
    let index =
        SemanticIndex::build(project.path(), &files, &mut embed, 16).expect("build semantic index");
    let freshness = cache_freshness::collect(&source_file).expect("collect source freshness");
    index.write_to_disk(storage.path(), "same-metadata-project");

    let mut restored = SemanticIndex::read_from_disk(
        storage.path(),
        "same-metadata-project",
        project.path(),
        false,
        None,
    )
    .expect("restore semantic index from disk");
    assert!(
        !restored.is_file_stale(&source_file),
        "freshly restored file should start hot"
    );

    let mut bytes = fs::read(&source_file).expect("read source bytes");
    let bang = bytes
        .iter()
        .position(|byte| *byte == b'!')
        .expect("fixture contains negation byte");
    bytes[bang] = b' ';
    fs::write(&source_file, &bytes).expect("rewrite source with same size");
    filetime::set_file_mtime(
        &source_file,
        filetime::FileTime::from_system_time(freshness.mtime),
    )
    .expect("restore original mtime");

    assert_eq!(
        cache_freshness::verify_file(&source_file, &freshness),
        FreshnessVerdict::HotFresh,
        "non-strict freshness misses same-size/same-mtime content edits"
    );
    assert!(
        restored.is_file_stale(&source_file),
        "semantic staleness must hash-check same-size/same-mtime edits"
    );

    let mut refreshed_chunks = 0usize;
    let mut refresh_embed = |texts: Vec<String>| {
        refreshed_chunks += texts.len();
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .map(|_| vec![1.0, 0.0, 0.0, 0.0])
                .collect(),
        )
    };
    let mut progress = |_done: usize, _total: usize| {};
    let summary = restored
        .refresh_stale_files(
            project.path(),
            &files,
            &mut refresh_embed,
            16,
            &mut progress,
        )
        .expect("strict refresh should re-embed stale file");

    assert_eq!(summary.changed, 1);
    assert_eq!(summary.added, 0);
    assert_eq!(summary.deleted, 0);
    assert!(refreshed_chunks > 0, "changed file should be re-embedded");
    assert!(
        !restored.is_file_stale(&source_file),
        "refreshed file should become fresh again"
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
    let before = fs::read(&semantic_file).expect("read v1 semantic cache");

    let expected_fingerprint = SemanticIndexFingerprint {
        backend: "fastembed".to_string(),
        model: "all-MiniLM-L6-v2".to_string(),
        base_url: "none".to_string(),
        dimension: 3,
        chunking_version: 2,
        ..Default::default()
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
    assert_eq!(
        fs::read(&semantic_file).expect("re-read v1 semantic cache"),
        before,
        "legacy semantic cache should stay on disk until a replacement is written"
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

/// Migration: V2 caches must be ignored on load so persisted snippets are
/// rebuilt with V4 range handling. `from_bytes` still parses V2 for low-level
/// compatibility, but `read_from_disk` rejects old cache files until a newer
/// writer replaces them.
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
        ..Default::default()
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
    let semantic_file = semantic_dir.join("semantic.bin");
    fs::write(&semantic_file, &bytes).expect("write v2 cache");
    let before = fs::read(&semantic_file).expect("read v2 semantic cache");

    assert!(SemanticIndex::read_from_disk(
        storage.path(),
        "v2-project",
        project.path(),
        false,
        Some(&fp_str)
    )
    .is_none());
    assert_eq!(
        fs::read(&semantic_file).expect("re-read v2 semantic cache"),
        before,
        "V2 semantic cache should stay on disk until a replacement is written"
    );
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
            ..Default::default()
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
    let root = tempfile::tempdir().expect("semantic cache root");
    let err = SemanticIndex::from_bytes(&bad_nanos, root.path())
        .expect_err("V3 with nanos >= 1e9 must be rejected");
    assert!(
        err.contains("nanos") && err.contains("1_000_000_000"),
        "nanos-overflow error should explain the rejection: {err}"
    );

    // Case 2: secs close to u64::MAX → SystemTime overflow rejected without
    // panicking. We pick secs = u64::MAX so adding any Duration carries past
    // the platform's representable range on every target.
    let overflow = build_v3_with_mtime(u64::MAX, 0);
    let err = SemanticIndex::from_bytes(&overflow, root.path())
        .expect_err("V3 with secs=u64::MAX must be rejected");
    assert!(
        err.contains("overflows SystemTime"),
        "SystemTime-overflow error should explain the rejection: {err}"
    );

    // Case 3: valid V3 payload with nanos = 999_999_999 (max valid) loads
    // cleanly — proves the boundary is strictly < 1e9, not <=.
    let boundary = build_v3_with_mtime(1_700_000_000, 999_999_999);
    let _ = SemanticIndex::from_bytes(&boundary, root.path())
        .expect("V3 with nanos=999_999_999 must load cleanly");
}
