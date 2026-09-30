//! Semantic views on the shared-base overlay, through the composite runtime
//! contracts (`CheckoutDriver`, `SiblingLoader`) and the public semantic plane.
//!
//! Every parity check compares with an independent cold rebuild: a full
//! `SemanticIndex` build of the frozen checkout, with a deterministic model
//! (the vector is a hash of the model name and the text), so equal inputs get
//! bit-equal scores and nothing is shared with the view under test.
//!
//! The embed-count tests measure the property this plane exists for: equal
//! content (same path, bytes and producer) is embedded once per family, across
//! views and across sessions, and a view of an older branch embeds only content
//! nobody embedded before.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aft::blob_store::v2::{FamilyPlane, TrigramPolicy};
use aft::semantic_index::{EmbedTextCaps, SemanticIndex, SemanticResult};
use aft::views::contracts::{PlaneAdapter, PlaneLoader, ViewAccess};
use aft::views::first_load::{
    CheckoutDriver, CompositePlane, ConfiguredMembershipWalker, MembershipWalker, QueryState,
    SiblingLoader,
};
use aft::views::manifest_v2::Producers;
use aft::views::parity_harness::{self, FrozenCheckout, IsolatedStore, PlaneOracle};
use aft::views::readiness::PlaneReadiness;
use aft::views::registry::{FamilyRegistry, ViewRegistration};
use aft::views::semantic::{
    FillBudget, FillReport, SemanticPlane, SemanticProducer, SemanticQuery,
};
use aft::views::snapshot::{LiveDelta, Snapshot};

fn vector(model: &str, text: &str) -> Vec<f32> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(model.as_bytes());
    hasher.update(&[0]);
    hasher.update(text.as_bytes());
    hasher.finalize().as_bytes()[..16]
        .iter()
        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
        .collect()
}

/// Counts every text that reaches the model, across views and threads.
#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    texts: AtomicUsize,
    delay: Option<Duration>,
}

impl Model {
    fn embed(&self, model: &str, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        if let Some(delay) = self.delay {
            std::thread::sleep(delay);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|text| vector(model, text)).collect())
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn module(index: usize, flavour: &str) -> String {
    format!(
        "pub fn load_{index}_{flavour}(path: &str) -> Option<String> {{\n    std::fs::read_to_string(path).ok()\n}}\n\npub struct Record{index} {{\n    pub id: u64,\n}}\n\nimpl Record{index} {{\n    pub fn describe(&self) -> String {{\n        format!(\"{flavour} {{}}\", self.id)\n    }}\n}}\n"
    )
}

/// A tree of `count` Rust modules plus one non-semantic file.
fn tree(root: &Path, count: usize) {
    for index in 0..count {
        write(
            root,
            &format!("src/module_{index}.rs"),
            &module(index, "base"),
        );
    }
    write(root, "README.txt", "not a semantic source\n");
}

fn plane(storage: &Path, model: &str) -> Arc<SemanticPlane> {
    Arc::new(SemanticPlane::new(
        storage.to_path_buf(),
        SemanticProducer::current(model, EmbedTextCaps::default()),
    ))
}

struct Checkout {
    root: PathBuf,
    owner: ViewRegistration,
    access: ViewAccess,
    driver: Arc<CheckoutDriver>,
    loader: SiblingLoader,
    plane: Arc<SemanticPlane>,
}

impl Checkout {
    fn open(storage: &Path, scope: &str, root: &Path, plane: &Arc<SemanticPlane>) -> Self {
        let registry = FamilyRegistry::open(storage, "family").unwrap();
        let owner = registry.register_view(scope, root).unwrap();
        let composite: Arc<dyn CompositePlane> = plane.clone();
        let adapter: Arc<dyn PlaneAdapter> = plane.clone();
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: "trigram".into(),
                    semantic: Some(plane.semantic_producer().id()),
                    callgraph: "callgraph".into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![composite],
            )
            .with_adapters(vec![adapter.clone()]),
        );
        let loader = SiblingLoader::new(driver.clone(), vec![adapter]);
        Self {
            root: root.to_path_buf(),
            access: ViewAccess::Owner(owner.clone()),
            owner,
            driver,
            loader,
            plane: plane.clone(),
        }
    }

    /// Loads the checkout: publishes and installs its own generation. Called
    /// again, it folds the installed fills into a new generation.
    fn load(&self) -> Snapshot {
        self.loader.load(&self.access).unwrap().snapshot
    }

    fn installed(&self) -> Snapshot {
        self.driver
            .installed_state(&self.access, FamilyPlane::Semantic)
            .0
    }

    fn fill_with(
        &self,
        snapshot: &Snapshot,
        budget: FillBudget,
        model: &Model,
        name: &str,
    ) -> FillReport {
        let current = snapshot.clone();
        self.plane
            .fill(
                &self.owner,
                snapshot,
                budget,
                &mut |texts| model.embed(name, texts),
                &move || current.clone(),
            )
            .unwrap()
    }

    fn fill(&self, snapshot: &Snapshot, model: &Model, name: &str) -> FillReport {
        self.fill_with(snapshot, FillBudget::default(), model, name)
    }

    fn query(&self, snapshot: &Snapshot, model: &str, text: &str) -> SemanticQuery {
        self.plane
            .search(
                &self.access,
                &self.root,
                snapshot,
                &vector(model, text),
                1000,
                &|_| true,
            )
            .unwrap()
    }
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

