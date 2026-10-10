use super::*;
use crate::{
    config::Config,
    context::{CallgraphStoreAccess, ViewRuntimeSnapshot},
    executor::{Executor, ExecutorConfig, Lane},
    parser::TreeSitterProvider,
    path_identity::ProjectRootId,
    protocol::Response,
};
use crossbeam_channel::{Receiver, Sender};
use std::{path::Path, time::Duration};

static CAS_TIMINGS: LazyLock<Mutex<HashMap<PathBuf, Vec<Duration>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(super) fn record_cas(root: PathBuf, elapsed: Duration) {
    eprintln!(
        "view pointer CAS and handle swap: {} ms",
        elapsed.as_millis()
    );
    CAS_TIMINGS.lock().entry(root).or_default().push(elapsed);
}

static SETTLED: LazyLock<Mutex<HashMap<PathBuf, Sender<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

type Failure = (Instant, Duration, usize, bool);
static FAILURES: LazyLock<Mutex<HashMap<PathBuf, Sender<Failure>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn record_failure(root: &Path, delay: Duration, count: usize, warned: bool) {
    if let Some(tx) = FAILURES.lock().get(root) {
        let _ = tx.send((Instant::now(), delay, count, warned));
    }
}

struct Failures {
    root: PathBuf,
    rx: Receiver<Failure>,
}
impl Failures {
    fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        FAILURES.lock().insert(root.clone(), tx);
        Self { root, rx }
    }
    fn next(&self) -> Failure {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("publication failure")
    }
}
impl Drop for Failures {
    fn drop(&mut self) {
        FAILURES.lock().remove(&self.root);
    }
}

pub(super) fn settled(id: u64, root: &Path) {
    if let Some(tx) = SETTLED.lock().get(root) {
        let _ = tx.send(id);
    }
}

struct Completions {
    root: PathBuf,
    rx: Receiver<u64>,
}
impl Completions {
    fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        SETTLED.lock().insert(root.clone(), tx);
        Self { root, rx }
    }
    fn next(&self) -> u64 {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("publication completion")
    }
}
impl Drop for Completions {
    fn drop(&mut self) {
        SETTLED.lock().remove(&self.root);
    }
}

