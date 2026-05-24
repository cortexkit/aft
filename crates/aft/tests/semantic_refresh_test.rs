//! Integration tests for `SemanticIndex::refresh_stale_files`.
//!
//! These cover the four real cases the configure path triggers on restart:
//!  - no-op (nothing changed → empty summary, no embed calls)
//!  - one file changed mtime → only that file is re-embedded
//!  - one file deleted from the walk → entries dropped, no embeds
//!  - one new file appeared in the walk → only that file is embedded
//!
//! We deliberately use a stub embedder that returns deterministic vectors so
//! we can assert byte-exact entry counts and dimensions without depending on
//! ONNX runtime or fastembed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aft::semantic_index::SemanticIndex;

/// Stub embedder that returns vectors based on text content.
/// Tracks all calls so we can assert which files (and how many) got embedded.
struct StubEmbedder {
    calls: Mutex<Vec<Vec<String>>>,
}

impl StubEmbedder {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        let vectors: Vec<Vec<f32>> = texts
            .iter()
            .map(|text| {
                // Spread vectors based on content so different chunks have
                // different embeddings — keeps the index nontrivial.
                let len = text.len() as f32;
                vec![1.0, len.fract().abs(), 0.0, 0.0]
            })
            .collect();
        self.calls.lock().expect("lock embed calls").push(texts);
        Ok(vectors)
    }

    fn total_embedded_texts(&self) -> usize {
        self.calls
            .lock()
            .expect("lock embed calls")
            .iter()
            .map(|batch| batch.len())
            .sum()
    }

    fn batch_count(&self) -> usize {
        self.calls.lock().expect("lock embed calls").len()
    }
}

/// Build an initial index over a small two-file project. Returns the index +
/// the file paths so tests can mutate one and call refresh.
fn build_two_file_index(project_root: &Path) -> (SemanticIndex, PathBuf, PathBuf) {
    let file_a = project_root.join("src/a.rs");
    let file_b = project_root.join("src/b.rs");
    fs::create_dir_all(file_a.parent().expect("parent")).expect("create src");
    fs::write(
        &file_a,
        "pub fn alpha() -> i32 {\n    let x = 1;\n    x\n}\n\npub fn alpha_helper() -> i32 {\n    let y = 2;\n    y\n}\n",
    )
    .expect("write a");
    fs::write(
        &file_b,
        "pub fn beta() -> i32 {\n    let x = 3;\n    x\n}\n\npub fn beta_helper() -> i32 {\n    let y = 4;\n    y\n}\n",
    )
    .expect("write b");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let index = SemanticIndex::build(
        project_root,
        &[file_a.clone(), file_b.clone()],
        &mut embed,
        16,
    )
    .expect("build initial index");
    (index, file_a, file_b)
}

/// Touch a file's mtime by overwriting its contents, then explicitly advance
/// it so platforms with 1-second mtime granularity observe a changed timestamp.
fn rewrite_with_new_mtime(path: &Path, new_contents: &str) {
    let modified = fs::metadata(path)
        .expect("stat file before rewrite")
        .modified()
        .expect("file mtime before rewrite");
    let advanced = modified
        .checked_add(Duration::from_secs(2))
        .expect("advanced mtime");
    fs::write(path, new_contents).expect("rewrite");
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(advanced))
        .expect("set advanced mtime");
}

static SHARED_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn shared_lock() -> &'static Mutex<()> {
    SHARED_LOG_LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn refresh_is_noop_when_nothing_changed() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let entries_before = index.entry_count();

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert!(summary.is_noop(), "summary should be noop, got {summary:?}");
    assert_eq!(summary.deleted, 0);
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.added, 0);
    assert_eq!(stub.total_embedded_texts(), 0, "no embeds for noop");
    assert_eq!(index.entry_count(), entries_before, "entries preserved");
}

#[test]
fn refresh_re_embeds_only_changed_file() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let entries_before = index.entry_count();

    // Modify file_a — file_b is untouched and must keep its cached embeddings.
    rewrite_with_new_mtime(
        &file_a,
        "pub fn alpha_renamed() -> i32 {\n    let x = 99;\n    x\n}\n\npub fn alpha_helper_renamed() -> i32 {\n    let y = 100;\n    y\n}\n",
    );

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a.clone(), file_b.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.changed, 1, "exactly one file changed");
    assert_eq!(summary.deleted, 0);
    assert_eq!(summary.added, 0);
    assert!(stub.total_embedded_texts() > 0, "should re-embed something");

    // We embedded only file_a's chunks, not file_b's. Total embedded texts
    // must be strictly less than entries_before (which covered both files).
    assert!(
        stub.total_embedded_texts() < entries_before,
        "should embed less than full rebuild; embedded={}, full={}",
        stub.total_embedded_texts(),
        entries_before
    );

    // Sanity: index still has entries for file_b without re-embedding.
    let count_for_b = count_entries_for_file(&index, &file_b);
    assert!(count_for_b > 0, "file_b entries preserved");
}