/// A full cold build of `root`'s walker membership; returns the ranked rows
/// and the number of texts it embedded.
fn cold(root: &Path, model: &str, text: &str) -> (Vec<Row>, usize) {
    let files = ConfiguredMembershipWalker.files(root).unwrap();
    let mut texts = 0;
    let index = SemanticIndex::build(
        root,
        &files,
        &mut |batch: Vec<String>| {
            texts += batch.len();
            Ok(batch.iter().map(|text| vector(model, text)).collect())
        },
        64,
    )
    .unwrap();
    (rows(root, &index.search(&vector(model, text), 1000)), texts)
}

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
    }
}

/// The live checkout against its installed generation, as the watcher's
/// strict reconcile would leave it.
fn reconciled(checkout: &Checkout) -> LiveDelta {
    let mut delta = LiveDelta::new(Arc::clone(checkout.installed().generation()));
    aft::views::live_delta::reconcile(&mut delta, &checkout.root, &policy());
    delta
}

struct Oracle<'a> {
    checkout: &'a Checkout,
    snapshot: Snapshot,
    query: &'a str,
}

impl PlaneOracle for Oracle<'_> {
    type Observation = Vec<(String, String, u32, u32)>;

    fn name(&self) -> &str {
        "semantic-result-identities-and-scores"
    }

    fn observe_view(&self, root: &Path) -> Self::Observation {
        let answer = self.checkout.query(&self.snapshot, "model-a", self.query);
        assert!(answer.complete(), "view not complete: {answer:?}");
        rows(root, &answer.results)
    }

    fn rebuild_cold(&self, frozen: &FrozenCheckout, store: &IsolatedStore) -> Self::Observation {
        assert!(fs::read_dir(store.path()).unwrap().next().is_none());
        cold(frozen.root(), "model-a", self.query).0
    }
}