struct GateEntry {
    phase: &'static str,
    started: Sender<u64>,
    release: Receiver<()>,
}
static GATES: LazyLock<Mutex<HashMap<PathBuf, GateEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PHASES: LazyLock<Mutex<HashMap<PathBuf, Vec<(u64, String)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn phase_gate(id: u64, root: &Path, phase: &str) {
    PHASES
        .lock()
        .entry(root.to_owned())
        .or_default()
        .push((id, phase.to_owned()));
    let gate = {
        let mut gates = GATES.lock();
        if gates.get(root).is_some_and(|gate| gate.phase == phase) {
            gates.remove(root)
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        let _ = gate.started.send(id);
        gate.release
            .recv_timeout(Duration::from_secs(60))
            .expect("release publication gate");
    }
}
struct Gate {
    arrived: Receiver<u64>,
    release: Sender<()>,
}
impl Gate {
    fn new(root: &Path, phase: &'static str) -> Self {
        let (started, arrived) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        GATES.lock().insert(
            root.to_owned(),
            GateEntry {
                phase,
                started,
                release: released,
            },
        );
        Self { arrived, release }
    }
    fn started(&self) -> u64 {
        self.arrived
            .recv_timeout(Duration::from_secs(60))
            .expect("publication reached gate")
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = self.release.try_send(());
    }
}

struct Fixture {
    project: tempfile::TempDir,
    _storage: tempfile::TempDir,
    ctx: Arc<AppContext>,
    executor: Executor,
    root: ProjectRootId,
    view: crate::views::ViewStore,
    initial: String,
}
impl Fixture {
    fn new() -> Self {
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_module("aft::views::generation", log::LevelFilter::Info)
            .try_init();
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(project.path()).unwrap();
        git(&root_path, &["init", "--quiet"]);
        std::fs::write(root_path.join("tracked.rs"), "pub fn previous() {}\n").unwrap();
        commit(&root_path);
        let mut config = Config {
            project_root: Some(root_path.clone()),
            storage_dir: Some(storage.path().to_owned()),
            indexes: crate::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        };
        config.views.enabled = true;
        let ctx = Arc::new(AppContext::new(Box::new(TreeSitterProvider::new()), config));
        ctx.set_canonical_cache_root(root_path.clone());
        let scope = crate::path_identity::project_scope_key(&root_path);
        let view = crate::views::ViewStore::open(storage.path(), &scope).unwrap();
        ctx.install_view_runtime(
            ViewRuntimeSnapshot {
                query_pin: None,
                storage: storage.path().to_owned(),
                family: "publication-fixture".to_owned(),
                scope,
                view_dir: view.view_dir().to_owned(),
                generation: None,
                manifest: None,
                head_fingerprint: String::new(),
                head_metadata: crate::alias::capture_git_head_metadata(&root_path, None).unwrap(),
                pending_paths: BTreeSet::new(),
                pending_inputs: Default::default(),
            },
            None,
        );
        let initial = ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap()
            .generation
            .unwrap();
        let root = ProjectRootId::from_path(&root_path).unwrap();
        let executor = Executor::with_config(ExecutorConfig {
            pool_size: 4,
            read_cap: 2,
            actor_cap: 3,
            heavy_permits: 2,
            drr_quantum: 1,
        });
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        Self {
            project,
            _storage: storage,
            ctx,
            executor,
            root,
            view,
            initial,
        }
    }
    /// The root spelling the publication job records and reports: the context's
    /// canonical cache root (a `std::fs::canonicalize` result, verbatim on
    /// Windows). `ProjectRootId::as_path()` is a different spelling there, so
    /// gates, timing tables and health rows are keyed on this one.
    fn job_root(&self) -> PathBuf {
        self.ctx
            .canonical_cache_root_opt()
            .expect("fixture configured a canonical cache root")
    }
    fn change(&self, symbol: &str) {
        std::fs::write(
            self.project.path().join("tracked.rs"),
            format!("pub fn {symbol}() {{}}\n"),
        )
        .unwrap();
        commit(self.project.path());
    }
    fn schedule(&self) {
        let response = self
            .executor
            .submit(
                self.root.clone(),
                Lane::MaintenanceCommit,
                "views.schedule".to_owned(),
                Box::new(|ctx| {
                    match schedule(ctx, BTreeSet::from([b"tracked.rs".to_vec()]), true) {
                        Ok(()) => Response::success("views.schedule", serde_json::json!({})),
                        Err(error) => Response::error("views.schedule", "publication", error),
                    }
                }),
            )
            .recv_timeout(Duration::from_secs(60))
            .unwrap();
        assert!(response.success, "{:?}", response.data);
    }
    fn wait_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while running_for_context(&self.ctx) {
            assert!(
                Instant::now() < deadline,
                "publication did not settle: {}",
                health_snapshot()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
fn git(root: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    crate::test_env::apply_hermetic_git_env(command.current_dir(root));
    assert!(command.args(args).output().unwrap().status.success());
}
fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn publication_busy_readiness_uses_backoff_and_recovers_after_unlock() {
    let fixture = Fixture::new();
    let derived = fixture.view.derived_path(&fixture.initial).unwrap();
    crate::views::wait_for_derived_checkpoint_for_test(fixture.view.view_dir());
    let writer = crate::db::file_identity::IdentityConnection::open(
        &derived,
        "publication_busy_readiness_test",
    )
    .unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    fixture.change("after_unlock");
    let failures = Failures::new(fixture.job_root());
    let completions = Completions::new(fixture.job_root());
    fixture.schedule();
    let (_, delay, count, _) = failures.next();
    assert_eq!(delay, Duration::from_secs(1));
    assert_eq!(count, 1);
    assert_eq!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    assert_eq!(
        crate::views::assembly::cold_build_counts(fixture.view.view_dir()),
        Default::default()
    );
    assert!(fixture
        .ctx
        .view_publication_retry()
        .lock()
        .error
        .as_deref()
        .unwrap()
        .contains("database is locked"));
    assert!(!fixture
        .ctx
        .view_publication_retry()
        .lock()
        .error
        .as_deref()
        .unwrap()
        .contains("invalid view manifest"));
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    completions.next();
    fixture.wait_idle();
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    assert_eq!(
        crate::views::assembly::cold_build_counts(fixture.view.view_dir()),
        Default::default()
    );
    assert!(fixture.ctx.view_publication_retry().lock().due().is_none());
    match fixture.ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => {
            assert!(crate::callgraph_store::CallGraphRead::node_for(
                &store,
                Path::new("tracked.rs"),
                "after_unlock"
            )
            .is_ok())
        }
        _ => panic!("publication did not recover after the writer released its lock"),
    }
}

#[test]
fn publication_omits_staged_and_unstaged_working_tree_deletions() {
    use crate::views::{
        contracts::{PlaneAdapter, PlaneLoader, ViewAccess},
        first_load::{
            CallgraphBridge, CheckoutDriver, ConfiguredMembershipWalker, SiblingLoader,
            TrigramBridge,
        },
        manifest_v2::Producers,
        registry::FamilyRegistry,
    };
    let mut failures = Vec::new();
    for staged in [false, true] {
        let fixture = Fixture::new();
        std::fs::write(fixture.project.path().join("kept.rs"), "pub fn kept() {}\n").unwrap();
        commit(fixture.project.path());
        let root = fixture.job_root();
        let registry =
            FamilyRegistry::open(fixture._storage.path(), "publication-fixture").unwrap();
        let owner = registry.register_view("query", &root).unwrap();
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let trigram = Arc::new(TrigramBridge::new(
            fixture._storage.path().to_owned(),
            policy,
        ));
        let callgraph = Arc::new(CallgraphBridge::default());
        let adapters: Vec<Arc<dyn PlaneAdapter>> =
            vec![trigram.adapter.clone(), callgraph.adapter.clone()];
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: None,
                    callgraph: crate::views::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![trigram.clone(), callgraph.clone()],
            )
            .with_adapters(adapters.clone()),
        );
        let loader = SiblingLoader::new(driver.clone(), adapters);
        let access = ViewAccess::Owner(owner);
        let before = loader.load(&access).unwrap();
        let index = trigram
            .adapter
            .resident("query", before.snapshot.generation().name())
            .unwrap();
        assert_eq!(
            index
                .query(&root, &before.snapshot, "previous")
                .matches
                .len(),
            1
        );
        assert_eq!(
            callgraph
                .adapter
                .reader(&access, &before.snapshot)
                .unwrap()
                .store
                .indexed_file_count()
                .unwrap(),
            2
        );
        std::fs::remove_file(fixture.project.path().join("tracked.rs")).unwrap();
        if staged {
            git(fixture.project.path(), &["add", "-u"]);
        }
        let result = fixture.ctx.publish_view_paths(BTreeSet::new(), true);
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                failures.push(format!("staged={staged}: {error}"));
                continue;
            }
        };
        assert!(report.published);
        assert!(report
            .manifest
            .unwrap()
            .get(&crate::views::RelPath::new(b"tracked.rs".to_vec()).unwrap())
            .is_none());
        let CallgraphStoreAccess::Ready(store) = fixture.ctx.callgraph_store_for_ops() else {
            panic!("deleted-file view reader unavailable")
        };
        assert_eq!(
            crate::callgraph_store::CallGraphRead::indexed_file_count(&store).unwrap(),
            1
        );
        driver.record_absolute_change(&root.join("tracked.rs"));
        let after = loader.load(&access).unwrap();
        assert!(after.pending_planes.is_empty());
        let index = trigram
            .adapter
            .resident("query", after.snapshot.generation().name())
            .unwrap();
        let deleted = index.query(&root, &after.snapshot, "previous");
        assert!(deleted.matches.is_empty());
        assert!(deleted.gaps.is_empty());
        assert_eq!(index.query(&root, &after.snapshot, "kept").matches.len(), 1);
        let reader = callgraph.adapter.reader(&access, &after.snapshot).unwrap();
        assert_eq!(reader.store.indexed_file_count().unwrap(), 1);
        assert!(reader
            .store
            .node_for(Path::new("tracked.rs"), "previous")
            .is_err());
        assert!(reader.store.node_for(Path::new("kept.rs"), "kept").is_ok());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn publication_omits_file_deleted_after_head_listing() {
    let fixture = Fixture::new();
    let mut deleted = false;
    let mut prepared = fixture
        .ctx
        .prepare_view_paths(BTreeSet::new(), true, &mut |phase| {
            if phase == "working_tree_read" && !deleted {
                std::fs::remove_file(fixture.project.path().join("tracked.rs")).unwrap();
                deleted = true;
            }
            Ok(())
        })
        .unwrap();
    assert!(
        deleted,
        "the deletion must happen after listing, before reading"
    );
    let report = fixture.ctx.commit_view_update(&mut prepared).unwrap();
    assert!(report.published);
    assert!(report.manifest.unwrap().entries().next().is_none());
}

#[test]
fn publication_retires_root_deleted_during_source_read() {
    let fixture = Fixture::new();
    let result = fixture
        .ctx
        .prepare_view_paths(BTreeSet::new(), true, &mut |phase| {
            if phase == "working_tree_read" {
                std::fs::remove_dir_all(fixture.project.path()).unwrap();
            }
            Ok(())
        });
    assert!(matches!(result, Err(error) if error == "view publication root was deleted"));
    assert!(fixture.ctx.view_runtime_snapshot().is_none());
    assert_eq!(
        fixture.view.current_generation().unwrap().unwrap(),
        fixture.initial
    );
}

#[test]
fn publication_does_not_reuse_a_deleted_unchanged_member() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.project.path().join("tracked.rs")).unwrap();
    let report = fixture
        .ctx
        .publish_view_paths(BTreeSet::from([b"other.rs".to_vec()]), true)
        .unwrap();
    assert!(report.published);
    assert!(report.manifest.unwrap().entries().next().is_none());
}