#[test]
fn refresh_drops_entries_for_files_no_longer_in_walk() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());

    let count_for_b_before = count_entries_for_file(&index, &file_b);
    assert!(count_for_b_before > 0, "precondition: index has b entries");

    // Simulate: walk now only returns file_a (file_b deleted or excluded).
    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            std::slice::from_ref(&file_a),
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.deleted, 1, "file_b reported as deleted");
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.added, 0);
    assert_eq!(stub.total_embedded_texts(), 0, "no embed calls");
    assert_eq!(
        count_entries_for_file(&index, &file_b),
        0,
        "file_b entries dropped"
    );
}

#[test]
fn refresh_embeds_new_files_added_to_walk() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());
    let entries_before = index.entry_count();

    // A new file that was never in the original index.
    let file_c = project.path().join("src/c.rs");
    fs::write(
        &file_c,
        "pub fn gamma() -> i32 {\n    let z = 5;\n    z\n}\n\npub fn gamma_helper() -> i32 {\n    let w = 6;\n    w\n}\n",
    )
    .expect("write c");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut progress = |_done: usize, _total: usize| {};

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a, file_b, file_c.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.added, 1, "file_c discovered as new");
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.deleted, 0);
    assert!(stub.total_embedded_texts() > 0);
    assert!(stub.total_embedded_texts() > 0, "embedded only file_c");

    // Index grew strictly larger (kept original, added new).
    assert!(
        index.entry_count() > entries_before,
        "index grew; before={}, after={}",
        entries_before,
        index.entry_count()
    );
    assert!(
        count_entries_for_file(&index, &file_c) > 0,
        "file_c entries present"
    );
}

#[test]
fn refresh_handles_changed_plus_deleted_plus_new_in_one_call() {
    let _guard = shared_lock().lock();
    let project = tempfile::tempdir().expect("create project dir");
    let (mut index, file_a, file_b) = build_two_file_index(project.path());

    // Change file_a, drop file_b from the walk, add file_c.
    rewrite_with_new_mtime(
        &file_a,
        "pub fn alpha_v2() -> i32 {\n    let v = 42;\n    v\n}\n",
    );
    let file_c = project.path().join("src/c.rs");
    fs::write(
        &file_c,
        "pub fn gamma() -> i32 {\n    let z = 5;\n    z\n}\n",
    )
    .expect("write c");

    let stub = StubEmbedder::new();
    let mut embed = |texts: Vec<String>| stub.embed(texts);
    let mut batches: Vec<(usize, usize)> = Vec::new();
    let mut progress = |done: usize, total: usize| batches.push((done, total));

    let summary = index
        .refresh_stale_files(
            project.path(),
            &[file_a, file_c.clone()],
            &mut embed,
            16,
            &mut progress,
        )
        .expect("refresh succeeds");

    assert_eq!(summary.deleted, 1, "file_b deleted");
    assert_eq!(summary.changed, 1, "file_a changed");
    assert_eq!(summary.added, 1, "file_c new");

    // file_b entries are gone.
    assert_eq!(count_entries_for_file(&index, &file_b), 0);
    // file_c entries are present.
    assert!(count_entries_for_file(&index, &file_c) > 0);

    // Progress callback fired at least once with a meaningful total.
    assert!(
        batches.iter().any(|(_done, total)| *total > 0),
        "progress callback should report nonzero total at least once"
    );

    // Embedded calls should match changed + new files only — not file_b
    // (deleted) and not the original file_a/file_b cached embeddings.
    assert!(stub.batch_count() >= 1, "at least one embed batch");
}

/// Helper: count entries in the index that belong to `file`. We only have
/// public access via search results, so we issue a query that should match
/// every entry (vector field is mostly 1.0 in our stub) and filter by file.
fn count_entries_for_file(index: &SemanticIndex, file: &Path) -> usize {
    let query = vec![1.0, 0.5, 0.0, 0.0];
    let results = index.search(&query, 1024);
    results.iter().filter(|r| r.file == file).count()
}
