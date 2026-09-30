//! Checkout semantic views through the runtime that serves views-on search:
//! shared planes across checkouts, embed counts across sessions, and edits
//! reaching the fill through both the watcher and AFT's own writes.

use super::*;
use crate::semantic_index::{EmbedTextCaps, SemanticIndex, SemanticResult};
use std::sync::atomic::AtomicUsize;

const FILES: &[(&str, &str)] = &[
    (
        "src/alpha.rs",
        "pub fn alpha_total(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n\npub fn alpha_label() -> &'static str {\n    \"alpha\"\n}\n",
    ),
    (
        "src/beta.rs",
        "pub struct BetaCache {\n    entries: Vec<String>,\n}\n\nimpl BetaCache {\n    pub fn insert(&mut self, value: String) {\n        self.entries.push(value);\n    }\n}\n",
    ),
    (
        "src/gamma.rs",
        "fn gamma_parse(input: &str) -> Option<u64> {\n    input.trim().parse().ok()\n}\n",
    ),
    ("notes.txt", "not a semantic source\n"),
];

/// A deterministic model: equal texts get equal vectors.
fn vector(text: &str) -> Vec<f32> {
    blake3::hash(text.as_bytes()).as_bytes()[..16]
        .iter()
        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
        .collect()
}

#[derive(Default)]
struct Model {
    texts: AtomicUsize,
}

impl Model {
    fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|text| vector(text)).collect())
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }
}

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn producer() -> SemanticProducer {
    SemanticProducer::current("runtime-test-model", EmbedTextCaps::default())
}

fn open(storage: &Path, scope: &str, root: &Path) -> CheckoutSemantic {
    let runtime =
        CheckoutSemantic::new(storage, "family", scope, root, producer(), Weak::new()).unwrap();
    runtime.driver().register_write_intent();
    runtime.load().unwrap();
    runtime
}

fn refresh(runtime: &CheckoutSemantic, model: &Model) -> FillReport {
    runtime
        .refresh(FillBudget::default(), &mut |texts| model.embed(texts))
        .unwrap()
}

type Row = (String, String, u32, u32);

fn rows(root: &Path, results: &[SemanticResult]) -> Vec<Row> {
    results
        .iter()
        .map(|result| {
            (
                result
                    .file
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                result.name.clone(),
                result.start_line,
                result.score.to_bits(),
            )
        })
        .collect()
}

/// A full legacy build of `root`: its ranked rows for `query` and the number
/// of texts it embedded, which is the number of unique chunks.
fn cold(root: &Path, query: &str) -> (Vec<Row>, usize) {
    let files = ConfiguredMembershipWalker.files(root).unwrap();
    let mut texts = 0;
    let index = SemanticIndex::build(
        root,
        &files,
        &mut |batch: Vec<String>| {
            texts += batch.len();
            Ok(batch.iter().map(|text| vector(text)).collect())
        },
        64,
    )
    .unwrap();
    (rows(root, &index.search(&vector(query), 100)), texts)
}

fn query(runtime: &CheckoutSemantic, text: &str) -> SemanticQuery {
    runtime.search(&vector(text), 100, &|_| true).unwrap()
}

use super::super::first_load::MembershipWalker;

/// Two worktrees of one family with identical content embed each chunk once
/// between them; a later session on the same worktrees embeds nothing and
/// ranks exactly like a legacy build of the same content.
#[test]
fn two_worktrees_two_sessions_embed_each_chunk_once() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_tree(first.path(), FILES);
    write_tree(second.path(), FILES);
    let (expected, chunks) = cold(first.path(), "beta cache insert");
    assert!(chunks > 0);

    let model = Model::default();
    {
        let a = open(storage.path(), "worktree-a", first.path());
        let b = open(storage.path(), "worktree-b", second.path());
        assert!(Arc::ptr_eq(a.plane(), b.plane()), "one plane per storage");
        refresh(&a, &model);
        refresh(&b, &model);
        assert_eq!(model.texts(), chunks, "identical content embedded twice");
        let answer = query(&b, "beta cache insert");
        assert!(answer.complete(), "second worktree incomplete: {answer:?}");
        assert_eq!(rows(second.path(), &answer.results), expected);
    }

    // Session 2: the first session's checkouts and plane are gone.
    let embedded = model.texts();
    let a = open(storage.path(), "worktree-a", first.path());
    let b = open(storage.path(), "worktree-b", second.path());
    refresh(&a, &model);
    refresh(&b, &model);
    assert_eq!(model.texts(), embedded, "a later session embedded again");
    for (runtime, root) in [(&a, first.path()), (&b, second.path())] {
        let answer = query(runtime, "beta cache insert");
        assert!(answer.complete(), "incomplete after restart: {answer:?}");
        assert_eq!(rows(root, &answer.results), expected);
    }
}