#[test]
fn publication_io_error_names_operation_and_path() {
    let fixture = Fixture::new();
    let path = fixture.project.path().join("tracked.rs");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = fixture
        .ctx
        .publish_view_paths(BTreeSet::new(), true)
        .unwrap_err();
    assert!(error.starts_with("view I/O failed reading "), "{error}");
    assert!(error.contains("tracked.rs"), "{error}");
    assert_ne!(fixture.view.current_generation().unwrap(), None);
}

#[test]
fn publication_failure_streak_survives_changing_artifact_error_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut retry = PublicationRetry::default();
    retry.failed(
        root.path(),
        "view I/O failed writing trigram-1.bin: Permission denied",
    );
    retry.failed(
        root.path(),
        "view I/O failed writing trigram-2.bin: Permission denied",
    );
    assert_eq!(retry.failures, 2);
    assert_eq!(retry.delay, Duration::from_secs(2));
}

#[test]
fn publication_repeated_failure_backs_off_and_edit_resets_deadline() {
    let fixture = Fixture::new();
    let root = fixture.job_root();
    {
        let mut retry = fixture.ctx.view_publication_retry().lock();
        retry.initial = Duration::from_millis(80);
        retry.maximum = Duration::from_millis(320);
    }
    let failures = Failures::new(root.clone());
    let completions = Completions::new(root.clone());
    let path = root.join("tracked.rs");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    fixture.schedule();
    let mut observed = Vec::new();
    for _ in 0..4 {
        observed.push(failures.next());
    }
    // Cancel before asserting, so even a broken retry policy leaves no live
    // worker behind when an assertion fails.
    cancel_for_context(&fixture.ctx);
    completions.next();
    fixture.wait_idle();
    assert_eq!(
        observed
            .iter()
            .map(|row| row.1.as_millis())
            .collect::<Vec<_>>(),
        vec![80, 160, 320, 320]
    );
    assert_eq!(
        observed.iter().map(|row| row.2).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        observed.iter().map(|row| row.3).collect::<Vec<_>>(),
        vec![true, true, false, false]
    );
    for pair in observed.windows(2) {
        assert!(
            pair[1].0.duration_since(pair[0].0) >= pair[0].1,
            "retry ran before its deadline: {pair:?}"
        );
    }
    // A fresh scheduling request without a source edit must retain the streak.
    fixture.schedule();
    let repeated = failures.next();
    cancel_for_context(&fixture.ctx);
    completions.next();
    fixture.wait_idle();
    assert_eq!(repeated.2, 5);
    let edited = root.join("untracked.rs");
    std::fs::write(&edited, "pub fn edit() {}\n").unwrap();
    fixture.ctx.record_checkout_watcher_change(&edited);
    assert!(fixture.ctx.view_publication_retry().lock().due().is_none());
    fixture.schedule();
    let reset = failures.next();
    cancel_for_context(&fixture.ctx);
    completions.next();
    fixture.wait_idle();
    assert_eq!(
        (reset.1, reset.2, reset.3),
        (Duration::from_millis(80), 1, true)
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, "pub fn recovered() {}\n").unwrap();
    fixture.ctx.record_checkout_watcher_change(&path);
    fixture.schedule();
    completions.next();
    fixture.wait_idle();
    assert!(fixture.ctx.view_publication_retry().lock().due().is_none());
    assert_ne!(
        fixture.view.current_generation().unwrap().unwrap(),
        fixture.initial
    );
}

