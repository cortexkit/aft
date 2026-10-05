//! Per-checkout views: migration of legacy (pre-view) index sets and the
//! explicit `aft cache prune-legacy` command.
//!
//! The import is exercised through real process kills at every durable
//! boundary, concurrent first binds in threads and in another process, an
//! older binary rewriting its cache mid-import, and an incompatible cache.
//! The prune runs in an isolated storage root against a scripted process
//! census, which lets two races be placed exactly: an opener between the
//! census and the rename, and a process arriving between the rename and the
//! delete.
//!
//! Child processes re-run this test binary with `--ignored --exact
//! per_checkout_7::per_checkout_7_child`; `PC7_*` variables name the scenario
//! and the step to park at. A parked child writes a ready file and waits for a
//! go file, or is killed.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use aft::blob_store::v2::{FamilyPlane, FamilyStoreReader, TrigramPolicy};
use aft::migration::per_checkout::{
    legacy_semantic_path, legacy_trigram_path, run_import, Artifact, ImportLedger, ImportObserver,
    ImportOutcome, ImportRequest, ImportState, ImportStep,
};
#[cfg(unix)]
use aft::migration::prune_legacy::PruneStep;
use aft::migration::prune_legacy::{
    prune_legacy, CensusFinding, ProcessCensus, PruneExit, PruneObserver, PruneOptions,
    OPERATOR_CONTRACT, PRUNE_DIR_PREFIX,
};
use aft::search_index::SearchIndex;
use aft::semantic_index::{EmbedTextCaps, SemanticIndex, SemanticIndexFingerprint};
use aft::views::first_load::{ConfiguredMembershipWalker, MembershipWalker};
use aft::views::manifest_v2::{GenerationName, Producers};
use aft::views::registry::FamilyRegistry;
use aft::views::semantic::{FillBudget, SemanticProducer};
use aft::views::semantic_runtime::{CheckoutSemantic, UNREGISTERED_PRODUCER};
use aft::views::RelPath;
use tempfile::tempdir;

const KEY: &str = "family7";
const SCOPE: &str = "scope7";

/// The `aft` binary. Nextest remaps archive binaries into its extraction
/// directory, so its runtime variables win over Cargo's compile-time path.
fn aft_binary() -> PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}
const CHILD_TEST: &str = "per_checkout_7::per_checkout_7_child";
const POLICY: TrigramPolicy = TrigramPolicy {
    max_file_size: 1 << 20,
};
const FIFTEEN_DAYS: Duration = Duration::from_secs(15 * 24 * 60 * 60);
const THIRTEEN_DAYS: Duration = Duration::from_secs(13 * 24 * 60 * 60);

const FILES: [(&str, &str); 3] = [
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
];

// ---------------------------------------------------------------------------
// Fixtures

/// A deterministic model: equal texts get equal vectors.
fn vector(text: &str) -> Vec<f32> {
    blake3::hash(text.as_bytes()).as_bytes()[..16]
        .iter()
        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
        .collect()
}

fn embed(texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
    Ok(texts.iter().map(|text| vector(text)).collect())
}

fn fingerprint() -> SemanticIndexFingerprint {
    SemanticIndexFingerprint {
        backend: "test".to_owned(),
        model: "deterministic".to_owned(),
        base_url: "none".to_owned(),
        dimension: 16,
        chunking_version: 2,
        embed_text_caps: EmbedTextCaps::default(),
        ..SemanticIndexFingerprint::default()
    }
}

fn producer() -> SemanticProducer {
    let fingerprint = fingerprint();
    SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps)
}

fn checkout(base: &Path) -> PathBuf {
    let root = base.join("checkout");
    for (path, text) in FILES {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fs::canonicalize(root).unwrap()
}

fn members(root: &Path) -> Vec<PathBuf> {
    ConfiguredMembershipWalker.files(root).unwrap()
}

/// Writes the legacy set an older binary leaves for `root` under `key`.
fn write_legacy_set(storage: &Path, root: &Path, key: &str, max_file_size: u64) {
    let mut index = SearchIndex::build_with_limit(root, max_file_size);
    assert!(index.write_to_disk(&storage.join("index").join(key), None));
    let mut semantic = SemanticIndex::build(root, &members(root), &mut embed, 64).unwrap();
    semantic.set_fingerprint(fingerprint());
    assert!(semantic.write_to_disk(storage, key));
    let callgraph = storage.join("callgraph").join(key);
    fs::create_dir_all(&callgraph).unwrap();
    fs::write(callgraph.join("store.sqlite"), b"legacy callgraph bytes").unwrap();
    let owners = storage.join("artifact-owners").join(key);
    fs::create_dir_all(&owners).unwrap();
    fs::write(owners.join("owner.json"), b"{\"pid\":1}").unwrap();
}

/// A binder that registers the trigram and semantic planes.
fn full_request(storage: &Path, root: &Path) -> ImportRequest {
    let producer = producer();
    ImportRequest {
        storage: storage.to_path_buf(),
        family: KEY.to_owned(),
        legacy_key: KEY.to_owned(),
        scope: SCOPE.to_owned(),
        root: root.to_path_buf(),
        producers: Producers {
            trigram: POLICY.fingerprint_hex(),
            semantic: Some(producer.id()),
            callgraph: "callgraph-test".to_owned(),
        },
        trigram_policy: Some(POLICY),
        semantic: Some(producer),
    }
}

/// The production views-on semantic lane: semantic only.
fn lane_request(storage: &Path, root: &Path, key: &str) -> ImportRequest {
    let producer = producer();
    ImportRequest {
        storage: storage.to_path_buf(),
        family: key.to_owned(),
        legacy_key: key.to_owned(),
        scope: SCOPE.to_owned(),
        root: root.to_path_buf(),
        producers: Producers {
            trigram: UNREGISTERED_PRODUCER.to_owned(),
            semantic: Some(producer.id()),
            callgraph: UNREGISTERED_PRODUCER.to_owned(),
        },
        trigram_policy: None,
        semantic: Some(producer),
    }
}

/// Every file under `root` with its bytes, relative to `root`.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    if !root.exists() {
        return files;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    files
}