/// An edit the watcher reports and an edit AFT writes each reach the fill:
/// the new content is embedded and the next query scores it.
#[test]
fn watcher_and_aft_edits_reach_the_fill() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    write_tree(&root_path, FILES);
    let runtime = open(storage.path(), "scope", &root_path);
    let model = Model::default();
    refresh(&runtime, &model);

    // Outside edit: the watcher drain hands the path to the driver.
    let before = model.texts();
    write_tree(
        &root_path,
        &[(
            "src/gamma.rs",
            "pub fn gamma_watched_rewrite() -> u8 {\n    3\n}\n",
        )],
    );
    runtime
        .driver()
        .record_absolute_change(&root_path.join("src/gamma.rs"));
    let pending = query(&runtime, "gamma watched rewrite");
    assert!(
        pending.pending.contains(&root_path.join("src/gamma.rs")),
        "an unreconciled edit must be a named gap: {pending:?}"
    );
    let report = refresh(&runtime, &model);
    assert!(report.embedded_keys >= 1 && model.texts() > before);
    let answer = query(&runtime, "gamma watched rewrite");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(&root_path, &answer.results),
        cold(&root_path, "gamma watched rewrite").0
    );

    // AFT write: the write-intent listener records it before the bytes land.
    let before = model.texts();
    let target = root_path.join("src/alpha.rs");
    {
        let _intent = crate::views::intent::record_paths([target.as_path()]);
        std::fs::write(&target, "pub fn alpha_written_by_aft() -> u8 {\n    1\n}\n").unwrap();
    }
    assert!(
        query(&runtime, "alpha written").pending.contains(&target),
        "an acknowledged write must be a named gap until reconciled"
    );
    refresh(&runtime, &model);
    assert!(model.texts() > before, "the AFT write was never embedded");
    let answer = query(&runtime, "alpha written by aft");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(&root_path, &answer.results),
        cold(&root_path, "alpha written by aft").0
    );
}

fn pending_semantic(runtime: &CheckoutSemantic) -> usize {
    runtime
        .installed()
        .generation()
        .manifest()
        .entries()
        .filter(|(_, entry)| {
            entry
                .plane_state(crate::blob_store::v2::FamilyPlane::Semantic)
                .is_some_and(|state| state.is_pending())
        })
        .count()
}

/// A catch-up that takes several budgeted rounds serves each round's vectors
/// at once but publishes them in one fold at the end, because every fold
/// re-walks the whole checkout.
#[test]
fn budgeted_rounds_fold_once_when_caught_up() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let runtime = open(storage.path(), "scope", root.path());
    let model = Model::default();
    let one_file = FillBudget {
        max_files: 1,
        ..FillBudget::default()
    };
    assert_eq!(pending_semantic(&runtime), 3);
    let first = runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((first.installed, first.deferred), (1, 2));
    let generation = runtime.installed().generation().name().to_owned();
    assert_eq!(pending_semantic(&runtime), 3, "folded before catching up");
    assert_eq!(query(&runtime, "alpha").pending.len(), 2);
    runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!(runtime.installed().generation().name(), generation);
    let last = runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((last.installed, last.deferred), (1, 0));
    assert_eq!(pending_semantic(&runtime), 0, "caught up but never folded");
    assert!(query(&runtime, "alpha").complete());
}

fn fast_schedule(root: &Path) -> FillSchedule {
    FillSchedule {
        root: root.to_path_buf(),
        quiet_window: Duration::from_millis(10),
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(80),
        limiter: crate::cold_build_limiter::isolated_limiter(1),
        paused: Box::new(|| false),
        max_batch: 64,
    }
}

/// A lane whose checkout is loaded and served, as the worker leaves it.
fn served_lane(
    storage: &Path,
    root: &Path,
) -> (
    Arc<CheckoutSemanticSlot>,
    u64,
    crossbeam_channel::Receiver<()>,
    Arc<CheckoutSemantic>,
) {
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let runtime = Arc::new(
        CheckoutSemantic::new(
            storage,
            "family",
            "scope",
            root,
            producer(),
            Arc::downgrade(&slot),
        )
        .unwrap(),
    );
    runtime.load().unwrap();
    assert!(slot.install(epoch, Arc::clone(&runtime)));
    (slot, epoch, wake, runtime)
}