#[test]
fn publication_inline_failure_obeys_the_shared_retry_deadline() {
    let fixture = Fixture::new();
    let root = fixture.job_root();
    let path = root.join("tracked.rs");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let before = fixture.ctx.view_publication_attempts_for_test();
    let mut state = crate::context::WatcherDrainSliceState::new(
        fixture.ctx.configure_generation(),
        fixture.ctx.configure_content_generation(),
    );
    state.view_publication_paths.insert(path.clone());
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    let due = fixture.ctx.view_publication_retry().lock().due().unwrap();
    assert_eq!(fixture.ctx.view_publication_attempts_for_test(), before + 1);
    assert!(state.view_publication_paths.contains(&path));
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    assert_eq!(fixture.ctx.view_publication_attempts_for_test(), before + 1);
    assert_eq!(state.view_publication_due, Some(due));
    fixture.ctx.record_checkout_watcher_change(&path);
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    assert_eq!(fixture.ctx.view_publication_attempts_for_test(), before + 2);
}

#[cfg(unix)]
#[test]
fn publication_omits_deleted_symlinks() {
    for staged in [false, true] {
        let fixture = Fixture::new();
        let link = fixture.project.path().join("link.rs");
        std::os::unix::fs::symlink("tracked.rs", &link).unwrap();
        commit(fixture.project.path());
        fixture
            .ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap();
        std::fs::remove_file(&link).unwrap();
        if staged {
            git(fixture.project.path(), &["add", "-u"]);
        }
        let report = fixture
            .ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap();
        assert!(report.published);
        assert_eq!(report.manifest.unwrap().entries().count(), 1);
    }
}

