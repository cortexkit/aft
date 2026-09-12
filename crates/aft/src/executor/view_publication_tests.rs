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
            .recv_timeout(Duration::from_secs(15))
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
            .recv_timeout(Duration::from_secs(10))
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
            semantic_search: false,
            search_index: false,
            callgraph_store: true,
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
                pending_paths: BTreeSet::new(),
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
            .recv_timeout(Duration::from_secs(2))
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
    let gate = Gate::new(fixture.job_root().as_path(), "blobs");
    fixture.schedule();
    let older = gate.started();
    fixture.change("newer");
    fixture.schedule();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.view.current_generation().unwrap().as_deref() == Some(&fixture.initial) {
        assert!(
            Instant::now() < deadline,
            "newer publication blocked behind older one"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let newer = fixture.view.current_generation().unwrap();
    drop(gate);
    fixture.wait_idle();
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