fn legacy_tree(storage: &Path, key: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut all = BTreeMap::new();
    for kind in ["index", "semantic", "callgraph", "artifact-owners"] {
        for (path, bytes) in tree(&storage.join(kind).join(key)) {
            all.insert(Path::new(kind).join(key).join(path), bytes);
        }
    }
    all
}

/// Asserts two inventories are identical, naming the paths that differ.
fn assert_same(
    after: &BTreeMap<PathBuf, Vec<u8>>,
    before: &BTreeMap<PathBuf, Vec<u8>>,
    context: &str,
) {
    let differing = after
        .keys()
        .chain(before.keys())
        .filter(|path| after.get(*path) != before.get(*path))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        differing.is_empty(),
        "{context}: changed paths {differing:?}"
    );
}

fn current_generation(storage: &Path, root: &Path) -> Option<String> {
    FamilyRegistry::open(storage, KEY)
        .unwrap()
        .register_view(SCOPE, root)
        .unwrap()
        .view_store()
        .unwrap()
        .current_generation()
        .unwrap()
}

fn content_of(generation: &str) -> [u8; 32] {
    *GenerationName::parse(generation).unwrap().content()
}

fn rows(storage: &Path) -> BTreeMap<Artifact, (ImportState, u64)> {
    ImportLedger::open_existing(storage, KEY)
        .unwrap()
        .unwrap()
        .rows()
        .unwrap()
        .into_iter()
        .map(|row| (row.artifact, (row.state, row.attempt)))
        .collect()
}

// ---------------------------------------------------------------------------
// Child process plumbing