#[test]
fn publication_build_does_not_delay_same_root_bind_and_read() {
    let fixture = Fixture::new();
    fixture.change("next");
    let gate = Gate::new(fixture.job_root().as_path(), "derived");
    fixture.schedule();
    gate.started();
    let health = health_snapshot();
    assert!(health.as_array().unwrap().iter().any(|job| job["root"]
        == fixture.job_root().as_path().to_string_lossy().as_ref()
        && job["phase"] == "derived"
        && job["barrier_holder"] == false));
    let start = Instant::now();
    let bind = fixture.executor.submit(
        fixture.root.clone(),
        Lane::Mutating,
        "route.bind".to_owned(),
        Box::new(|_| Response::success("route.bind", serde_json::json!({}))),
    );
    let read = fixture.executor.submit(
        fixture.root.clone(),
        Lane::PureRead,
        "view.read".to_owned(),
        Box::new(|ctx| {
            let CallgraphStoreAccess::Ready(store) = ctx.callgraph_store_for_ops() else {
                panic!("previous view reader unavailable")
            };
            assert!(crate::callgraph_store::CallGraphRead::node_for(
                &store,
                Path::new("tracked.rs"),
                "previous"
            )
            .is_ok());
            Response::success("view.read", serde_json::json!({}))
        }),
    );
    let bound = bind.recv_timeout(Duration::from_secs(1));
    let read_result = read.recv_timeout(Duration::from_secs(1).saturating_sub(start.elapsed()));
    let elapsed = start.elapsed();
    eprintln!(
        "bind+read during gated publication: {} ms",
        elapsed.as_millis()
    );
    let previous = fixture.view.current_generation().unwrap();
    drop(gate);
    fixture.wait_idle();
    // The barrier-excludes-the-build proof is the bind+read above completing
    // while `derived` was gated, not this number. The CAS timing is a liveness
    // ceiling only: its fsync+rename exceeded 50 ms on a contended Windows
    // runner, which says nothing about whether the build ran under the barrier.
    let cas_timings = CAS_TIMINGS.lock()[fixture.job_root().as_path()].clone();
    assert!(
        cas_timings
            .iter()
            .all(|elapsed| *elapsed < Duration::from_secs(2)),
        "pointer CAS exceeded the liveness ceiling: {cas_timings:?}"
    );
    assert!(
        bound.is_ok() && read_result.is_ok() && elapsed < Duration::from_secs(1),
        "bind+read blocked for {elapsed:?}"
    );
    assert_eq!(previous.as_deref(), Some(fixture.initial.as_str()));
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
}

