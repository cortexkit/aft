//! v3 base references across real processes: concurrent binders with a
//! stopped one, a publisher killed at every base-path durability boundary,
//! and the delete-main scenario where a worker restarts and still serves from
//! its base after a foreign process collected the family.
//!
//! Child processes re-run this test binary with `--ignored --exact
//! v3_base_refs::base_ref_child`; the scenario and its pause point come from
//! `BASE_REF_*` environment variables. A child parks after writing its ready
//! file and is stopped or killed by the parent.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aft::blob_store::v2::{ContentHash, FamilyPlane, TrigramPolicy};
use aft::blob_store::v3;
use aft::views::base_builder::{
    build_base, claim_base, publish_base, publish_successor, BaseBuild, BaseBuildInput,
    BasePublication, ClaimOutcome,
};
use aft::views::base_refs::{
    open_foreign_v3, publish_worker, resolve_base, with_base_barrier, BaseIdentity, WorkerOutcome,
    WorkerPublication,
};
use aft::views::base_sweep::sweep_family;
use aft::views::contracts::{DurabilityObserver, DurabilityStep};
use aft::views::eviction::DiskBudget;
use aft::views::manifest_v2::{
    EntryPlanes, EntryV2, ManifestHeader, ManifestV2, Producers, PublishedFrontier,
};
use aft::views::readiness::PlaneState;
use aft::views::RelPath;
use tempfile::tempdir;

const FAMILY: &str = "00112233445566ff";
const CHILD_TEST: &str = "v3_base_refs::base_ref_child";
const FILES: &[(&str, &[u8])] = &[
    ("src/lib.rs", b"pub fn shared() -> u32 { 1 }"),
    ("src/util.rs", b"pub fn helper() -> u32 { 2 }"),
];

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
    }
}

fn producers() -> Producers {
    Producers {
        trigram: policy().fingerprint_hex(),
        semantic: None,
        callgraph: "graph".into(),
    }
}

fn rel(name: &str) -> RelPath {
    RelPath::new(name.as_bytes().to_vec()).unwrap()
}

fn checkout(root: &Path, files: &[(&str, &[u8])]) -> Vec<RelPath> {
    files
        .iter()
        .map(|(name, bytes)| {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            rel(name)
        })
        .collect()
}

fn build(storage: &Path, root: &Path, observer: Option<&dyn DurabilityObserver>) -> BaseBuild {
    let paths = checkout(root, FILES);
    build_base(
        BaseBuildInput {
            storage,
            family: FAMILY,
            root,
            paths: &paths,
            policy: policy(),
            semantic_producer: None,
            callgraph_producer: "graph".into(),
            ignore_fingerprint: None,
            history: None,
            budget: DiskBudget::default(),
        },
        observer,
    )
    .unwrap()
}

fn current_base(storage: &Path, scope: &str) -> (BaseIdentity, String) {
    let base = resolve_base(storage, FAMILY, scope).unwrap();
    (base.identity().clone(), base.generation().to_owned())
}