fn spawn_child(scenario: &str, base: &Path, step: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("PC7_SCENARIO", scenario)
        .env("PC7_BASE", base)
        .env("PC7_STEP", step)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_file(child: &mut Child, path: &Path) {
    let started = Instant::now();
    while !path.is_file() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before writing {path:?}: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "timed out waiting for {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "child did not exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn kill(child: &mut Child) {
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
}

fn step_name(step: ImportStep) -> String {
    format!("{step:?}")
}

/// Parks the process at one named step: writes the ready file, then waits
/// for the go file, which a parent that kills the child never writes.
struct ParkAt {
    step: String,
    base: PathBuf,
}

impl ImportObserver for ParkAt {
    fn reached(&self, step: ImportStep) {
        if step_name(step) == self.step {
            fs::write(self.base.join("ready"), b"ready").unwrap();
            while !self.base.join("go").is_file() {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[test]
#[ignore = "child process of the per_checkout_7 tests"]
fn per_checkout_7_child() {
    let Ok(scenario) = std::env::var("PC7_SCENARIO") else {
        return;
    };
    let base = PathBuf::from(std::env::var("PC7_BASE").unwrap());
    let step = std::env::var("PC7_STEP").unwrap();
    let storage = base.join("storage");
    let root = fs::canonicalize(base.join("checkout")).unwrap();
    match scenario.as_str() {
        "import" => {
            let observer = ParkAt { step, base };
            run_import(&full_request(&storage, &root), Some(&observer)).unwrap();
        }
        other => panic!("unknown scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// Import

/// One uninterrupted import in its own storage: the reference every
/// interrupted import must reach.
fn reference_generation(root: &Path) -> [u8; 32] {
    let storage = tempdir().unwrap();
    write_legacy_set(storage.path(), root, KEY, POLICY.max_file_size);
    let report = run_import(&full_request(storage.path(), root), None).unwrap();
    assert_eq!(report.outcome, ImportOutcome::Completed);
    assert!(report.trigram_files >= FILES.len(), "{report:?}");
    assert_eq!(report.semantic_files, FILES.len(), "{report:?}");
    content_of(report.published.as_deref().unwrap())
}

/// The step a resumed attempt commits first for the artifact a kill stopped.
fn next_step(killed: ImportStep) -> Option<ImportStep> {
    Some(match killed {
        ImportStep::Claimed(artifact) => ImportStep::Staged(artifact),
        ImportStep::Staged(artifact) => ImportStep::Validated(artifact),
        ImportStep::Validated(artifact) => ImportStep::Registered(artifact),
        ImportStep::Registered(_) | ImportStep::Rejected(_) => ImportStep::Published,
        ImportStep::Published | ImportStep::Done => return None,
    })
}

/// A process killed right after any durable boundary leaves a state the
/// next attempt resumes: it takes the claim over (attempt 2), repeats no
/// step the killed process committed, and publishes exactly the generation
/// an uninterrupted import publishes. The legacy set is never written.
#[test]
fn every_import_transition_survives_a_kill_and_restart() {
    let reference_base = tempdir().unwrap();
    let reference = reference_generation(&checkout(reference_base.path()));
    for killed in [
        ImportStep::Claimed(Artifact::Trigram),
        ImportStep::Staged(Artifact::Trigram),
        ImportStep::Validated(Artifact::Trigram),
        ImportStep::Registered(Artifact::Trigram),
        ImportStep::Staged(Artifact::Semantic),
        ImportStep::Validated(Artifact::Semantic),
        ImportStep::Registered(Artifact::Semantic),
        ImportStep::Rejected(Artifact::Callgraph),
        ImportStep::Published,
    ] {
        let base = tempdir().unwrap();
        let root = checkout(base.path());
        let storage = base.path().join("storage");
        write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
        let legacy = legacy_tree(&storage, KEY);
        let mut child = spawn_child("import", base.path(), &step_name(killed));
        wait_for_file(&mut child, &base.path().join("ready"));
        kill(&mut child);

        let report = run_import(&full_request(&storage, &root), None).unwrap();
        assert_eq!(report.outcome, ImportOutcome::Completed, "{killed:?}");
        assert!(
            !report.steps.contains(&killed),
            "{killed:?}: a committed step ran again: {:?}",
            report.steps
        );
        if let Some(next) = next_step(killed) {
            assert!(
                report.steps.contains(&next),
                "{killed:?}: resumed without {next:?}: {:?}",
                report.steps
            );
        }
        let rows = rows(&storage);
        assert_eq!(rows[&Artifact::Trigram].0, ImportState::Done, "{killed:?}");
        assert_eq!(rows[&Artifact::Semantic].0, ImportState::Done, "{killed:?}");
        assert_eq!(rows[&Artifact::Callgraph].0, ImportState::Rejected);
        // The killed process had claimed the trigram row first, the other
        // two only after that first boundary; a row it had already finished
        // keeps its single attempt, every other row it held is taken over.
        let attempts = rows
            .iter()
            .map(|(artifact, (_, attempt))| (*artifact, *attempt))
            .collect::<BTreeMap<_, _>>();
        let expected = match killed {
            ImportStep::Claimed(_) => [
                (Artifact::Trigram, 2),
                (Artifact::Semantic, 1),
                (Artifact::Callgraph, 1),
            ],
            ImportStep::Rejected(_) | ImportStep::Published => [
                (Artifact::Trigram, 2),
                (Artifact::Semantic, 2),
                (Artifact::Callgraph, 1),
            ],
            _ => [
                (Artifact::Trigram, 2),
                (Artifact::Semantic, 2),
                (Artifact::Callgraph, 2),
            ],
        }
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        assert_eq!(attempts, expected, "{killed:?}");
        let generation = current_generation(&storage, &root).expect("published generation");
        assert_eq!(content_of(&generation), reference, "{killed:?}");
        assert_eq!(legacy_tree(&storage, KEY), legacy, "{killed:?}");
        assert!(
            tree(&aft::migration::per_checkout::ledger_dir(&storage, KEY).join("staged"))
                .is_empty(),
            "{killed:?}: staged bundles are removed once done"
        );
    }
}

/// A second import of a finished set does nothing and changes no byte.
#[test]
fn a_repeated_import_is_a_no_op() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let first = run_import(&full_request(&storage, &root), None).unwrap();
    assert_eq!(first.outcome, ImportOutcome::Completed);
    let before = tree(&storage);
    let again = run_import(&full_request(&storage, &root), None).unwrap();
    assert_eq!(again.outcome, ImportOutcome::AlreadyComplete);
    assert!(again.steps.is_empty());
    assert_eq!(tree(&storage), before);
}

/// Concurrent first binds in one process: exactly one converts the set.
#[test]
fn concurrent_first_binds_in_one_process_import_once() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let barrier = Arc::new(Barrier::new(4));
    let reports = (0..4)
        .map(|_| {
            let (barrier, storage, root) = (Arc::clone(&barrier), storage.clone(), root.clone());
            thread::spawn(move || {
                barrier.wait();
                run_import(&full_request(&storage, &root), None).unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let converters = reports
        .iter()
        .filter(|report| {
            report
                .steps
                .contains(&ImportStep::Staged(Artifact::Trigram))
        })
        .count();
    assert_eq!(converters, 1, "{reports:?}");
    assert!(rows(&storage).values().all(|(_, attempt)| *attempt == 1));
}

/// Concurrent first binds in two processes: while one process holds the
/// import, the other is told to wait (it would report the planes as
/// migrating) and converts nothing; afterwards it finds the import done.
#[test]
fn a_bind_in_another_process_waits_for_the_live_importer() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let mut child = spawn_child(
        "import",
        base.path(),
        &step_name(ImportStep::Staged(Artifact::Trigram)),
    );
    wait_for_file(&mut child, &base.path().join("ready"));
    let busy = run_import(&full_request(&storage, &root), None).unwrap();
    assert_eq!(
        busy.outcome,
        ImportOutcome::Busy {
            owner_pid: child.id()
        }
    );
    assert!(busy.steps.is_empty());
    fs::write(base.path().join("go"), b"go").unwrap();
    assert!(wait_for_exit(&mut child).success());
    let after = run_import(&full_request(&storage, &root), None).unwrap();
    assert_eq!(after.outcome, ImportOutcome::AlreadyComplete);
    assert!(rows(&storage).values().all(|(_, attempt)| *attempt == 1));
}

struct RewriteAt(ImportStep, PathBuf, PathBuf);

impl ImportObserver for RewriteAt {
    fn reached(&self, step: ImportStep) {
        if step == self.0 {
            // An older daemon that still serves this root rewrites its cache.
            fs::write(self.2.join("src/delta.rs"), "fn delta() {}\n").unwrap();
            let mut index = SearchIndex::build_with_limit(&self.2, POLICY.max_file_size);
            assert!(index.write_to_disk(&self.1, None));
        }
    }
}

/// An older views-on binary keeps serving and rewriting its legacy cache
/// while the import runs. The import notices that the converted bytes no
/// longer exist, rejects the trigram row instead of publishing stale
/// postings, and leaves the older binary's files usable.
#[test]
fn an_older_binary_rewriting_its_cache_mid_import_forces_a_rebuild() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let cache_dir = storage.join("index").join(KEY);
    let observer = RewriteAt(
        ImportStep::Staged(Artifact::Trigram),
        cache_dir.clone(),
        root.clone(),
    );
    let report = run_import(&full_request(&storage, &root), Some(&observer)).unwrap();
    assert_eq!(report.outcome, ImportOutcome::Completed);
    let ledger = ImportLedger::open_existing(&storage, KEY).unwrap().unwrap();
    let trigram = ledger
        .rows()
        .unwrap()
        .into_iter()
        .find(|row| row.artifact == Artifact::Trigram)
        .unwrap();
    assert_eq!(trigram.state, ImportState::Rejected);
    assert!(trigram
        .reason
        .unwrap()
        .contains("changed during the import"));
    assert!(SearchIndex::read_from_disk(&cache_dir, &root).is_some());
}

/// A cache built under another size limit is rejected, not relabelled:
/// no trigram payload is stored and the published generation leaves the
/// trigram plane pending for a build from the checkout.
#[test]
fn an_incompatible_cache_is_rebuilt_not_relabelled() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, 4096);
    let report = run_import(&full_request(&storage, &root), None).unwrap();
    assert_eq!(report.outcome, ImportOutcome::Completed);
    assert_eq!(report.trigram_files, 0);
    let row = ImportLedger::open_existing(&storage, KEY)
        .unwrap()
        .unwrap()
        .rows()
        .unwrap()
        .into_iter()
        .find(|row| row.artifact == Artifact::Trigram)
        .unwrap();
    assert_eq!(row.state, ImportState::Rejected);
    assert!(row.reason.unwrap().contains("max_file_size"));
    let trigram = FamilyStoreReader::open_existing(&storage, KEY, FamilyPlane::Trigram).unwrap();
    assert!(trigram.is_none_or(|store| store.usage().unwrap().rows == 0));
    let registration = FamilyRegistry::open(&storage, KEY)
        .unwrap()
        .register_view(SCOPE, &root)
        .unwrap();
    let store = registration.view_store().unwrap();
    let manifest = store
        .load_manifest_v2(&store.current_generation().unwrap().unwrap())
        .unwrap();
    assert!(manifest.segment_id().is_none());
    for (_, entry) in manifest.entries() {
        assert!(entry
            .plane_state(FamilyPlane::Trigram)
            .is_some_and(|state| state.is_pending()));
    }
}

/// After the import, the previous binary still finds and loads its own
/// legacy caches: rollback is stopping the daemons and reinstalling it.
#[test]
fn offline_rollback_finds_the_retained_legacy_set() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let legacy = legacy_tree(&storage, KEY);
    run_import(&full_request(&storage, &root), None).unwrap();
    assert_eq!(legacy_tree(&storage, KEY), legacy);
    assert!(legacy_trigram_path(&storage, KEY).is_file());
    assert!(legacy_semantic_path(&storage, KEY).is_file());
    let index = SearchIndex::read_from_disk(&storage.join("index").join(KEY), &root)
        .expect("the previous binary's trigram cache still loads");
    assert_eq!(index.file_count(), FILES.len());
}

type Row = (String, String, u32, u32);

fn result_rows(root: &Path, results: &[aft::semantic_index::SemanticResult]) -> Vec<Row> {
    let mut rows = results
        .iter()
        .map(|result| {
            (
                result
                    .file
                    .strip_prefix(root)
                    .unwrap_or(&result.file)
                    .to_string_lossy()
                    .into_owned(),
                result.name.clone(),
                result.start_line,
                result.score.to_bits(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

/// Loads and fills the production semantic lane runtime for `root`, and
/// returns how many texts it embedded and its answer to `query`.
fn lane_fill(storage: &Path, root: &Path, key: &str, query: &str) -> (usize, Vec<Row>) {
    let lane = CheckoutSemantic::new(storage, key, SCOPE, root, producer(), Weak::new()).unwrap();
    lane.load().unwrap();
    let mut embedded = 0;
    let mut counting = |texts: Vec<String>| {
        embedded += texts.len();
        embed(texts)
    };
    let report = lane.refresh(FillBudget::default(), &mut counting).unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    let answer = lane.search(&vector(query), 100, &|_| true).unwrap();
    assert!(answer.complete(), "{:?}", answer.pending);
    (embedded, result_rows(root, &answer.results))
}

/// Migrated planes equal a cold rebuild. Semantic: after the import the
/// production lane answers exactly like a cold `SemanticIndex` build of the
/// same files, without embedding a single text (without the import it
/// embeds them all). Trigram: the imported segment is byte-identical to one
/// built from the checkout files.
#[test]
fn migrated_planes_equal_a_cold_rebuild() {
    let query = "sum the alpha values";
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let cold = SemanticIndex::build(&root, &members(&root), &mut embed, 64).unwrap();
    let cold_rows = result_rows(&root, &cold.search(&vector(query), 100));
    assert!(!cold_rows.is_empty());

    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let report = run_import(&lane_request(&storage, &root, KEY), None).unwrap();
    assert_eq!(report.semantic_files, FILES.len(), "{report:?}");
    let (embedded, rows) = lane_fill(&storage, &root, KEY, query);
    assert_eq!(
        embedded, 0,
        "imported vectors are reused, never re-embedded"
    );
    assert_eq!(rows, cold_rows);

    let control = base.path().join("control");
    let (embedded, rows) = lane_fill(&control, &root, KEY, query);
    assert!(embedded > 0, "without an import the lane embeds the files");
    assert_eq!(rows, cold_rows);

    let full = base.path().join("full");
    write_legacy_set(&full, &root, KEY, POLICY.max_file_size);
    let report = run_import(&full_request(&full, &root), None).unwrap();
    let rows = ImportLedger::open_existing(&full, KEY)
        .unwrap()
        .unwrap()
        .rows()
        .unwrap();
    let segment = rows
        .iter()
        .find(|row| row.artifact == Artifact::Trigram)
        .and_then(|row| row.result.clone())
        .expect("imported segment id");
    let segment_id = aft::blob_store::v2::parse_hex32(&segment).unwrap();
    let imported =
        fs::read(aft::blob_store::v2::segment_path(&full, KEY, &segment_id).unwrap()).unwrap();
    let rel_paths = FILES
        .iter()
        .map(|(path, _)| RelPath::new(path.as_bytes().to_vec()).unwrap())
        .collect::<Vec<_>>();
    let cold_segment =
        aft::views::segment_store::build_from_files(&root, &rel_paths, &POLICY).unwrap();
    assert_eq!(cold_segment.id, segment_id);
    assert_eq!(imported, cold_segment.bytes);
    assert!(report.published.is_some());
}

// ---------------------------------------------------------------------------
// prune-legacy

/// A process census that answers from a script; once the script runs out
/// it keeps returning its last answer.
#[derive(Clone, Default)]
struct Script(Arc<Mutex<VecDeque<Result<Vec<CensusFinding>, String>>>>);

impl Script {
    fn clean() -> Self {
        Self::default()
    }

    fn then(self, answer: Result<Vec<CensusFinding>, String>) -> Self {
        self.0.lock().unwrap().push_back(answer);
        self
    }

    #[cfg(unix)]
    fn set(&self, answer: Result<Vec<CensusFinding>, String>) {
        let mut script = self.0.lock().unwrap();
        script.clear();
        script.push_back(answer);
    }
}

impl ProcessCensus for Script {
    fn take(&self) -> Result<Vec<CensusFinding>, String> {
        let mut script = self.0.lock().unwrap();
        match script.len() {
            0 => Ok(Vec::new()),
            1 => script.front().cloned().unwrap(),
            _ => script.pop_front().unwrap(),
        }
    }
}

fn holder(pid: u32) -> CensusFinding {
    CensusFinding::Aft {
        pid,
        daemon: true,
        command: "aft --subc /tmp/conn.json".to_owned(),
    }
}

/// An isolated storage root with: legacy set `family7` imported through the
/// production lane, legacy set `familyb` never imported, the v2 stores, and
/// unrelated families (the embedding model cache, backups).
fn prune_fixture(base: &Path) -> PathBuf {
    let root = checkout(base);
    let storage = base.join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    write_legacy_set(&storage, &root, "familyb", POLICY.max_file_size);
    let report = run_import(&lane_request(&storage, &root, KEY), None).unwrap();
    assert_eq!(report.outcome, ImportOutcome::Completed);
    for (path, bytes) in [
        ("semantic/models/model.onnx", &b"model"[..]),
        ("backups/session/one.bak", &b"backup"[..]),
    ] {
        let path = storage.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    storage
}

fn prune(
    storage: &Path,
    yes: bool,
    after: Duration,
    census: &dyn ProcessCensus,
    observer: Option<&dyn PruneObserver>,
) -> (PruneExit, String) {
    let mut out = Vec::new();
    let exit = prune_legacy(
        storage,
        &PruneOptions {
            yes,
            now: SystemTime::now() + after,
        },
        census,
        observer,
        &mut out,
    )
    .unwrap();
    (exit, String::from_utf8(out).unwrap())
}

/// Everything except the imported legacy set.
#[cfg(unix)]
fn without_set(storage: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let set = legacy_tree(storage, KEY);
    tree(storage)
        .into_iter()
        .filter(|(path, _)| !set.contains_key(path))
        .collect()
}

fn prune_areas(storage: &Path) -> Vec<PathBuf> {
    fs::read_dir(storage)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(PRUNE_DIR_PREFIX)
        })
        .collect()
}

/// The positive case: the whole imported legacy set is removed once its
/// retention passed, and every other byte (the v2 stores, the ledger, the
/// never-imported set, the model cache, backups) is identical.
///
/// Unix only: directories are renamed aside only where `rename(2)` is atomic.
/// Windows skips every set, which the next test asserts.
#[cfg(unix)]
#[test]
fn prune_removes_the_whole_imported_set_and_nothing_else() {
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let others = without_set(&storage);
    assert!(!legacy_tree(&storage, KEY).is_empty());
    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &Script::clean(), None);
    assert_eq!(exit, PruneExit::Clean, "{out}");
    for kind in ["index", "semantic", "callgraph", "artifact-owners"] {
        assert!(!storage.join(kind).join(KEY).exists(), "{kind}: {out}");
    }
    assert_same(&tree(&storage), &others, &out);
    assert!(prune_areas(&storage).is_empty());
}

/// On Windows a directory cannot be renamed aside atomically, so every
/// eligible set is skipped and reported and nothing is deleted.
#[cfg(windows)]
#[test]
fn windows_prune_skips_every_set_and_deletes_nothing() {
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let before = tree(&storage);
    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &Script::clean(), None);
    assert_eq!(exit, PruneExit::Skipped, "{out}");
    assert!(
        out.contains(&format!(
            "Skipped {KEY}: directories cannot be renamed atomically"
        )),
        "{out}"
    );
    assert!(out.contains("Nothing was deleted"), "{out}");
    assert_same(&tree(&storage), &before, &out);
    assert!(prune_areas(&storage).is_empty(), "{out}");
}

/// Every refusal and every ineligible set retains all bytes.
#[test]
fn prune_negatives_retain_every_byte() {
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let before = tree(&storage);
    let unclassifiable = CensusFinding::Unclassifiable {
        pid: 9,
        command: "aft-bridge".to_owned(),
        reason: "lookalike".to_owned(),
    };
    for (name, census, after, expected, needle) in [
        (
            "live holder",
            Script::clean().then(Ok(vec![holder(42)])),
            FIFTEEN_DAYS,
            PruneExit::Refused,
            "live AFT daemon pid 42",
        ),
        (
            "census error",
            Script::clean().then(Err("ps failed".to_owned())),
            FIFTEEN_DAYS,
            PruneExit::Refused,
            "the process census failed: ps failed",
        ),
        (
            "unclassifiable process",
            Script::clean().then(Ok(vec![unclassifiable])),
            FIFTEEN_DAYS,
            PruneExit::Refused,
            "unclassifiable process pid 9",
        ),
        (
            "retention not over",
            Script::clean(),
            THIRTEEN_DAYS,
            PruneExit::Clean,
            "retained for rollback until",
        ),
    ] {
        let (exit, out) = prune(&storage, true, after, &census, None);
        assert_eq!(exit, expected, "{name}: {out}");
        assert!(out.contains(needle), "{name}: {out}");
        assert_same(&tree(&storage), &before, name);
    }
    let (exit, out) = prune(&storage, false, FIFTEEN_DAYS, &Script::clean(), None);
    assert_eq!(exit, PruneExit::Clean);
    assert!(out.contains("Dry run: 1 legacy set(s)"), "{out}");
    assert!(out.contains("familyb"), "{out}");
    assert!(out.contains("kept: never imported"), "{out}");
    assert_same(&tree(&storage), &before, "dry run");
}

/// Import completion is required even after the retention window, and
/// even with `--yes`.
#[test]
fn an_unfinished_import_is_never_pruned() {
    let base = tempdir().unwrap();
    let root = checkout(base.path());
    let storage = base.path().join("storage");
    write_legacy_set(&storage, &root, KEY, POLICY.max_file_size);
    let mut child = spawn_child(
        "import",
        base.path(),
        &step_name(ImportStep::Staged(Artifact::Trigram)),
    );
    wait_for_file(&mut child, &base.path().join("ready"));
    kill(&mut child);
    let before = tree(&storage);
    let (exit, out) = prune(
        &storage,
        true,
        Duration::from_secs(400 * 24 * 60 * 60),
        &Script::clean(),
        None,
    );
    assert_eq!(exit, PruneExit::Clean, "{out}");
    assert!(out.contains("kept: its import has not completed"), "{out}");
    assert_same(&tree(&storage), &before, "unfinished import");
}

#[cfg(unix)]
struct OnStep<F: Fn(PruneStep<'_>)>(F);

#[cfg(unix)]
impl<F: Fn(PruneStep<'_>)> PruneObserver for OnStep<F> {
    fn reached(&self, step: PruneStep<'_>) {
        (self.0)(step)
    }
}

#[cfg(unix)]
fn moved_tree(area: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    tree(&area.join(KEY))
        .into_iter()
        .map(|(path, bytes)| {
            let mut parts = path.components();
            let kind = parts.next().unwrap().as_os_str().to_owned();
            (Path::new(&kind).join(KEY).join(parts.as_path()), bytes)
        })
        .collect()
}

/// A legacy opener that starts after the census: the descriptor it opened
/// before the rename still reads the moved inode, an open by path after the
/// rename finds no set, the second census sees the opener's process, and
/// the moved set is kept intact with a non-zero exit. A later clean run
/// removes the leftover.
///
/// Unix only, like every test that needs a set renamed aside.
#[cfg(unix)]
#[test]
fn an_opener_between_census_and_rename_keeps_the_moved_set() {
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let legacy = legacy_tree(&storage, KEY);
    let others = without_set(&storage);
    let cache = legacy_trigram_path(&storage, KEY);
    let census = Script::clean();
    let opened = Mutex::new(None);
    let seen_absent = Mutex::new(false);
    let observer = OnStep(|step| match step {
        PruneStep::CensusClean => {
            *opened.lock().unwrap() = Some(fs::File::open(&cache).unwrap());
            census.set(Ok(vec![holder(77)]));
        }
        PruneStep::SetMoved(key) if key == KEY => {
            *seen_absent.lock().unwrap() = fs::File::open(&cache)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        }
        _ => {}
    });
    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &census, Some(&observer));
    assert_eq!(exit, PruneExit::ProcessAppeared, "{out}");
    assert!(
        *seen_absent.lock().unwrap(),
        "an open after the rename finds no set"
    );
    let mut held = opened.lock().unwrap().take().unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut held, &mut bytes).unwrap();
    assert_eq!(
        bytes,
        legacy[&Path::new("index").join(KEY).join("cache.bin")],
        "the held descriptor still reads the moved file"
    );
    assert!(!storage.join("index").join(KEY).exists());
    let areas = prune_areas(&storage);
    assert_eq!(areas.len(), 1);
    assert!(out.contains(&areas[0].display().to_string()), "{out}");
    assert_eq!(
        moved_tree(&areas[0]),
        legacy,
        "the moved inventory is intact"
    );

    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &Script::clean(), None);
    assert_eq!(exit, PruneExit::Clean, "{out}");
    assert!(prune_areas(&storage).is_empty());
    assert_eq!(tree(&storage), others);
}

/// A process that arrives between the rename and the delete: the moved set
/// is kept and reported, the exit is non-zero, and a later clean run
/// removes it.
///
/// Unix only, like every test that needs a set renamed aside.
#[cfg(unix)]
#[test]
fn a_process_arriving_before_the_delete_keeps_the_moved_set() {
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let legacy = legacy_tree(&storage, KEY);
    let others = without_set(&storage);
    let census = Script::clean();
    let observer = OnStep(|step| {
        if step == PruneStep::BeforeRecensus {
            census.set(Ok(vec![holder(88)]));
        }
    });
    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &census, Some(&observer));
    assert_eq!(exit, PruneExit::ProcessAppeared, "{out}");
    assert!(out.contains("live AFT daemon pid 88"), "{out}");
    let areas = prune_areas(&storage);
    assert_eq!(areas.len(), 1);
    assert!(out.contains(&areas[0].display().to_string()), "{out}");
    assert!(!storage.join("semantic").join(KEY).exists());
    assert_eq!(moved_tree(&areas[0]), legacy);

    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &Script::clean(), None);
    assert_eq!(exit, PruneExit::Clean, "{out}");
    assert!(out.contains("Removed leftover"), "{out}");
    assert!(prune_areas(&storage).is_empty());
    assert_eq!(tree(&storage), others);
}

/// A rename the filesystem refuses skips the set and deletes nothing in
/// place; directories already moved for it are moved back.
#[cfg(unix)]
#[test]
fn a_refused_rename_skips_the_set_and_deletes_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let before = tree(&storage);
    let semantic_root = storage.join("semantic");
    fs::set_permissions(&semantic_root, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = fs::write(semantic_root.join("probe"), b"x").is_ok();
    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &Script::clean(), None);
    fs::set_permissions(&semantic_root, fs::Permissions::from_mode(0o755)).unwrap();
    if probe {
        eprintln!("skipped: this user can write to a read-only directory");
        return;
    }
    assert_eq!(exit, PruneExit::Skipped, "{out}");
    assert!(out.contains("Nothing was deleted"), "{out}");
    assert_same(&tree(&storage), &before, &out);
}

/// The help and the dry run carry the operator contract word for word.
#[test]
fn help_and_dry_run_state_the_operator_contract() {
    let help = Command::new(aft_binary())
        .args(["cache", "prune-legacy", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains(OPERATOR_CONTRACT), "{text}");
    assert!(text.contains(
        "Stop every AFT process (the daemon, OpenCode and Pi hosts) before running this. The check below refuses when it sees one, but it can't stop a process that starts after the check."
    ));

    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let before = tree(&storage);
    let dry = Command::new(aft_binary())
        .args(["cache", "prune-legacy"])
        .env("AFT_STORAGE_DIR", &storage)
        .output()
        .unwrap();
    let text = String::from_utf8(dry.stdout).unwrap();
    assert!(text.contains(OPERATOR_CONTRACT), "{text}");
    assert!(text.contains(KEY) && text.contains("familyb"), "{text}");
    // Other test processes may be running `aft`, which the real census
    // refuses on; either way the dry run lists and deletes nothing.
    assert!(
        matches!(dry.status.code(), Some(0 | 3)),
        "{:?}: {text}",
        dry.status
    );
    assert_eq!(tree(&storage), before);
}

/// The real census, narrowed to processes this test started, so the test
/// can tell whether its own process was classified on a host that also runs
/// other AFT processes (this test suite, or the operator's daemon).
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct OnlyPids(Vec<u32>);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ProcessCensus for OnlyPids {
    fn take(&self) -> Result<Vec<CensusFinding>, String> {
        Ok(aft::migration::prune_legacy::SystemCensus
            .take()?
            .into_iter()
            .filter(|finding| match finding {
                CensusFinding::Aft { pid, .. } | CensusFinding::Unclassifiable { pid, .. } => {
                    self.0.contains(pid)
                }
            })
            .collect())
    }
}

/// Kills a child process when the test ends, however it ends.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct KillOnDrop(Child);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An `aft` executable installed under a path with spaces (as in an app
/// bundle or `Application Support`) and running is a live AFT process: the
/// census classifies it and the prune refuses, deleting nothing.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_census_sees_an_aft_executable_under_a_path_with_spaces() {
    let base = tempdir().unwrap();
    let storage = prune_fixture(base.path());
    let before = tree(&storage);
    let dir = base.path().join("with space").join("Application Support");
    fs::create_dir_all(&dir).unwrap();
    let executable = dir.join("aft");
    fs::copy(aft_binary(), &executable).unwrap();
    // With stdin held open the standalone server keeps running.
    let child = KillOnDrop(
        Command::new(&executable)
            .env("AFT_STORAGE_DIR", base.path().join("child-storage"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    let census = OnlyPids(vec![pid]);
    let findings = census.take().unwrap();
    let (exit, out) = prune(&storage, true, FIFTEEN_DAYS, &census, None);
    drop(child);
    assert_eq!(
        exit,
        PruneExit::Refused,
        "the census missed the AFT process at {executable:?} ({findings:?}), so the prune ran: {out}"
    );
    assert!(
        matches!(findings.as_slice(), [CensusFinding::Aft { pid: found, .. }] if *found == pid),
        "{findings:?}"
    );
    assert!(
        out.contains(&format!("live AFT process pid {pid}")),
        "{out}"
    );
    assert_same(&tree(&storage), &before, &out);
}

// ---------------------------------------------------------------------------
// The production views-on configure path

mod production {
    use super::*;
    use aft::config::Config;
    use aft::context::{AppContext, SemanticIndexStatus};
    use aft::parser::TreeSitterProvider;
    use aft::protocol::RawRequest;
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const DEADLINE: Duration = Duration::from_secs(120);
    const FINGERPRINT_PROBE: &str = "semantic index fingerprint probe";
    const MODEL: &str = "per-checkout-7-mock";

    /// Deterministic embeddings over HTTP that count every text other than
    /// the fingerprint probe a model sends once when it starts.
    struct MockEmbedder {
        base_url: String,
        addr: SocketAddr,
        running: Arc<AtomicBool>,
        texts: Arc<AtomicUsize>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MockEmbedder {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let running = Arc::new(AtomicBool::new(true));
            let texts = Arc::new(AtomicUsize::new(0));
            let (thread_running, thread_texts) = (Arc::clone(&running), Arc::clone(&texts));
            let handle = thread::spawn(move || {
                while thread_running.load(Ordering::SeqCst) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    let texts = Arc::clone(&thread_texts);
                    thread::spawn(move || {
                        let _ = serve(&mut stream, &texts);
                    });
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                addr,
                running,
                texts,
                handle: Some(handle),
            }
        }

        fn texts(&self) -> usize {
            self.texts.load(Ordering::SeqCst)
        }
    }

    impl Drop for MockEmbedder {
        fn drop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn mock_vector(text: &str) -> Vec<f32> {
        blake3::hash(text.as_bytes()).as_bytes()[..8]
            .iter()
            .map(|byte| f32::from(*byte) / 255.0 - 0.5)
            .collect()
    }

    fn serve(stream: &mut TcpStream, texts: &AtomicUsize) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut body_start = None;
        let mut length = 0usize;
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if body_start.is_none() {
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    body_start = Some(end + 4);
                    for line in String::from_utf8_lossy(&bytes[..end]).lines() {
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                }
            }
            if body_start.is_some_and(|start| bytes.len() >= start + length) {
                break;
            }
        }
        let body = body_start
            .and_then(|start| bytes.get(start..start + length))
            .and_then(|body| serde_json::from_slice::<Value>(body).ok())
            .unwrap_or_else(|| json!({ "input": [] }));
        let inputs = match &body["input"] {
            Value::Array(values) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            Value::String(value) => vec![value.clone()],
            _ => Vec::new(),
        };
        texts.fetch_add(
            inputs
                .iter()
                .filter(|input| *input != FINGERPRINT_PROBE)
                .count(),
            Ordering::SeqCst,
        );
        let data = inputs
            .iter()
            .enumerate()
            .map(|(index, input)| json!({ "embedding": mock_vector(input), "index": index }))
            .collect::<Vec<_>>();
        let body = json!({ "data": data }).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn git(root: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
        let output = command.args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository(base: &Path) -> PathBuf {
        let root = fs::canonicalize(base).unwrap().join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "views@example.test"]);
        git(&root, &["config", "user.name", "Views Test"]);
        for (path, text) in FILES {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        root
    }

    fn request(value: Value) -> RawRequest {
        serde_json::from_value(value).unwrap()
    }

    fn send_configure(ctx: &AppContext, root: &Path, storage: &Path, url: &str, views: bool) {
        let configured = aft::commands::configure::handle_configure(
            &request(json!({
                "id": "configure-per-checkout-7",
                "command": "configure",
                "harness": "opencode",
                "project_root": root,
                "storage_dir": storage,
                "config": crate::helpers::user_config(json!({
                    "search_index": true,
                    "semantic_search": true,
                    "callgraph_store": false,
                    "views": { "enabled": views },
                    "semantic": {
                        "backend": "openai_compatible",
                        "model": MODEL,
                        "base_url": url,
                        "timeout_ms": 5_000,
                        "max_batch_size": 64,
                        "max_files": 2_000
                    }
                }))
            })),
            ctx,
        );
        assert!(configured.success, "configure failed: {configured:?}");
    }

    fn drain(ctx: &AppContext) {
        aft::runtime_drain::drain_watcher_events(ctx);
        aft::runtime_drain::drain_search_index_events(ctx);
        aft::runtime_drain::drain_semantic_index_events(ctx);
        aft::runtime_drain::drain_semantic_refresh_events(ctx);
    }

    fn wait_until(ctx: &AppContext, what: &str, done: impl Fn(&AppContext) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            drain(ctx);
            if done(ctx) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what} never happened: status={:?}",
                ctx.semantic_index_status().read().unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn search(ctx: &AppContext, query: &str) -> Value {
        serde_json::to_value(aft::commands::semantic_search::handle_semantic_search(
            &request(json!({ "id": "per-checkout-7-search", "command": "search", "query": query })),
            ctx,
        ))
        .unwrap()
    }

    fn legacy_semantic_files(storage: &Path) -> Vec<PathBuf> {
        fs::read_dir(storage.join("semantic"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path().join("semantic.bin"))
                    .filter(|path| path.is_file())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The real views-on configure path on a root whose previous release
    /// left a semantic index: the import runs on the lane's background
    /// thread, a search while it runs answers at once and names the
    /// migration, and afterwards the lane serves the imported vectors
    /// without embedding a single text again.
    #[test]
    fn views_on_configure_imports_a_legacy_semantic_index_in_the_background() {
        let server = MockEmbedder::start();
        let base = tempdir().unwrap();
        let root = repository(base.path());
        let storage = base.path().join("storage");

        // The previous release: views off, which builds and saves the
        // legacy semantic index.
        {
            let legacy = Arc::new(AppContext::new(
                Box::new(TreeSitterProvider::new()),
                crate::context_storage::isolate(Config::default()),
            ));
            send_configure(&legacy, &root, &storage, &server.base_url, false);
            aft::runtime_drain::drain_deferred_configure_maintenance(&legacy);
            wait_until(&legacy, "the legacy semantic index", |ctx| {
                matches!(
                    &*ctx.semantic_index_status().read().unwrap(),
                    SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
                ) && !legacy_semantic_files(&storage).is_empty()
            });
        }
        let legacy_texts = server.texts();
        assert!(legacy_texts > 0);
        let [semantic_bin] = legacy_semantic_files(&storage).try_into().unwrap();
        let key = semantic_bin
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        // The new release, views on. Holding the context's only
        // maintenance-build slot parks the import at its admission point.
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::context_storage::isolate(Config::default()),
        ));
        ctx.isolate_cold_build_limiter_for_test(1);
        let slot = ctx.take_cold_build_slot_for_test().unwrap();
        let started = Instant::now();
        send_configure(&ctx, &root, &storage, &server.base_url, true);
        aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "configure waited for the import"
        );
        wait_until(&ctx, "the migrating stage", |ctx| {
            matches!(
                &*ctx.semantic_index_status().read().unwrap(),
                SemanticIndexStatus::Building { stage, .. } if stage == "migrating_legacy_index"
            )
        });
        let asked = Instant::now();
        let during = search(&ctx, "evict the oldest cache entry");
        assert!(
            asked.elapsed() < Duration::from_secs(10),
            "a search waited for the import"
        );
        // The gap is named in the readiness the answer carries.
        let reasons = &during["structuredContent"]["plan"]["readiness"]["reasons"];
        assert!(
            reasons.as_array().is_some_and(|reasons| reasons
                .iter()
                .any(|reason| reason == "semantic:building:migrating_legacy_index")),
            "no migrating gap named: {during:#}"
        );
        assert_eq!(
            during["lanes"]["semantic"]["status"], "building",
            "{during:#}"
        );
        assert_ne!(during["complete"], true, "{during:#}");
        let ledger = ImportLedger::open_existing(&storage, &key).unwrap();
        assert!(
            ledger.is_none_or(|ledger| !ledger.status().unwrap().complete),
            "the import ran before it was admitted"
        );

        drop(slot);
        wait_until(&ctx, "the imported view served", |ctx| {
            ctx.checkout_semantic_runtime().is_some_and(|runtime| {
                runtime
                    .search(&mock_vector("probe"), 1, &|_| true)
                    .is_ok_and(|answer| answer.complete())
            })
        });
        assert_eq!(
            server.texts(),
            legacy_texts,
            "the views-on lane re-embedded content the legacy index already held"
        );
        let status = ImportLedger::read_status(&storage, &key).unwrap().unwrap();
        assert!(status.complete, "{status:?}");
        let semantic = status
            .rows
            .iter()
            .find(|row| row.artifact == Artifact::Semantic)
            .unwrap();
        assert_eq!(semantic.state, ImportState::Done, "{semantic:?}");
        let after = search(&ctx, "evict the oldest cache entry");
        assert!(
            after["results"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty()),
            "{after:#}"
        );
    }
}