#[test]
fn superseded_publication_cancels_before_derived_and_removes_generation_files() {
    let fixture = Fixture::new();
    fixture.change("older");
    let completions = Completions::new(fixture.job_root());
    let gate = Gate::new(fixture.job_root().as_path(), "blobs");
    fixture.schedule();
    let older = gate.started();
    fixture.change("newer");
    let newer_gate = Gate::new(fixture.job_root().as_path(), "cas");
    fixture.schedule();
    let newer_id = newer_gate.started();
    let delay = std::env::var("AFT_TEST_TIMING_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(delay));
    drop(newer_gate);
    // The newer job must finish while the obsolete assembly remains held.
    // Waiting for its lifecycle completion also observes cleanup, not just CAS.
    assert_eq!(completions.next(), newer_id);
    let newer = fixture.view.current_generation().unwrap();
    assert_ne!(newer.as_deref(), Some(fixture.initial.as_str()));
    drop(gate);
    assert_eq!(completions.next(), older);
    assert_eq!(fixture.view.current_generation().unwrap(), newer);
    assert!(
        !PHASES.lock()[fixture.job_root().as_path()]
            .iter()
            .any(|(id, phase)| *id == older && phase == "derived"),
        "superseded assembly entered derived phase instead of cancelling"
    );
    let files = std::fs::read_dir(fixture.view.view_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("derived-")
                || name.starts_with("manifest-")
                || name.starts_with("trigram-")
        })
        .collect::<Vec<_>>();
    assert!(files.iter().all(|name| name.contains(&fixture.initial) || name.contains(newer.as_deref().unwrap())), "orphan generation files: {files:?}");
    let CallgraphStoreAccess::Ready(store) = fixture.ctx.callgraph_store_for_ops() else {
        panic!("newer reader unavailable")
    };
    assert!(crate::callgraph_store::CallGraphRead::node_for(
        &store,
        Path::new("tracked.rs"),
        "newer"
    )
    .is_ok());
}