/// A worker delta that edits nothing: it inherits every base entry.
fn bind(
    storage: &Path,
    scope: &str,
    identity: &BaseIdentity,
    generation: &str,
    changed: Option<&[u8]>,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<WorkerOutcome, aft::views::base_refs::BaseRefError> {
    let header = ManifestHeader {
        producers: producers(),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    };
    let mut delta = ManifestV2::new_v3(
        header,
        Some(identity.header(generation)),
        PublishedFrontier::default(),
    );
    if let Some(bytes) = changed {
        delta
            .insert(
                rel("src/worker_only.rs"),
                EntryV2::regular(
                    ContentHash::of(bytes),
                    bytes.len() as u64,
                    EntryPlanes::default(),
                ),
            )
            .unwrap();
    }
    let root = storage.join("roots").join(scope);
    fs::create_dir_all(&root).unwrap();
    let base = identity.header(generation);
    publish_worker(
        WorkerPublication {
            storage,
            family: FAMILY,
            worker_scope: scope,
            root: &root,
            base: &base,
            delta,
            previous: None,
        },
        observer,
    )
}

fn spawn_child(scenario: &str, storage: &Path, ready: &Path, extra: &[(&str, &str)]) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("BASE_REF_SCENARIO", scenario)
        .env("BASE_REF_STORAGE", storage)
        .env("BASE_REF_READY", ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in extra {
        command.env(name, value);
    }
    command.spawn().unwrap()
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

fn signal(child: &Child, signal: i32) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
}

fn kill(child: &mut Child) {
    signal(child, libc::SIGKILL);
    assert!(!child.wait().unwrap().success());
}

struct ParkAt {
    step: DurabilityStep,
    ready: PathBuf,
}

impl DurabilityObserver for ParkAt {
    fn reached(&self, step: DurabilityStep) {
        if step == self.step {
            fs::write(&self.ready, self.step.as_str()).unwrap();
            loop {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn park_forever(ready: &Path) -> ! {
    fs::write(ready, b"ready").unwrap();
    loop {
        thread::sleep(Duration::from_millis(10));
    }
}

/// The child side of every multi-process scenario.
#[test]
#[ignore]
fn base_ref_child() {
    let storage = PathBuf::from(std::env::var_os("BASE_REF_STORAGE").expect("BASE_REF_STORAGE"));
    let ready = PathBuf::from(std::env::var_os("BASE_REF_READY").expect("BASE_REF_READY"));
    let scenario = std::env::var("BASE_REF_SCENARIO").unwrap();
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    match scenario.as_str() {
        "bind-and-park" => {
            let scope = std::env::var("BASE_REF_WORKER").unwrap();
            let base = std::env::var("BASE_REF_BASE").unwrap();
            let (identity, generation) = current_base(&storage, &base);
            let park = std::env::var("BASE_REF_STEP").ok().map(|step| ParkAt {
                step: DurabilityStep::parse(&step).expect("unknown step"),
                ready: ready.clone(),
            });
            let outcome = bind(
                &storage,
                &scope,
                &identity,
                &generation,
                Some(scope.as_bytes()),
                park.as_ref().map(|park| park as &dyn DurabilityObserver),
            )
            .unwrap();
            assert!(matches!(outcome, WorkerOutcome::Published { .. }));
            // Keep the bundle (and its markers) for as long as this process lives.
            let _held = outcome;
            park_forever(&ready);
        }
        "build-main-and-park" => {
            let root = PathBuf::from(std::env::var_os("BASE_REF_ROOT").unwrap());
            let BaseBuild::Published {
                identity,
                generation,
                claim,
            } = build(&storage, &root, None)
            else {
                panic!("main's daemon is the only builder");
            };
            let _main = bind(&storage, "main", &identity, &generation, None, None).unwrap();
            let _claim = claim;
            park_forever(&ready);
        }
        "foreign-gc" => {
            // A standalone process with no view session or views
            // configuration: it honours references through the protocol.
            for _ in 0..3 {
                sweep_family(&storage, FAMILY, None).unwrap();
            }
            fs::write(&ready, b"swept").unwrap();
        }
        "base-path" => {
            let name = std::env::var("BASE_REF_STEP").unwrap();
            let step = DurabilityStep::parse(&name)
                .unwrap_or_else(|| panic!("the kill-test child refuses unknown step {name}"));
            assert!(
                DurabilityStep::BASE_PATH.contains(&step),
                "{name} is not a base-path step"
            );
            let observer = ParkAt { step, ready };
            let root = storage.join("roots/main");
            let BaseBuild::Published {
                identity,
                generation,
                claim,
            } = build(&storage, &root, Some(&observer))
            else {
                panic!("the only builder publishes");
            };
            let successor = publish_successor(
                &claim,
                |manifest| {
                    for (_, entry) in manifest.entries_mut() {
                        if let EntryV2::Regular { planes, .. } = entry {
                            planes.callgraph = Some(PlaneState::Failed {
                                reason: "no extractor".into(),
                                producer: "graph".into(),
                            });
                        }
                    }
                },
                Some(&observer),
            )
            .unwrap();
            let bound = bind(
                &storage,
                "worker",
                &identity,
                &successor,
                Some(b"worker"),
                Some(&observer),
            )
            .unwrap();
            drop(bound);
            claim.release().unwrap();
            let _ = generation;
            // An unrelated, unreferenced base for the reclaim boundaries.
            let other = base_manifest(&[("other.rs", b"other")]);
            assert!(matches!(
                publish_base(&storage, FAMILY, &other, Some(&observer)).unwrap(),
                BasePublication::Published { .. }
            ));
            for _ in 0..2 {
                sweep_family(&storage, FAMILY, Some(&observer)).unwrap();
            }
            panic!("the pause point {name} was never reached");
        }
        other => panic!("unknown scenario {other}"),
    }
}

/// A base manifest whose lexical plane is complete without stored payloads:
/// an empty-content file needs no trigram row for this protocol test.
fn base_manifest(files: &[(&str, &[u8])]) -> ManifestV2 {
    let mut manifest = ManifestV2::new_v3(
        ManifestHeader {
            producers: producers(),
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        },
        None,
        PublishedFrontier::default(),
    );
    for (name, bytes) in files {
        manifest
            .insert(
                rel(name),
                EntryV2::regular(
                    ContentHash::of(bytes),
                    bytes.len() as u64,
                    EntryPlanes::default(),
                ),
            )
            .unwrap();
    }
    manifest
}

fn refs(storage: &Path) -> Vec<(String, String)> {
    with_base_barrier(storage, FAMILY, |tx| {
        Ok(tx
            .prepare("SELECT worker_scope,state FROM base_refs ORDER BY worker_scope")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap()
}

fn registered(storage: &Path, scope: &str) -> bool {
    v3::LedgerReader::open_existing(storage, FAMILY)
        .unwrap()
        .member(scope)
        .unwrap()
        .is_some()
}

/// G7: two child binders plus several in-process binds, one of them aborting,
/// and one child SIGSTOPped with its intent row across two sweeps.
#[test]
fn concurrent_binders_across_processes_keep_the_base_and_a_stopped_owner_keeps_its_intent() {
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    let storage = tempdir().unwrap();
    let root = storage.path().join("roots/main");
    let BaseBuild::Published {
        identity, claim, ..
    } = build(storage.path(), &root, None)
    else {
        panic!()
    };
    claim.release().unwrap();
    let control = tempdir().unwrap();
    let ready_a = control.path().join("a");
    let mut a = spawn_child(
        "bind-and-park",
        storage.path(),
        &ready_a,
        &[
            ("BASE_REF_WORKER", "worker-a"),
            ("BASE_REF_BASE", &identity.scope),
        ],
    );
    let ready_s = control.path().join("s");
    let mut stopped = spawn_child(
        "bind-and-park",
        storage.path(),
        &ready_s,
        &[
            ("BASE_REF_WORKER", "worker-s"),
            ("BASE_REF_BASE", &identity.scope),
            ("BASE_REF_STEP", DurabilityStep::BaseRefMarked.as_str()),
        ],
    );
    wait_for_file(&mut a, &ready_a);
    wait_for_file(&mut stopped, &ready_s);
    signal(&stopped, libc::SIGSTOP);
    let (_, generation) = current_base(storage.path(), &identity.scope);
    let mut held = Vec::new();
    for index in 0..3u8 {
        let mut header = identity.clone();
        if index == 2 {
            header.membership = [0; 32];
        }
        match bind(
            storage.path(),
            &format!("local-{index}"),
            &header,
            &generation,
            Some(&[index]),
            None,
        ) {
            Ok(outcome) => held.push(outcome),
            Err(_) => assert_eq!(index, 2, "only the forged binder aborts"),
        }
    }
    assert_eq!(held.len(), 2);
    for _ in 0..2 {
        let report = sweep_family(storage.path(), FAMILY, None).unwrap();
        assert!(
            report.counted_bases.is_empty() && report.reclaimed_bases.is_empty(),
            "{report:?}"
        );
    }
    let rows = refs(storage.path());
    assert!(
        rows.contains(&("worker-s".into(), "intent".into())),
        "{rows:?}"
    );
    assert!(
        rows.contains(&("worker-a".into(), "published".into())),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|(worker, _)| worker == "local-2"),
        "{rows:?}"
    );
    // The stopped owner dies: its intent is orphaned, then retired, while
    // the published references keep the base.
    signal(&stopped, libc::SIGCONT);
    kill(&mut stopped);
    sweep_family(storage.path(), FAMILY, None).unwrap();
    assert!(refs(storage.path()).contains(&("worker-s".into(), "orphaned".into())));
    sweep_family(storage.path(), FAMILY, None).unwrap();
    assert!(!refs(storage.path())
        .iter()
        .any(|(worker, _)| worker == "worker-s"));
    kill(&mut a);
    drop(held);
    for _ in 0..2 {
        let report = sweep_family(storage.path(), FAMILY, None).unwrap();
        assert!(report.reclaimed_bases.is_empty(), "{report:?}");
    }
    let reader = open_foreign_v3(storage.path(), FAMILY, "worker-a", &producers())
        .unwrap()
        .unwrap();
    assert!(reader.entry(&rel("src/lib.rs")).is_some());
    assert!(!storage.path().join("views/v3").exists());
}

/// Kill a builder/binder/sweeper at every base-path boundary. Afterwards no
/// published reference is dangling, a dead claim is taken over with
/// attempt + 1, dead reservations are released, and nothing appears under
/// `<storage>/views/v3`.
#[test]
fn a_process_killed_at_any_base_path_boundary_leaves_references_readable() {
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    for step in DurabilityStep::BASE_PATH {
        let storage = tempdir().unwrap();
        let control = tempdir().unwrap();
        let ready = control.path().join("ready");
        let mut child = spawn_child(
            "base-path",
            storage.path(),
            &ready,
            &[("BASE_REF_STEP", step.as_str())],
        );
        wait_for_file(&mut child, &ready);
        assert_eq!(fs::read_to_string(&ready).unwrap(), step.as_str());
        kill(&mut child);
        // Recovery: a fresh process sweeps and, if the base is unfinished,
        // takes over the dead claim.
        for _ in 0..3 {
            sweep_family(storage.path(), FAMILY, None)
                .unwrap_or_else(|error| panic!("{step:?}: {error}"));
        }
        let lock = v3::ReservationLock::acquire(storage.path()).unwrap();
        let registry = v3::RegistryWriter::open_existing(storage.path(), FAMILY, &lock).unwrap();
        assert_eq!(registry.accounting().unwrap().reserved_bytes, 0, "{step:?}");
        drop(registry);
        drop(lock);
        if registered(storage.path(), "worker") {
            if let Some(reader) = open_foreign_v3(storage.path(), FAMILY, "worker", &producers())
                .unwrap_or_else(|error| panic!("{step:?}: dangling reference: {error}"))
            {
                assert!(reader.entry(&rel("src/lib.rs")).is_some(), "{step:?}");
            }
        }
        if step == DurabilityStep::BaseClaimed {
            let paths = checkout(&storage.path().join("roots/main"), FILES);
            let scope = {
                let BaseBuild::Published { claim, .. } = build_base(
                    BaseBuildInput {
                        storage: storage.path(),
                        family: FAMILY,
                        root: &storage.path().join("roots/main"),
                        paths: &paths,
                        policy: policy(),
                        semantic_producer: None,
                        callgraph_producer: "graph".into(),
                        ignore_fingerprint: None,
                        history: None,
                        budget: DiskBudget::default(),
                    },
                    None,
                )
                .unwrap() else {
                    panic!("{step:?}: the dead claim was not taken over");
                };
                assert_eq!(claim.attempt(), 2, "{step:?}");
                claim.scope().to_owned()
            };
            assert!(matches!(
                claim_base(storage.path(), FAMILY, &scope, None).unwrap(),
                ClaimOutcome::Busy { attempt: 2, .. }
            ));
        }
        assert!(!storage.path().join("views/v3").exists(), "{step:?}");
    }
}

/// Delete main, kill main's daemon, run GC in another process, restart the
/// worker: it serves from its base with correct answers.
#[test]
fn a_worker_restarted_after_main_is_deleted_and_collected_serves_from_its_base() {
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    let storage = tempdir().unwrap();
    let control = tempdir().unwrap();
    let main_root = storage.path().join("roots/main");
    let ready = control.path().join("main");
    let mut main = spawn_child(
        "build-main-and-park",
        storage.path(),
        &ready,
        &[("BASE_REF_ROOT", main_root.to_str().unwrap())],
    );
    wait_for_file(&mut main, &ready);
    // The base main's daemon built is the family's only base member.
    let scope = with_base_barrier(storage.path(), FAMILY, |tx| {
        Ok(
            tx.query_row("SELECT scope FROM members WHERE kind='base'", [], |r| {
                r.get::<_, String>(0)
            })?,
        )
    })
    .unwrap();
    // The worker binds and its daemon exits.
    let ready_worker = control.path().join("worker");
    let mut worker = spawn_child(
        "bind-and-park",
        storage.path(),
        &ready_worker,
        &[("BASE_REF_WORKER", "worker"), ("BASE_REF_BASE", &scope)],
    );
    wait_for_file(&mut worker, &ready_worker);
    kill(&mut worker);
    // Delete main and kill its daemon, then collect in a foreign process.
    fs::remove_dir_all(&main_root).unwrap();
    kill(&mut main);
    let ready_gc = control.path().join("gc");
    let mut gc = spawn_child("foreign-gc", storage.path(), &ready_gc, &[]);
    wait_for_file(&mut gc, &ready_gc);
    assert!(gc.wait().unwrap().success());
    // Restart the worker: its published generation composes over the base.
    assert!(registered(storage.path(), &scope));
    let reader = open_foreign_v3(storage.path(), FAMILY, "worker", &producers())
        .unwrap()
        .unwrap();
    for (name, bytes) in FILES {
        let entry = reader.entry(&rel(name)).unwrap();
        assert_eq!(entry.content(), Some(ContentHash::of(bytes)), "{name}");
        assert!(matches!(
            entry.plane_state(FamilyPlane::Trigram),
            Some(PlaneState::Ready { .. })
        ));
    }
    assert!(reader.entry(&rel("src/worker_only.rs")).is_some());
    assert!(!storage.path().join("views/v3").exists());
}