/// Parity with a cold rebuild after every step of the shared edit schedules,
/// including tombstones (delete, rename), reverts, A→B→A, untracked files and
/// ignored and re-included files. Before each fill the view may lag, but it
/// never scores a path that is not a current member with its current bytes.
#[test]
fn semantic_parity_with_cold_rebuild_across_edit_schedules() {
    let base = "pub fn schedule_base(value: u32) -> u32 {\n    value + 1\n}\n";
    for schedule in
        parity_harness::standard_schedules("src/base.rs", base.as_bytes(), "src/ignored.rs")
    {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        tree(root.path(), 3);
        write(root.path(), "src/base.rs", base);
        write(root.path(), "src/ignored.rs", "pub fn hidden_item() {}\n");
        write(
            root.path(),
            parity_harness::FIXTURE_IGNORE_FILE,
            "src/ignored.rs\n",
        );
        let plane = plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope", root.path(), &plane);
        let snapshot = checkout.load();
        checkout.fill(&snapshot, &Model::default(), "model-a");
        checkout.load();
        for (step_index, step) in schedule.steps.iter().enumerate() {
            let before_reconcile = checkout.installed();
            step.apply(root.path()).unwrap();
            let members = parity_harness::walker_membership(root.path())
                .unwrap()
                .into_keys()
                // Rows carry native paths; compare in that form.
                .map(|path| {
                    std::str::from_utf8(path.as_bytes())
                        .unwrap()
                        .split('/')
                        .collect::<PathBuf>()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<BTreeSet<_>>();
            let (cold_rows, _) = cold(root.path(), "model-a", "schedule base value");
            // The first query after an ignore-file edit, before any reconcile
            // has reached the snapshot, still scores current members only.
            if step
                .paths()
                .iter()
                .any(|path| path == Path::new(parity_harness::FIXTURE_IGNORE_FILE))
            {
                let early = checkout.query(&before_reconcile, "model-a", "schedule base value");
                assert!(
                    early.unvouched,
                    "{}: ignore edit went unnoticed",
                    schedule.name
                );
                for row in rows(root.path(), &early.results) {
                    assert!(
                        members.contains(&row.0),
                        "{} step {step_index}: scored non-member {row:?}",
                        schedule.name
                    );
                }
            }
            let delta = reconciled(&checkout);
            let lagging = checkout.query(&delta.snapshot(), "model-a", "schedule base value");
            let pending = lagging
                .pending
                .iter()
                .map(|path| {
                    path.strip_prefix(root.path())
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<BTreeSet<_>>();
            for row in rows(root.path(), &lagging.results) {
                assert!(
                    cold_rows.contains(&row),
                    "{} step {step_index}: stale row {row:?}",
                    schedule.name
                );
            }
            for row in &cold_rows {
                assert!(
                    rows(root.path(), &lagging.results).contains(row) || pending.contains(&row.0),
                    "{} step {step_index}: undisclosed missing row {row:?}",
                    schedule.name
                );
            }
            let snapshot = delta.snapshot();
            let model = Model::default();
            checkout
                .plane
                .fill(
                    &checkout.owner,
                    &snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed("model-a", texts),
                    &|| delta.snapshot(),
                )
                .unwrap();
            let step_dir = scratch.path().join(format!("{step_index}"));
            fs::create_dir_all(&step_dir).unwrap();
            let oracle = Oracle {
                checkout: &checkout,
                snapshot: delta.snapshot(),
                query: "schedule base value",
            };
            parity_harness::check_parity(&oracle, root.path(), &step_dir, schedule.name)
                .unwrap()
                .unwrap_or_else(|mismatch| panic!("{mismatch}"));
            // Fold the step into a generation; parity holds from it too.
            checkout.load();
            let folded = checkout.query(&checkout.installed(), "model-a", "schedule base value");
            assert!(folded.complete(), "{}: {folded:?}", schedule.name);
            assert_eq!(
                rows(root.path(), &folded.results),
                cold_rows,
                "{}",
                schedule.name
            );
        }
    }
}

/// Two views and two sessions over the same content make one model call per
/// chunk; a pool worktree whose session ended before publication reuses its
/// embeddings; a view of an older branch embeds only content never embedded.
/// The legacy counts are what per-root builds embed for the same scenario.
#[test]
fn embed_counts_views_and_sessions_share_identical_content() {
    const FILES: usize = 40;
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    tree(first.path(), FILES);
    tree(second.path(), FILES);
    let (_, chunks) = cold(first.path(), "model-a", "load");

    // Session 1: two views of identical content.
    let model = Model::default();
    let plane_one = plane(storage.path(), "model-a");
    let a = Checkout::open(storage.path(), "pool-a", first.path(), &plane_one);
    let b = Checkout::open(storage.path(), "pool-b", second.path(), &plane_one);
    let snapshot_a = a.load();
    let snapshot_b = b.load();
    a.fill(&snapshot_a, &model, "model-a");
    b.fill(&snapshot_b, &model, "model-a");
    let two_views = model.texts();
    assert_eq!(
        two_views, chunks,
        "two views embedded identical content twice"
    );
    // The session ends before either view publishes its fills. This is the
    // case where the resident index's per-root in-memory changes were lost
    // and the next session embedded the same files again.
    drop((a, b, plane_one));

    // Session 2 on the same pool worktrees.
    let plane_two = plane(storage.path(), "model-a");
    let a = Checkout::open(storage.path(), "pool-a", first.path(), &plane_two);
    let b = Checkout::open(storage.path(), "pool-b", second.path(), &plane_two);
    let snapshot_a = a.load();
    let snapshot_b = b.load();
    assert_eq!(
        plane_two.readiness(&a.access, &snapshot_a),
        PlaneReadiness::Ready {
            pending: 0,
            failed: 0
        },
        "publication did not mark stored content ready"
    );
    a.fill(&snapshot_a, &model, "model-a");
    b.fill(&snapshot_b, &model, "model-a");
    let two_sessions = model.texts() - two_views;
    assert_eq!(two_sessions, 0, "the second session re-embedded");
    assert_eq!(
        rows(
            first.path(),
            &a.query(&snapshot_a, "model-a", "read record").results
        ),
        cold(first.path(), "model-a", "read record").0
    );

    // A view of an older branch: 30 modules as on main, 10 changed, 5 extra.
    let older = tempfile::tempdir().unwrap();
    tree(older.path(), FILES);
    for index in 0..10 {
        write(
            older.path(),
            &format!("src/module_{index}.rs"),
            &module(index, "older"),
        );
    }
    for index in FILES..FILES + 5 {
        write(
            older.path(),
            &format!("src/module_{index}.rs"),
            &module(index, "older"),
        );
    }
    let novel = tempfile::tempdir().unwrap();
    for index in (0..10).chain(FILES..FILES + 5) {
        write(
            novel.path(),
            &format!("src/module_{index}.rs"),
            &module(index, "older"),
        );
    }
    let (_, novel_chunks) = cold(novel.path(), "model-a", "load");
    let (_, older_chunks) = cold(older.path(), "model-a", "load");
    let c = Checkout::open(storage.path(), "older", older.path(), &plane_two);
    let snapshot_c = c.load();
    let before = model.texts();
    c.fill(&snapshot_c, &model, "model-a");
    let older_branch = model.texts() - before;
    assert_eq!(
        older_branch, novel_chunks,
        "the older branch re-embedded shared content"
    );
    let folded = c.load();
    assert_eq!(
        rows(
            older.path(),
            &c.query(&folded, "model-a", "read record").results
        ),
        cold(older.path(), "model-a", "read record").0
    );

    // The same scenario through one full `SemanticIndex` build per checkout
    // and session, which is what a root re-embeds when nothing it held
    // survived; measured, not derived, with the same model and batch size.
    let per_root = |root: &Path| cold(root, "model-a", "load").1;
    let legacy_two_views = per_root(first.path()) + per_root(second.path());
    let legacy_second_session = per_root(first.path()) + per_root(second.path());
    let legacy_older = per_root(older.path());
    eprintln!(
        "semantic embed texts (chunks per checkout = {chunks}):\n\
         two views, one session: per-root builds {legacy_two_views} -> views {two_views}\n\
         second session on the same pool worktrees: per-root builds {legacy_second_session} -> views {two_sessions}\n\
         older-branch view ({older_chunks} chunks, {novel_chunks} never embedded): per-root build {legacy_older} -> views {older_branch}"
    );
}

/// Two views filling the same content at the same time: one fill embeds each
/// key, the other waits for it. The model sees each chunk once.
#[test]
fn concurrent_fills_of_two_views_make_one_model_call_per_chunk() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    tree(first.path(), 12);
    tree(second.path(), 12);
    let (_, chunks) = cold(first.path(), "model-a", "load");
    let plane = plane(storage.path(), "model-a");
    let a = Checkout::open(storage.path(), "a", first.path(), &plane);
    let b = Checkout::open(storage.path(), "b", second.path(), &plane);
    let snapshot_a = a.load();
    let snapshot_b = b.load();
    let model = Model {
        delay: Some(Duration::from_millis(40)),
        ..Model::default()
    };
    let budget = FillBudget {
        max_batch: 4,
        ..FillBudget::default()
    };
    let (report_a, report_b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| a.fill_with(&snapshot_a, budget, &model, "model-a"));
        let second = scope.spawn(|| b.fill_with(&snapshot_b, budget, &model, "model-a"));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(
        model.texts(),
        chunks,
        "identical content was embedded by both views: {report_a:?} {report_b:?}"
    );
    assert_eq!(report_a.installed + report_b.installed, 24);
    let answer_a = a.query(&a.load(), "model-a", "describe record");
    let answer_b = b.query(&b.load(), "model-a", "describe record");
    assert_eq!(
        rows(first.path(), &answer_a.results),
        rows(second.path(), &answer_b.results)
    );
    assert_eq!(
        rows(first.path(), &answer_a.results),
        cold(first.path(), "model-a", "describe record").0
    );
}

const CHILD: &str = "per_checkout_semantic::semantic_persistence_child";

/// One session in its own process. `SEMANTIC_MODE` picks what it does:
/// `fill` fills and exits without publishing, `fill-fold` also publishes,
/// `park` fills one file per fill and parks inside the model call of the
/// second fill until it is killed. Embedded texts go to `texts-<label>`.
#[test]
#[ignore]
fn semantic_persistence_child() {
    let storage = PathBuf::from(std::env::var_os("SEMANTIC_STORAGE").unwrap());
    let root = PathBuf::from(std::env::var_os("SEMANTIC_ROOT").unwrap());
    let mode = std::env::var("SEMANTIC_MODE").unwrap();
    let label = std::env::var("SEMANTIC_LABEL").unwrap();
    let plane = plane(&storage, "model-a");
    let checkout = Checkout::open(&storage, "pool", &root, &plane);
    let snapshot = checkout.load();
    let texts = AtomicUsize::new(0);
    let fills = AtomicUsize::new(0);
    let budget = FillBudget {
        max_files: if mode == "park" { 1 } else { usize::MAX },
        ..FillBudget::default()
    };
    loop {
        let fill = fills.fetch_add(1, Ordering::SeqCst);
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                budget,
                &mut |batch: Vec<String>| {
                    if mode == "park" && fill == 1 {
                        fs::write(storage.join("ready"), b"ready").unwrap();
                        loop {
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                    texts.fetch_add(batch.len(), Ordering::SeqCst);
                    Ok(batch.iter().map(|text| vector("model-a", text)).collect())
                },
                &|| snapshot.clone(),
            )
            .unwrap();
        fs::write(
            storage.join(format!("texts-{label}")),
            texts.load(Ordering::SeqCst).to_string(),
        )
        .unwrap();
        if report.deferred == 0 {
            break;
        }
    }
    if mode == "fill-fold" {
        checkout.load();
    }
}

fn spawn(storage: &Path, root: &Path, mode: &str, label: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env("SEMANTIC_STORAGE", storage)
        .env("SEMANTIC_ROOT", root)
        .env("SEMANTIC_MODE", mode)
        .env("SEMANTIC_LABEL", label)
        .spawn()
        .unwrap()
}

fn texts_of(storage: &Path, label: &str) -> usize {
    fs::read_to_string(storage.join(format!("texts-{label}")))
        .map(|text| text.parse().unwrap())
        .unwrap_or(0)
}

/// Real process exits: a session fills a pool worktree and exits before
/// publishing; the next session (a new process) embeds nothing, publishes,
/// and a third process answers exactly like a cold rebuild. A process for
/// another model then scores none of those vectors.
#[test]
fn semantic_real_restart_reuses_embeddings_across_sessions() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    tree(root.path(), 6);
    let (cold_rows, chunks) = cold(root.path(), "model-a", "load path");
    assert!(spawn(storage.path(), root.path(), "fill", "one")
        .wait()
        .unwrap()
        .success());
    assert_eq!(texts_of(storage.path(), "one"), chunks);
    assert!(spawn(storage.path(), root.path(), "fill-fold", "two")
        .wait()
        .unwrap()
        .success());
    assert_eq!(
        texts_of(storage.path(), "two"),
        0,
        "the restarted session re-embedded"
    );

    let plane_a = plane(storage.path(), "model-a");
    let checkout = Checkout::open(storage.path(), "pool", root.path(), &plane_a);
    let snapshot = checkout.load();
    assert_eq!(
        plane_a.readiness(&checkout.access, &snapshot),
        PlaneReadiness::Ready {
            pending: 0,
            failed: 0
        }
    );
    let answer = checkout.query(&snapshot, "model-a", "load path");
    assert!(answer.complete());
    assert_eq!(rows(root.path(), &answer.results), cold_rows);
    drop(checkout);

    // Another model: the stored model-a vectors are incompatible and stay unscored.
    let plane_b = plane(storage.path(), "model-b");
    let checkout_b = Checkout::open(storage.path(), "pool", root.path(), &plane_b);
    plane_b
        .open_generation(&checkout_b.access, snapshot.generation())
        .unwrap();
    let stale = checkout_b.query(&snapshot, "model-b", "load path");
    assert!(stale.results.is_empty());
    assert_eq!(stale.pending.len(), 6);
}

/// A process killed inside a fill, after one file was stored: the published
/// generation still names every file pending, the stored file is reused, and
/// once filled the answer equals a cold rebuild.
#[cfg(unix)]
#[test]
fn semantic_forced_kill_during_fill_keeps_pending_and_stored_work() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    tree(root.path(), 4);
    let mut child = spawn(storage.path(), root.path(), "park", "killed");
    let started = std::time::Instant::now();
    while !storage.path().join("ready").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before parking"
        );
        assert!(started.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let stored = texts_of(storage.path(), "killed");
    assert!(stored > 0, "the first fill stored nothing before the kill");

    let plane = plane(storage.path(), "model-a");
    let checkout = Checkout::open(storage.path(), "pool", root.path(), &plane);
    let installed_before = {
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let view = registry.register_view("pool", root.path()).unwrap();
        let store = view.view_store().unwrap();
        let current = store.current_generation().unwrap().unwrap();
        store.load_manifest_v2(&current).unwrap()
    };
    let pending_before = installed_before
        .entries()
        .filter(|(_, entry)| {
            entry
                .plane_state(FamilyPlane::Semantic)
                .is_some_and(|state| state.is_pending())
        })
        .count();
    assert_eq!(
        pending_before, 4,
        "the killed session's generation lost pending work"
    );
    let snapshot = checkout.load();
    assert_eq!(
        plane.readiness(&checkout.access, &snapshot),
        PlaneReadiness::Ready {
            pending: 3,
            failed: 0
        },
        "the file stored before the kill was not reused"
    );
    let model = Model::default();
    checkout.fill(&snapshot, &model, "model-a");
    let (cold_rows, chunks) = cold(root.path(), "model-a", "describe");
    assert_eq!(model.texts() + stored, chunks);
    let folded = checkout.load();
    let answer = checkout.query(&folded, "model-a", "describe");
    assert!(answer.complete());
    assert_eq!(rows(root.path(), &answer.results), cold_rows);
}