#[test]
fn generation_sweep_waits_for_the_last_query_handle() {
    let fixture = Fixture::new();
    let CallgraphStoreAccess::Ready(previous_reader) = fixture.ctx.callgraph_store_for_ops() else {
        panic!("previous reader unavailable")
    };
    // The initial publication also owns a deferred checkpoint connection. Settle
    // it before replacing the view, so the old query handle is the only keeper
    // of the obsolete database whose lifetime this test asserts.
    crate::views::wait_for_derived_checkpoint_for_test(fixture.view.view_dir());
    fixture.change("next");
    fixture.schedule();
    fixture.wait_idle();
    assert_eq!(fixture.view.sweep_generations().unwrap(), 0);
    assert!(fixture
        .view
        .derived_path(&fixture.initial)
        .unwrap()
        .is_file());
    assert!(crate::callgraph_store::CallGraphRead::node_for(
        &previous_reader,
        Path::new("tracked.rs"),
        "previous"
    )
    .is_ok());
    drop(previous_reader);
    assert_eq!(fixture.view.sweep_generations().unwrap(), 1);
    assert!(!fixture
        .view
        .derived_path(&fixture.initial)
        .unwrap()
        .exists());
    assert!(!fixture
        .view
        .manifest_path(&fixture.initial)
        .unwrap()
        .exists());
}

#[test]
fn publication_health_reports_root_and_each_off_lane_phase() {
    let fixture = Fixture::new();
    for phase in ["manifest", "blobs", "derived", "cas"] {
        fixture.change(&format!("phase_{phase}"));
        let gate = Gate::new(fixture.job_root().as_path(), phase);
        fixture.schedule();
        let id = gate.started();
        let health = health_snapshot();
        let job = health
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["id"] == id)
            .unwrap();
        assert_eq!(
            job["root"],
            fixture.job_root().as_path().to_string_lossy().as_ref()
        );
        assert_eq!(job["phase"], phase);
        assert_eq!(job["barrier_holder"], false);
        assert!(
            !fixture.executor.actor_is_idle(&fixture.root),
            "an off-lane build must prevent actor retirement"
        );
        drop(gate);
        fixture.wait_idle();
    }
}

#[test]
fn deleted_root_pending_publication_retires_once_without_rescheduling() {
    let fixture = Fixture::new();
    fixture.change("next");
    let root = fixture.job_root();
    let before = fixture.ctx.view_publication_attempts_for_test();
    let gate = Gate::new(&root, "manifest");
    let completions = Completions::new(root.clone());
    fixture.schedule();
    gate.started();
    std::fs::remove_dir_all(fixture.project.path()).unwrap();
    drop(gate);
    completions.next();
    fixture.wait_idle();
    assert_eq!(
        fixture.ctx.view_publication_attempts_for_test(),
        before + 1,
        "pending publication must make exactly one preparation attempt"
    );
    assert!(
        fixture.ctx.view_runtime_snapshot().is_none(),
        "deletion must retire views state, not just stop the current thread"
    );
    // Repeated quiet-window ticks simulate watcher publication retries after
    // checkout deletion. Late paths must not recreate a retired publication.
    let mut state = crate::context::WatcherDrainSliceState::new(
        fixture.ctx.configure_generation(),
        fixture.ctx.configure_content_generation(),
    );
    for _ in 0..3 {
        state.view_publication_due = Some(Instant::now());
        state.view_publication_paths.insert(root.join("tracked.rs"));
        crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
        assert!(state.view_publication_due.is_none());
        assert!(state.view_publication_paths.is_empty());
        assert!(schedule(&fixture.ctx, BTreeSet::new(), true).is_ok());
    }
    assert_eq!(
        fixture.ctx.view_publication_attempts_for_test(),
        before + 1,
        "deleted publication must never be re-scheduled"
    );
    assert!(!running_for_context(&fixture.ctx));
}