/// A fill that fails because the embedding backend is down is retried on a
/// backoff: with no edit, wake or reconfigure, the view becomes complete once
/// the backend answers again.
#[test]
fn transient_embed_errors_retry_until_complete_without_a_wake() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let calls = Arc::clone(&calls);
        let root = root.path().to_path_buf();
        std::thread::spawn(move || {
            let schedule = fast_schedule(&root);
            serve_fills(&weak, epoch, &wake, &schedule, &mut |texts: Vec<String>| {
                // The backend is unreachable for the first three calls.
                if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                    return Err("embedding backend unreachable".to_string());
                }
                Ok(texts.iter().map(|text| vector(text)).collect())
            });
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let complete = loop {
        if query(&runtime, "alpha").complete() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    slot.clear();
    worker.join().unwrap();
    assert!(
        complete,
        "a transient embed error left the view partial with no retry"
    );
    assert!(calls.load(Ordering::SeqCst) >= 4);
}

/// A superseded runtime of the same view (a lane restarted with the same
/// producer) must not release the state of the runtime that replaced it.
#[test]
fn dropping_a_superseded_runtime_keeps_the_replacements_view() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let old = open(storage.path(), "scope", root.path());
    let new = open(storage.path(), "scope", root.path());
    assert!(Arc::ptr_eq(old.plane(), new.plane()));
    refresh(&new, &Model::default());
    drop(old);
    let answer = new
        .search(&vector("alpha"), 10, &|_| true)
        .expect("the replacement lost its resident generation");
    assert!(
        answer.complete(),
        "the replacement lost its fills: {answer:?}"
    );
}

/// An AFT write under the root wakes the lane's worker; one elsewhere does not.
#[test]
fn aft_writes_wake_only_their_own_lane() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    write_tree(&root_path, FILES);
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let runtime = CheckoutSemantic::new(
        storage.path(),
        "family",
        "scope",
        &root_path,
        producer(),
        Arc::downgrade(&slot),
    )
    .unwrap();
    assert!(slot.install(epoch, Arc::new(runtime)));
    {
        let _intent = crate::views::intent::record_paths([other.path().join("x.rs").as_path()]);
    }
    assert!(wake.try_recv().is_err());
    {
        let _intent =
            crate::views::intent::record_paths([root_path.join("src/alpha.rs").as_path()]);
    }
    assert!(wake.try_recv().is_ok());
    slot.clear();
    assert!(slot.runtime().is_none());
    assert!(!slot.install(epoch, Arc::new(open(storage.path(), "scope", &root_path))));
}

/// Resident memory of this plane against the legacy index for the same
/// checkout. Run on a real tree with
/// `AFT_SEMANTIC_MEMORY_ROOT=<checkout> cargo test -p agent-file-tools --lib
/// semantic_runtime::tests::report_resident_memory -- --ignored --nocapture`.
#[test]
#[ignore = "measurement, run by hand on a real checkout"]
fn report_resident_memory() {
    let root = std::env::var_os("AFT_SEMANTIC_MEMORY_ROOT")
        .map(PathBuf::from)
        .expect("AFT_SEMANTIC_MEMORY_ROOT");
    let root = std::fs::canonicalize(root).unwrap();
    // Vectors as wide as the default local model's (384 floats), so the
    // byte counts match what a real embedding run would hold.
    let wide = |text: &str| -> Vec<f32> {
        let seed = blake3::hash(text.as_bytes());
        (0..384)
            .map(|index| f32::from(seed.as_bytes()[index % 32]) / 255.0)
            .collect()
    };
    let storage = tempfile::tempdir().unwrap();
    let runtime = open(storage.path(), "memory", &root);
    let started = Instant::now();
    let mut texts = 0usize;
    loop {
        let report = runtime
            .refresh(
                FillBudget {
                    max_files: 100_000,
                    ..FillBudget::default()
                },
                &mut |batch: Vec<String>| {
                    texts += batch.len();
                    Ok(batch.iter().map(|text| wide(text)).collect())
                },
            )
            .unwrap();
        if report.deferred == 0 {
            break;
        }
    }
    let fill_ms = started.elapsed().as_millis();
    let _ = runtime.search(&wide("query"), 10, &|_| true).unwrap();
    let memory = runtime.memory();
    let files = ConfiguredMembershipWalker.files(&root).unwrap();
    let legacy = SemanticIndex::build(
        &root,
        &files,
        &mut |batch: Vec<String>| Ok(batch.iter().map(|text| wide(text)).collect()),
        64,
    )
    .unwrap();
    println!(
        "semantic memory root={} texts={} fill_ms={} view_family_arena_bytes={} view_private_bytes={} legacy_index_bytes={}",
        root.display(),
        texts,
        fill_ms,
        memory.family_arena,
        memory.private,
        legacy.estimated_memory().estimated_bytes.unwrap_or(0)
    );
}
