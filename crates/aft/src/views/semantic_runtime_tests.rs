//! Checkout semantic views through the runtime that serves views-on search:
//! shared planes across checkouts, embed counts across sessions, and edits
//! reaching the fill through both the watcher and AFT's own writes.

use super::super::first_load::MembershipWalker;
use super::super::snapshot::ComposedGeneration;
use super::*;
use crate::semantic_index::{
    EmbedTextCaps, LocalEmbeddingProvider, SemanticEmbeddingModel, SemanticIndex, SemanticResult,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;

/// Store rows a filled one-chunk file leaves in the family's v3 semantic
/// plane: its file run and its chunk-vector row. (Verified-run marker rows
/// are a v2 optimization the v3 store does not write.)
const ROWS_PER_ONE_CHUNK_FILE: u64 = 2;

/// Row usage of the family's v3 semantic plane store, read through its own
/// read-only connection.
struct SemanticRows {
    storage: PathBuf,
    family: String,
    path: PathBuf,
}

struct SemanticRowUsage {
    rows: u64,
    payload_bytes: u64,
}

impl SemanticRows {
    fn of(runtime: &CheckoutSemantic) -> Self {
        let family = runtime.owner.family().to_owned();
        Self {
            path: crate::blob_store::v3::plane_path(
                &runtime.storage,
                &family,
                crate::blob_store::v2::FamilyPlane::Semantic,
            )
            .unwrap(),
            storage: runtime.storage.clone(),
            family,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn usage(&self) -> Result<SemanticRowUsage, String> {
        if !self.path.exists() {
            return Ok(SemanticRowUsage {
                rows: 0,
                payload_bytes: 0,
            });
        }
        rusqlite::Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .and_then(|connection| {
            connection.query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
                [],
                |row| {
                    Ok(SemanticRowUsage {
                        rows: row.get(0)?,
                        payload_bytes: row.get(1)?,
                    })
                },
            )
        })
        .map_err(|error| error.to_string())
    }

    fn get(&self, key: &crate::blob_store::v2::FamilyKey) -> Result<Option<Vec<u8>>, String> {
        crate::blob_store::v3::PlaneReader::open_existing(
            &self.storage,
            &self.family,
            crate::blob_store::v2::FamilyPlane::Semantic,
        )
        .and_then(|reader| reader.get(key))
        .map_err(|error| error.to_string())
    }
}

#[test]
fn reconciled_delta_does_not_request_another_source_walk() {
    let manifest =
        super::super::manifest_v2::ManifestV2::new(super::super::manifest_v2::ManifestHeader {
            producers: Producers {
                trigram: "unused".into(),
                semantic: None,
                callgraph: "unused".into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
    let mut delta = LiveDelta::new(Arc::new(ComposedGeneration::new(
        "published",
        manifest,
        None,
    )));
    let path = super::super::RelPath::new(b"file.rs".to_vec()).unwrap();
    let mut entry = super::super::snapshot::LiveEntry::new(DiskState::of_bytes(b"new"), 1);
    delta.apply(path.clone(), entry.clone());
    assert!(super::needs_reload(&delta.snapshot()));
    entry.reconciliation = super::super::readiness::ReconciledDelta::Reconciled;
    delta.apply(path.clone(), entry);
    assert!(!super::needs_reload(&delta.snapshot()));
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("file.rs")).unwrap();
    assert!(!super::needs_cold_reconcile(&delta.snapshot(), root.path()));
    delta.record_intent(path);
    assert!(super::needs_reload(&delta.snapshot()));
    assert!(super::needs_cold_reconcile(&delta.snapshot(), root.path()));
}

/// The test prints public setup, query answers, a v3 file inventory and exact
/// reader-floor bytes. Query expectations come from an independent cold index,
/// not the view.
#[test]
fn composition_no_marker_boundary_record() {
    use crate::migration::prune_legacy::{
        prune_legacy, CensusFinding, ProcessCensus, PruneExit, PruneOptions,
    };
    use crate::root_cache::StoragePaths;
    struct NoProcesses;
    impl ProcessCensus for NoProcesses {
        fn take(&self) -> Result<Vec<CensusFinding>, String> {
            Ok(Vec::new())
        }
    }
    // Inventory names and sizes without opening another descriptor on a live
    // SQLite file set. The reader floor is a separately encoded JSON file.
    fn inventory(root: &Path, path: &Path, files: &mut std::collections::BTreeMap<String, u64>) {
        if !path.exists() {
            return;
        }
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                inventory(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::metadata(&path).unwrap().len(),
                );
            }
        }
    }
    let mut answers = Vec::new();
    for views in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let temp = std::fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        let root = temp.join("project");
        write_tree(&root, FILES);
        std::fs::create_dir_all(&storage).unwrap();
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "composition-boundary", "command": "configure", "project_root": root, "storage_dir": storage,
            "harness": "runner", "session_id": "alfonso:sidekick-00000000-0000-4001-98dd-383a2dbeca78",
            "config": [{ "tier": "user", "source": "/u/aft.jsonc", "doc": serde_json::json!({
                "views": { "enabled": views }, "indexes": { "trigram": false, "semantic": true, "callgraph": false },
                "semantic": { "backend": "openai_compatible", "model": "boundary-no-network", "base_url": "http://127.0.0.1:1", "timeout_ms": 50 }
            }).to_string() }]
        })).unwrap();
        let response = crate::commands::configure::handle_configure(&request, &ctx);
        assert!(response.success, "{}", response.data);
        crate::commands::configure::drain_deferred_configure_maintenance(&ctx);
        assert!(
            ctx.semantic_refresh_sender().is_none(),
            "the sidekick fixture must not start an embedding worker"
        );
        let answer = if views {
            let family = crate::search_index::artifact_cache_key(&root);
            let scope = crate::path_identity::project_scope_key(&root);
            let runtime = CheckoutSemantic::new(
                &StoragePaths::same(&storage),
                &family,
                &scope,
                &root,
                producer(),
                Weak::new(),
            )
            .unwrap();
            runtime.load().unwrap();
            refresh(&runtime, &Model::default());
            let result = query(&runtime, "alpha cache");
            assert!(result.complete(), "{result:?}");
            rows(&root, &result.results)
        } else {
            cold(&root, "alpha cache").0
        };
        assert!(!answer.is_empty());
        for _ in 0..2 {
            let report = crate::storage_retention::run_pass_with_budget_for_test(
                &StoragePaths::same(&storage),
                Duration::from_secs(120),
            );
            assert!(report.timings_ms.contains_key("blobs_v3"), "{report:?}");
        }
        let floor_before = std::fs::read(storage.join("reader-floor.json")).unwrap();
        let mut before = Default::default();
        inventory(&storage, &storage.join("blobs/v3"), &mut before);
        let mut out = Vec::new();
        let exit = prune_legacy(
            &storage,
            &PruneOptions {
                yes: false,
                now: std::time::SystemTime::now(),
            },
            &NoProcesses,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, PruneExit::Clean);
        assert!(String::from_utf8(out).unwrap().contains("Dry run"));
        let mut v3 = Default::default();
        inventory(&storage, &storage.join("blobs/v3"), &mut v3);
        assert_eq!(before, v3, "dry-run must not mutate the v3 namespace");
        assert_eq!(
            std::fs::read(storage.join("reader-floor.json")).unwrap(),
            floor_before
        );
        let floor = std::fs::read_to_string(storage.join("reader-floor.json")).unwrap();
        if views {
            // The views-on semantic view is a v3 member and writes nothing to
            // the v2 namespace.
            assert!(storage.join("blobs/v3").exists());
            assert!(!storage.join("blobs/v2").exists());
        } else {
            assert!(
                !storage.join("blobs/v3").exists(),
                "a views-off entrypoint wrote v3"
            );
            assert!(!floor.contains("views_v3"));
        }
        eprintln!(
            "COMPOSITION_BOUNDARY_RECORD {}",
            serde_json::json!({ "views": views, "answers": answer, "v3_files": v3, "floor": floor })
        );
        answers.push(answer);
    }
    assert_eq!(answers[0], answers[1]);
}

#[test]
fn composition_prune_dry_run_preserves_populated_v3_inventory() {
    use crate::migration::prune_legacy::{
        prune_legacy, CensusFinding, ProcessCensus, PruneExit, PruneOptions,
    };
    struct NoProcesses;
    impl ProcessCensus for NoProcesses {
        fn take(&self) -> Result<Vec<CensusFinding>, String> {
            Ok(Vec::new())
        }
    }
    fn inventory(root: &Path, path: &Path, out: &mut std::collections::BTreeMap<String, [u8; 32]>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                inventory(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    *blake3::hash(&std::fs::read(&path).unwrap()).as_bytes(),
                );
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path();
    let v3 = storage.join("blobs/v3/populated");
    std::fs::create_dir_all(v3.join("views/worker")).unwrap();
    // A legacy pruner must preserve opaque newer files, not just rows it can decode.
    // These fixture files are opaque bytes with no live SQLite connection.
    std::fs::write(v3.join("semantic.sqlite"), b"opaque future plane payload").unwrap();
    std::fs::write(
        v3.join("views/worker/manifest-future.json"),
        br#"{"format_version":3,"entries":["kept.rs"]}"#,
    )
    .unwrap();
    crate::reader_floor::raise(
        storage,
        &[
            (crate::persisted_format::PersistedStore::ViewsV3Registry, 1),
            (crate::persisted_format::PersistedStore::ViewsV3Store, 1),
        ],
    )
    .unwrap();
    let mut before = Default::default();
    inventory(storage, &v3, &mut before);
    assert_eq!(before.len(), 2);
    let floor = std::fs::read(storage.join("reader-floor.json")).unwrap();
    let mut output = Vec::new();
    assert_eq!(
        prune_legacy(
            storage,
            &PruneOptions {
                yes: false,
                now: std::time::SystemTime::now()
            },
            &NoProcesses,
            None,
            &mut output
        )
        .unwrap(),
        PruneExit::Clean
    );
    let mut after = Default::default();
    if v3.exists() {
        inventory(storage, &v3, &mut after);
    }
    assert_eq!(before, after, "dry-run changed populated v3 files");
    assert_eq!(
        floor,
        std::fs::read(storage.join("reader-floor.json")).unwrap()
    );
}

#[test]
fn lane_paused_view_fill_waits_without_rechunking_and_resumes() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock.clone(), None, |config, requests| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("checkout");
            let storage = directory.path().join("storage");
            write_tree(
                &root,
                &[("pause.ts", "export function keyword() { return 3; }\n")],
            );
            let runtime = open(&storage, "paused", &root);
            let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            let before = lane_pause_chunk_calls();
            let mut attempts = Vec::new();
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, _wake) = slot.begin();
            let outcome = catch_up(
                &Arc::downgrade(&slot),
                epoch,
                &runtime,
                &fast_schedule(&root),
                FillBudget::default(),
                &mut |texts| {
                    attempts.push((
                        clock.load(Ordering::SeqCst),
                        lane_pause_chunk_calls() - before,
                    ));
                    model.embed(texts)
                },
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            assert_eq!(attempts, vec![(START, 1), (START + PAUSE_MS, 1)]);
            assert_eq!(lane_pause_chunk_calls() - before, 1);
            assert_eq!(requests.lock().unwrap().len(), 3);
            assert!(!runtime
                .search(&[1.0, 0.0], 10, &|_| true)
                .unwrap()
                .results
                .is_empty());
        });
    });
}

#[test]
fn shape_compiling_view_fill_waits_out_the_compile_and_resumes() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_compiling_synapse(clock.clone(), 90_000, |config, requests| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("checkout");
            let storage = directory.path().join("storage");
            write_tree(
                &root,
                &[("compile.ts", "export function keyword() { return 3; }\n")],
            );
            let runtime = open(&storage, "compiling", &root);
            let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            let before = lane_pause_chunk_calls();
            let mut attempts = Vec::new();
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, _wake) = slot.begin();
            let outcome = catch_up(
                &Arc::downgrade(&slot),
                epoch,
                &runtime,
                &fast_schedule(&root),
                FillBudget::default(),
                &mut |texts| {
                    attempts.push((
                        clock.load(Ordering::SeqCst),
                        lane_pause_chunk_calls() - before,
                    ));
                    model.embed(texts)
                },
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            // The 90 s hint outlasts the fill's 5 s backoff, so it sets the wait.
            assert_eq!(attempts, vec![(START, 1), (START + 90_000, 1)]);
            assert_eq!(lane_pause_chunk_calls() - before, 1);
            assert_eq!(requests.lock().unwrap().len(), 3);
            assert!(!runtime
                .search(&[1.0, 0.0], 10, &|_| true)
                .unwrap()
                .results
                .is_empty());
        });
    });
}

#[test]
fn lane_paused_view_fingerprint_waits_before_checkout_preparation() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock.clone(), None, |semantic, requests| {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            write_tree(root.path(), FILES);
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, wake) = slot.begin();
            let config = WorkerConfig {
                root: root.path().to_path_buf(),
                storage: crate::root_cache::StoragePaths::same(storage.path()),
                family: "family".into(),
                scope: "paused-fingerprint".into(),
                semantic: semantic.clone(),
                trigram_policy: crate::blob_store::v2::TrigramPolicy {
                    max_file_size: crate::config::Config::default().search_index_max_file_size,
                },
                status: Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled)),
                drivers: InstalledDriver::default(),
                schedule: fast_schedule(root.path()),
            };
            let mut model = SemanticEmbeddingModel::from_config(&semantic).unwrap();
            let before = lane_pause_chunk_calls();
            let _policy = PauseRetryScope::new(
                config.schedule.retry_initial,
                config.schedule.retry_max,
                || true,
            );
            let fingerprint = retry_activation(
                &Arc::downgrade(&slot),
                epoch,
                &wake,
                &config,
                "embedding model",
                || model.fingerprint_for_view(&semantic),
            )
            .unwrap();
            assert_eq!(fingerprint.dimension, 2);
            assert_eq!(lane_pause_chunk_calls(), before);
            assert!(config.drivers.get().is_none());
            let times = requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, request)| request["method"] == "embed.query")
                .map(|(time, _)| *time)
                .collect::<Vec<_>>();
            assert_eq!(times, vec![START, START + PAUSE_MS]);
        });
    });
}

#[test]
fn lane_paused_view_wait_releases_admission_refresh_and_family_claims() {
    use crate::semantic_index::{
        with_lane_pause_clock, with_lane_pause_wait_hook, PauseRetryScope,
    };
    use crate::synapse_embed::pause_test_support::*;
    with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock.clone(), None, |config, _requests| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("checkout");
            let storage = directory.path().join("storage");
            write_tree(
                &root,
                &[("pause.ts", "export function released() { return 3; }\n")],
            );
            let runtime = Arc::new(open(&storage, "paused-resources", &root));
            let arena = super::super::semantic_arena::arena_for(&storage, "family");
            let mut model = SemanticEmbeddingModel::from_config(&config).unwrap();
            let slot = Arc::new(CheckoutSemanticSlot::default());
            let (epoch, _wake) = slot.begin();
            let mut schedule = fast_schedule(&root);
            schedule.limiter = crate::cold_build_limiter::isolated_limiter(1);
            let preparation = crate::cold_build_limiter::isolated_limiter(1);
            let cold_probe = schedule.limiter.clone();
            let prep_probe = preparation.clone();
            let runtime_probe = runtime.clone();
            let clock_probe = clock.clone();
            let waited = Arc::new(AtomicUsize::new(0));
            let observed = waited.clone();
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
            let outcome = with_lane_pause_wait_hook(
                move |delay| {
                    let _cold = cold_probe
                        .try_acquire()
                        .expect("pause retained cold admission");
                    let _preparation = prep_probe
                        .try_acquire()
                        .expect("pause retained preparation admission");
                    let _refresh = runtime_probe
                        .refresh
                        .try_lock()
                        .expect("pause retained refresh mutex");
                    assert!(
                        arena.pause_resources_are_free(),
                        "pause retained a family gate or file claim"
                    );
                    observed.fetch_add(1, Ordering::SeqCst);
                    clock_probe.fetch_add(delay.as_millis() as u64, Ordering::SeqCst);
                },
                || {
                    catch_up_with_preparation_limiter(
                        &Arc::downgrade(&slot),
                        epoch,
                        &runtime,
                        &schedule,
                        FillBudget::default(),
                        &mut |texts| model.embed(texts),
                        &preparation,
                    )
                },
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            assert_eq!(waited.load(Ordering::SeqCst), 1);
        });
    });
}

#[test]
fn normal_view_fill_has_zero_checkpoint_work() {
    use crate::semantic_index::{with_checkpoint_work, PauseRetryScope};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    write_tree(&root, FILES);
    let runtime = open(
        &directory.path().join("storage"),
        "normal-no-checkpoints",
        &root,
    );
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, _wake) = slot.begin();
    let model = Model::default();
    let (outcome, work) = with_checkpoint_work(|| {
        let _policy =
            PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
        catch_up(
            &Arc::downgrade(&slot),
            epoch,
            &runtime,
            &fast_schedule(&root),
            FillBudget::default(),
            &mut |texts| model.embed(texts),
        )
    });
    assert!(matches!(outcome, FillOutcome::Settled));
    assert!(model.calls() > 0);
    assert_eq!(
        work,
        Default::default(),
        "healthy view fill copied pause checkpoints"
    );
}

#[test]
fn lane_paused_view_cursor_retains_completed_and_unattempted_chunks() {
    use crate::semantic_index::{lane_pause_chunk_calls, with_lane_pause_clock, PauseRetryScope};
    use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    write_tree(
        &root,
        &[
            ("a.rs", "/// First.\npub fn first(x: u64) -> u64 { x + 1 }\n/// Second.\npub fn second(x: u64) -> u64 { x + 2 }\n"),
            ("b.rs", "/// Third.\npub fn third(x: u64) -> u64 { x + 3 }\n/// Fourth.\npub fn fourth(x: u64) -> u64 { x + 4 }\n"),
            ("c.rs", "/// Fifth.\npub fn fifth(x: u64) -> u64 { x + 5 }\n/// Sixth.\npub fn sixth(x: u64) -> u64 { x + 6 }\n"),
        ],
    );
    let runtime = open(
        &directory.path().join("storage"),
        "paused-owned-cursor",
        &root,
    );
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, _wake) = slot.begin();
    with_lane_pause_clock(START, |clock| {
        let _policy =
            PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true);
        let before = lane_pause_chunk_calls();
        let mut inputs = Vec::new();
        let outcome = catch_up(
            &Arc::downgrade(&slot),
            epoch,
            &runtime,
            &fast_schedule(&root),
            FillBudget {
                max_batch: 1,
                ..FillBudget::default()
            },
            &mut |texts: Vec<String>| {
                inputs.extend(texts.clone());
                if inputs.len() == 2 && clock.load(Ordering::SeqCst) == START {
                    Err(SynapseEmbeddingError::LanePaused(LanePause {
                        lane_id: MODEL.into(),
                        fingerprint: Some("pinned".into()),
                        retry_after_ms: Some(PAUSE_MS),
                        paused_until_ms: START + PAUSE_MS,
                        reason: "maintenance".into(),
                        cause: Default::default(),
                        safe_to_retry_same_request: true,
                        record_id: 0,
                    })
                    .to_string())
                } else {
                    Ok(vec![vec![1.0, 0.0]; texts.len()])
                }
            },
        );
        assert!(matches!(outcome, FillOutcome::Settled));
        assert_eq!(
            runtime
                .search(&[1.0, 0.0], 100, &|_| true)
                .unwrap()
                .results
                .len(),
            9
        );
        assert_eq!(inputs.len(), 10, "a completed vector was embedded twice");
        assert_eq!(inputs[1], inputs[2]);
        assert_eq!(
            lane_pause_chunk_calls() - before,
            3,
            "unattempted prepared files were re-chunked"
        );
    });
}

/// The error a Synapse client returns when the embedding lane refuses a
/// request with `lane_paused`, as the client renders it.
fn lane_pause_error() -> String {
    use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
    SynapseEmbeddingError::LanePaused(LanePause {
        lane_id: MODEL.into(),
        fingerprint: Some("pinned".into()),
        retry_after_ms: Some(PAUSE_MS),
        paused_until_ms: START + PAUSE_MS,
        reason: "maintenance".into(),
        cause: Default::default(),
        safe_to_retry_same_request: true,
        record_id: 0,
    })
    .to_string()
}

/// The root loses its subc route (unbind) while the Synapse embedding lane is
/// paused, then the route comes back. The fill parks instead of stopping, and
/// the wake a rebind sends lets it finish indexing.
#[test]
fn lane_paused_fill_parks_on_disconnect_and_resumes_on_rebind() {
    use crate::semantic_index::{with_lane_pause_wait_hook, PauseRetryScope};
    use std::sync::atomic::AtomicBool;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let observer = FillObserver::new(root.path().to_path_buf());
    let bound = Arc::new(AtomicBool::new(true));
    let model = Arc::new(Model::default());
    let worker = {
        let weak = Arc::downgrade(&slot);
        let bound = Arc::clone(&bound);
        let model = Arc::clone(&model);
        let root = root.path().to_path_buf();
        std::thread::spawn(move || {
            let strict = Arc::clone(&bound);
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true)
                    .with_pause_current(move || strict.load(Ordering::SeqCst));
            let mut paused = false;
            // A pause wait that is not cut short returns at once instead of
            // sleeping out the announced pause.
            with_lane_pause_wait_hook(
                |_| {},
                || {
                    serve_fills(&weak, epoch, &wake, &fast_schedule(&root), &mut |texts| {
                        if !paused {
                            paused = true;
                            bound.store(false, Ordering::SeqCst);
                            return Err(lane_pause_error());
                        }
                        model.embed(texts)
                    });
                },
            );
        })
    };
    let hang_guard = Duration::from_secs(20);
    let parked = observer.rx.recv_timeout(hang_guard).ok();
    if parked != Some("parked") {
        slot.clear();
        worker.join().unwrap();
        panic!("a disconnect during the pause stopped the fill: {parked:?}");
    }
    assert_eq!(model.calls(), 0);
    assert!(slot.is_current(epoch), "the parked lane gave up its slot");
    bound.store(true, Ordering::SeqCst);
    slot.wake();
    let deadline = Instant::now() + hang_guard;
    let finished = loop {
        match observer.rx.recv_deadline(deadline) {
            Ok("wake") => continue,
            event => break event.ok(),
        }
    };
    slot.clear();
    worker.join().unwrap();
    assert_eq!(finished, Some("settled"));
    assert!(model.calls() > 0);
    assert!(!runtime
        .search(&vector("alpha_total"), 100, &|_| true)
        .unwrap()
        .results
        .is_empty());
}

/// The root loses its subc route while the lane is still starting and its
/// embedding-model fingerprint probe was refused with `lane_paused`: the probe
/// parks, and runs again once a rebind wakes the lane.
#[test]
fn lane_paused_activation_parks_on_disconnect_and_resumes_on_rebind() {
    use crate::semantic_index::{with_lane_pause_wait_hook, PauseRetryScope};
    use std::sync::atomic::AtomicBool;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let observer = FillObserver::new(root.path().to_path_buf());
    let bound = Arc::new(AtomicBool::new(true));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let bound = Arc::clone(&bound);
        let root = root.path().to_path_buf();
        let storage = storage.path().to_path_buf();
        std::thread::spawn(move || {
            let config = WorkerConfig {
                root: root.clone(),
                storage: crate::root_cache::StoragePaths::same(&storage),
                family: "family".into(),
                scope: "paused-activation".into(),
                semantic: crate::config::SemanticBackendConfig::default(),
                trigram_policy: crate::blob_store::v2::TrigramPolicy {
                    max_file_size: crate::config::Config::default().search_index_max_file_size,
                },
                status: Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled)),
                drivers: InstalledDriver::default(),
                schedule: fast_schedule(&root),
            };
            let strict = Arc::clone(&bound);
            let _policy =
                PauseRetryScope::new(Duration::from_secs(5), Duration::from_secs(300), || true)
                    .with_pause_current(move || strict.load(Ordering::SeqCst));
            let mut attempts = 0;
            let result = with_lane_pause_wait_hook(
                |_| {},
                || {
                    retry_activation(&weak, epoch, &wake, &config, "embedding model", || {
                        attempts += 1;
                        if attempts == 1 {
                            bound.store(false, Ordering::SeqCst);
                            return Err(lane_pause_error());
                        }
                        Ok(attempts)
                    })
                },
            );
            (result, attempts)
        })
    };
    let parked = observer.rx.recv_timeout(Duration::from_secs(20)).ok();
    if parked != Some("parked") {
        slot.clear();
        let (result, _) = worker.join().unwrap();
        panic!("a disconnect during the pause stopped activation: {parked:?} {result:?}");
    }
    bound.store(true, Ordering::SeqCst);
    slot.wake();
    let (result, attempts) = worker.join().unwrap();
    assert_eq!(result, Ok(2));
    assert_eq!(attempts, 2);
}

#[test]
fn released_lane_frees_only_its_own_epoch() {
    let slot = CheckoutSemanticSlot::default();
    let (replaced, _replaced_wake) = slot.begin();
    let (current, wake) = slot.begin();
    assert!(
        !slot.release(replaced),
        "a replaced worker released its successor"
    );
    assert!(slot.active() && slot.is_current(current));
    assert!(slot.release(current));
    // `active()` now reports no lane, which is what lets the root's next
    // artifact check start a new worker.
    assert!(!slot.active());
    assert!(!slot.is_current(current));
    assert!(
        wake.recv().is_err(),
        "the released worker's channel stayed open"
    );
}

/// Waits up to ten seconds for `done`, failing with `what`.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Idle eviction of a root lets go of its loaded semantic lane. The view
/// unbinds and the family arena drops the run only that view held, while the
/// runs another live view of the family still reads stay resident.
#[test]
fn idle_eviction_releases_the_lane_and_trims_only_its_own_runs() {
    let storage = tempfile::tempdir().unwrap();
    let evicted_root = tempfile::tempdir().unwrap();
    let live_root = tempfile::tempdir().unwrap();
    write_tree(evicted_root.path(), FILES);
    write_tree(
        evicted_root.path(),
        &[(
            "src/delta.rs",
            "pub fn delta_only_here() -> u8 {\n    4\n}\n",
        )],
    );
    write_tree(live_root.path(), FILES);
    let model = Model::default();
    let live = open(storage.path(), "idle-live", live_root.path());
    refresh(&live, &model);

    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    );
    let slot = ctx.checkout_semantic();
    let (epoch, wake) = slot.begin();
    let runtime = Arc::new(
        CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(storage.path()),
            "family",
            "idle-evicted",
            evicted_root.path(),
            producer(),
            Arc::downgrade(slot),
        )
        .unwrap(),
    );
    ctx.installed_checkout_driver()
        .install(Arc::clone(runtime.driver()));
    runtime.load().unwrap();
    refresh(&runtime, &model);
    assert!(slot.install(epoch, Arc::clone(&runtime)));
    assert!(Arc::ptr_eq(runtime.plane(), live.plane()));

    // The root's census row counts the view's own overlay, not the arena.
    let semantic = ctx.memory_root_snapshot().semantic;
    let overlay = runtime.memory().private;
    assert_eq!(
        semantic.counts.get("checkout_overlay_bytes"),
        Some(&overlay)
    );
    assert_eq!(semantic.estimated_bytes, Some(overlay));

    let arena = runtime.plane().arena("family");
    let before = arena.memory();
    assert_eq!(
        before.runs, 4,
        "three shared runs and one of the evicted view's own"
    );
    let released = Arc::downgrade(&runtime);
    drop(runtime);

    assert!(ctx.try_evict_idle_artifacts());
    assert!(
        !slot.active(),
        "idle eviction kept the semantic lane loaded"
    );
    assert!(
        wake.recv().is_err(),
        "the released lane's worker channel stayed open"
    );
    wait_until("the released semantic view was never dropped", || {
        released.upgrade().is_none()
    });
    wait_until("the released view's driver stayed installed", || {
        ctx.installed_checkout_driver().get().is_none()
    });
    // The view unbinds inside the runtime's drop, after its last strong
    // reference is gone, so wait for the trim rather than read it at once.
    wait_until("the arena never trimmed the released view's run", || {
        arena.memory().runs < before.runs
    });
    let after = arena.memory();
    assert_eq!(after.runs, 3, "only the evicted view's own run is dropped");
    assert!(after.bytes < before.bytes);
    assert!(
        query(&live, "alpha_total").complete(),
        "the live view lost a shared run"
    );
}

/// A lane that never finished loading (its worker waiting for a start signal
/// or retrying an unreachable backend) is released by idle eviction too, so
/// its worker exits instead of waiting for as long as the root stays unbound.
#[test]
fn idle_eviction_releases_a_lane_that_is_still_loading() {
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    );
    let slot = ctx.checkout_semantic();
    let (epoch, wake) = slot.begin();
    assert!(slot.active(), "a begun lane reads as loading");

    assert!(ctx.try_evict_idle_artifacts());
    assert!(!slot.active(), "idle eviction kept a loading semantic lane");
    assert!(
        !slot.is_current(epoch),
        "the loading worker's epoch must end"
    );
    assert!(
        wake.recv().is_err(),
        "the loading lane's worker channel stayed open"
    );
}

/// The memory census counts a family's arena runs once, however many views
/// of the family are loaded, and adds them once to the process total.
#[test]
fn census_counts_a_family_arena_once_across_its_views() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_tree(first.path(), FILES);
    write_tree(second.path(), FILES);
    let model = Model::default();
    let a = open(storage.path(), "census-a", first.path());
    let b = open(storage.path(), "census-b", second.path());
    refresh(&a, &model);
    refresh(&b, &model);
    let arena = a.plane().arena("family");
    let memory = arena.memory();
    assert_eq!(memory.runs, 3, "the two views share every run");
    assert!(memory.bytes > 0);

    let snapshot = crate::memory::MemorySnapshot::new("ready", Default::default());
    let storage_label = arena.storage().display().to_string();
    let rows: Vec<_> = snapshot
        .semantic_arenas
        .families
        .iter()
        .filter(|row| row.storage == storage_label)
        .collect();
    assert_eq!(rows.len(), 1, "the family was counted per view: {rows:?}");
    assert_eq!(
        (
            rows[0].family.as_str(),
            rows[0].runs,
            rows[0].chunks,
            rows[0].bytes
        ),
        ("family", memory.runs, memory.chunks, memory.bytes)
    );
    let families = &snapshot.semantic_arenas.families;
    let process = &snapshot.process;
    assert_eq!(
        process.semantic_arena_bytes,
        families.iter().map(|row| row.bytes).sum::<u64>()
    );
    assert_eq!(
        process.semantic_resident_chunks,
        families.iter().map(|row| row.chunks as u64).sum::<u64>()
    );
    assert_eq!(
        process.total_attributed_bytes,
        snapshot.shared_semantic_bases.estimated_bytes.unwrap_or(0)
            + process.semantic_arena_bytes
            + process.sqlite.memory_used_bytes,
        "the arena is not in the process total exactly once"
    );
}

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
    calls: AtomicUsize,
}

impl Model {
    fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|text| vector(text)).collect())
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
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
    let runtime = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage),
        "family",
        scope,
        root,
        producer(),
        Weak::new(),
    )
    .unwrap();
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

/// Every entry's content and semantic plane state in `runtime`'s installed
/// generation.
fn semantic_states(
    runtime: &CheckoutSemantic,
) -> Vec<(
    super::super::RelPath,
    Option<crate::blob_store::v2::ContentHash>,
    Option<super::super::readiness::PlaneState>,
)> {
    runtime
        .installed()
        .generation()
        .entries()
        .map(|(path, entry)| {
            (
                path.clone(),
                entry.content(),
                entry
                    .plane_state(crate::blob_store::v2::FamilyPlane::Semantic)
                    .cloned(),
            )
        })
        .collect()
}

#[test]
fn family_fill_rechecks_completed_work_after_acquiring_claim() {
    let storage = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let files = [("src/file.rs", "pub fn one_chunk() -> u8 { 1 }\n")];
    write_tree(first.path(), &files);
    write_tree(second.path(), &files);
    let a = open(storage.path(), "a", first.path());
    let b = Arc::new(open(storage.path(), "b", second.path()));
    let model = Arc::new(Model::default());
    let callback_model = Arc::clone(&model);
    let callback_b = Arc::clone(&b);
    // Finish the competing fill exactly between a's store miss and claim.
    // No sleep or host scheduling assumption is involved.
    super::super::semantic::before_claim_for_test(move || {
        let report = refresh(&callback_b, &callback_model);
        assert_eq!((report.embedded_keys, report.installed), (1, 1));
    });
    let report = refresh(&a, &model);
    let usage = SemanticRows::of(&a).usage().unwrap();
    eprintln!(
        "completed claim race: texts={} calls={} outer_embedded={} stored_rows={}",
        model.texts(),
        model.calls(),
        report.embedded_keys,
        usage.rows
    );
    assert!(query(&a, "one chunk").complete());
    assert_eq!(
        rows(first.path(), &query(&a, "one chunk").results),
        cold(first.path(), "one chunk").0
    );
    assert_eq!(
        model.texts(),
        1,
        "a completed claim must not embed the same stored key again"
    );
    assert_eq!(model.calls(), 1);
    assert_eq!(report.embedded_keys, 0);
    // One canonical file run, one reusable chunk vector, and the marker row
    // that vouches for the run's vectors matching that chunk row.
    assert_eq!(usage.rows, ROWS_PER_ONE_CHUNK_FILE);
    // Both checkouts name the same content with the same semantic key. Their
    // manifests differ only in what is checkout-specific (the base each one
    // composes and its own publication frontier).
    assert_eq!(semantic_states(&a), semantic_states(&b));
    assert_eq!(
        rows(first.path(), &query(&a, "one chunk").results),
        rows(second.path(), &query(&b, "one chunk").results)
    );
}

#[test]
fn completed_embedding_batches_survive_a_later_batch_failure() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(
        root.path(),
        &[
            ("a.rs", "pub fn first_chunk() -> u8 { 1 }\n"),
            ("b.rs", "pub fn second_chunk() -> u8 { 2 }\n"),
            ("c.rs", "pub fn third_chunk() -> u8 { 3 }\n"),
        ],
    );
    let runtime = open(storage.path(), "batch-failure", root.path());
    let model = Model::default();
    let budget = FillBudget {
        max_batch: 1,
        ..FillBudget::default()
    };
    let report = runtime
        .refresh(budget, &mut |texts| {
            let vectors = model.embed(texts)?;
            if model.calls() == 2 {
                Err("embedding backend interrupted".into())
            } else {
                Ok(vectors)
            }
        })
        .unwrap();
    let store = SemanticRows::of(&runtime);
    eprintln!(
        "later batch failure: texts={} calls={} embedded_keys={} installed={} rows={}",
        model.texts(),
        model.calls(),
        report.embedded_keys,
        report.installed,
        store.usage().unwrap().rows
    );
    assert_eq!(report.errors.len(), 1);
    assert_eq!(
        (report.embedded_keys, report.installed),
        (1, 1),
        "a later batch error must not discard an already complete file run"
    );
    assert_eq!(store.usage().unwrap().rows, ROWS_PER_ONE_CHUNK_FILE);
    runtime
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!(
        model.texts(),
        4,
        "retry must not re-embed the completed first file"
    );
    assert_eq!(model.calls(), 4);
    assert_eq!(store.usage().unwrap().rows, 3 * ROWS_PER_ONE_CHUNK_FILE);
    let answer = query(&runtime, "chunk");
    assert!(answer.complete());
    assert_eq!(
        rows(root.path(), &answer.results),
        cold(root.path(), "chunk").0
    );
}

#[test]
fn repeated_worktree_edits_with_periodic_backend_failures_never_repeat_stored_texts() {
    let _profile = crate::blob_store::v3::work_profile::Report;
    // Eight 64-text batches still cross the seventh-call failure boundary;
    // keep all three siblings and eight edit rounds in both workloads.
    let stress = std::env::var("AFT_STRESS").as_deref() == Ok("1");
    let files = if stress { 2048 } else { 512 };
    let storage = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    for root in &roots {
        for i in 0..files {
            std::fs::write(
                root.path().join(format!("file_{i:04}.rs")),
                format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n"),
            )
            .unwrap();
        }
    }
    let runtimes: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| open(storage.path(), &format!("flaky-worktree-{i}"), root.path()))
        .collect();
    let store = SemanticRows::of(&runtimes[0]);
    let model = Model::default();
    let completed = std::cell::RefCell::new(std::collections::HashSet::new());
    let successful_texts = std::cell::Cell::new(0usize);
    let mut failed_texts = 0usize;
    let mut embed = |texts: Vec<String>| {
        // One chunk per file produces a file row, a chunk-vector row and a
        // verified-run marker row. Each completed request must persist them
        // before another request is sent.
        assert_eq!(
            store.usage().unwrap().rows as usize,
            successful_texts.get() * ROWS_PER_ONE_CHUNK_FILE as usize,
            "completed batches were not persisted before the next request"
        );
        for text in &texts {
            assert!(
                !completed.borrow().contains(text),
                "a stored text was embedded again after retry"
            );
        }
        let count = texts.len();
        let retained = texts.clone();
        let vectors = model.embed(texts)?;
        if model.calls().is_multiple_of(7) {
            failed_texts += count;
            Err("embedding backend interrupted every seventh batch".into())
        } else {
            completed.borrow_mut().extend(retained);
            successful_texts.set(successful_texts.get() + count);
            Ok(vectors)
        }
    };
    let budget = FillBudget {
        max_files: 4096,
        ..FillBudget::default()
    };
    let mut new_keys = 0;
    for (i, runtime) in runtimes.iter().enumerate() {
        let before = model.calls();
        for attempt in 0..64 {
            let report = runtime.refresh(budget, &mut embed).unwrap();
            new_keys += report.embedded_keys;
            if report.errors.is_empty() && report.deferred == 0 {
                break;
            }
            assert!(attempt < 63, "fixture failed to settle");
        }
        assert!(query(runtime, "item").complete());
        // The first worktree embeds; its siblings reuse.
        if i > 0 {
            assert_eq!(
                model.calls(),
                before,
                "a sibling must reuse all stored vectors"
            );
        }
    }
    for round in 0..8 {
        // Change the signature so each revision has genuinely new embed text;
        // body-only edits of these single-line summaries reuse the vector.
        let source = format!(
            "pub fn revised_item_{round}() -> u32 {{ {} }}\n",
            round + 9000
        );
        for (i, (runtime, root)) in runtimes.iter().zip(&roots).enumerate() {
            let file = root.path().join("file_0000.rs");
            std::fs::write(&file, &source).unwrap();
            runtime.driver.record_absolute_change(&file);
            let before = model.calls();
            for attempt in 0..64 {
                let report = runtime.refresh(budget, &mut embed).unwrap();
                new_keys += report.embedded_keys;
                if report.errors.is_empty() && report.deferred == 0 {
                    break;
                }
                assert!(attempt < 63, "fixture failed to settle");
            }
            if i > 0 {
                assert_eq!(
                    model.calls(),
                    before,
                    "same-content sibling edit sent a model request"
                );
            }
            assert!(query(runtime, "revised item").complete());
        }
    }
    drop(embed);
    let (successful, failed, sent, calls) = if stress {
        (2056, 321, 2377, 46)
    } else {
        (520, 65, 585, 18)
    };
    assert_eq!(successful_texts.get(), successful);
    assert_eq!(new_keys, successful);
    assert_eq!(
        store.usage().unwrap().rows,
        successful as u64 * ROWS_PER_ONE_CHUNK_FILE
    );
    assert_eq!(failed_texts, failed);
    assert_eq!(model.texts(), sent);
    assert_eq!(model.calls(), calls);
    for (runtime, root) in runtimes.iter().zip(&roots) {
        assert_eq!(
            rows(root.path(), &query(runtime, "revised item").results),
            cold(root.path(), "revised item").0
        );
    }
    eprintln!("periodic failure, three worktrees: successful texts={} failed texts={failed_texts} sent texts={} calls={} new keys={new_keys} stored rows={}", successful_texts.get(), model.texts(), model.calls(), store.usage().unwrap().rows);
}

#[test]
fn repeated_worktree_edits_embed_only_new_family_keys_even_after_dropped_admission() {
    let _profile = crate::blob_store::v3::work_profile::Report;
    // Two batches retain cross-batch reuse in the default run. Nightly stress
    // keeps the original 32-batch workload; siblings and edit rounds are equal.
    let files = if std::env::var("AFT_STRESS").as_deref() == Ok("1") {
        2048
    } else {
        128
    };
    let storage = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    for root in &roots {
        for i in 0..files {
            std::fs::write(
                root.path().join(format!("file_{i:04}.rs")),
                format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n"),
            )
            .unwrap();
        }
    }
    let runtimes: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| open(storage.path(), &format!("worktree-{i}"), root.path()))
        .collect();
    let model = Model::default();
    let budget = FillBudget {
        max_files: 4096,
        ..FillBudget::default()
    };
    let mut embedded_keys = 0;
    for (i, runtime) in runtimes.iter().enumerate() {
        let report = runtime
            .refresh(budget, &mut |texts| model.embed(texts))
            .unwrap();
        embedded_keys += report.embedded_keys;
        assert_eq!(report.embedded_keys, if i == 0 { files } else { 0 });
        assert_eq!(report.model_calls, if i == 0 { files / 64 } else { 0 });
    }
    let store = SemanticRows::of(&runtimes[0]);
    for runtime in &runtimes[1..] {
        assert_eq!(SemanticRows::of(&runtime).path(), store.path());
    }
    for round in 0..8 {
        // Each revision must change embedded text, not only whole-file bytes.
        let source = format!(
            "pub fn revised_item_{round}() -> u32 {{ {} }}\n",
            round + 9000
        );
        for (i, (runtime, root)) in runtimes.iter().zip(&roots).enumerate() {
            let file = root.path().join("file_0000.rs");
            {
                let _intent = crate::views::intent::record_paths([file.as_path()]);
                std::fs::write(&file, &source).unwrap();
            }
            let before = model.texts();
            let report = runtime
                .refresh(budget, &mut |texts| model.embed(texts))
                .unwrap();
            embedded_keys += report.embedded_keys;
            assert_eq!(
                report.embedded_keys,
                usize::from(i == 0),
                "round={round} worktree={i} report={report:?}"
            );
            assert_eq!(report.model_calls, usize::from(i == 0));
            assert_eq!(
                model.texts() - before,
                usize::from(i == 0),
                "second/third worktree repeated an embedding"
            );
            // Repeated no-op writes to the same content must not send texts.
            let _intent = crate::views::intent::record_paths([file.as_path()]);
            std::fs::write(&file, &source).unwrap();
            drop(_intent);
            let report = runtime
                .refresh(budget, &mut |texts| model.embed(texts))
                .unwrap();
            assert_eq!((report.embedded_keys, report.model_calls), (0, 0));
        }
    }
    assert_eq!(model.texts(), files + 8);
    assert_eq!(model.calls(), files / 64 + 8);
    assert_eq!(embedded_keys, files + 8);
    assert_eq!(
        store.usage().unwrap().rows,
        (files + 8) as u64 * ROWS_PER_ONE_CHUNK_FILE
    );

    // A source completion rejected at admission still stores its immutable
    // payload first. Retry and a sibling must reuse it, not re-embed it.
    let a = &runtimes[0];
    let source = "pub fn admission_was_cancelled() -> u32 { 99999 }\n";
    let file = roots[0].path().join("file_0000.rs");
    std::fs::write(&file, source).unwrap();
    a.driver.record_absolute_change(&file);
    let snapshot = a.bind().unwrap();
    let path = super::super::RelPath::new(b"file_0000.rs".to_vec()).unwrap();
    let mut moved = super::super::snapshot::LiveDelta::new(Arc::clone(snapshot.generation()));
    moved.apply(
        path,
        super::super::snapshot::LiveEntry::new(
            super::super::snapshot::DiskState::of_bytes(b"other bytes"),
            1,
        ),
    );
    let cancelled = moved.snapshot();
    let report = a
        .plane
        .fill(
            &a.owner,
            &snapshot,
            budget,
            &mut |texts| model.embed(texts),
            &|| cancelled.clone(),
        )
        .unwrap();
    assert_eq!(
        (report.embedded_keys, report.installed, report.dropped),
        (1, 0, 1)
    );
    let before = model.texts();
    let retry = a.refresh(budget, &mut |texts| model.embed(texts)).unwrap();
    assert_eq!(
        (retry.embedded_keys, retry.model_calls, retry.installed),
        (0, 0, 1)
    );
    let file = roots[1].path().join("file_0000.rs");
    std::fs::write(&file, source).unwrap();
    runtimes[1].driver.record_absolute_change(&file);
    let sibling = runtimes[1]
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((sibling.embedded_keys, sibling.model_calls), (0, 0));
    assert_eq!(model.texts(), before);
    assert_eq!(model.texts(), files + 9);
    assert_eq!(model.calls(), files / 64 + 9);
    assert_eq!(
        store.usage().unwrap().rows,
        (files + 9) as u64 * ROWS_PER_ONE_CHUNK_FILE
    );
    for (runtime, root) in runtimes.iter().zip(&roots) {
        let answer = query(runtime, "revised item");
        assert!(answer.complete());
        assert_eq!(
            rows(root.path(), &answer.results),
            cold(root.path(), "revised item").0
        );
    }
    eprintln!("three worktrees, repeated edits and cancelled admission: texts={} calls={} stored_rows={} payload_bytes={}", model.texts(), model.calls(), store.usage().unwrap().rows, store.usage().unwrap().payload_bytes);
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
        .flatten()
        .unwrap()
        .entries()
        .filter(|(_, entry)| {
            entry
                .plane_state(crate::blob_store::v2::FamilyPlane::Semantic)
                .is_some_and(|state| state.is_pending())
        })
        .count()
}

fn large_pin_fixture() -> (tempfile::TempDir, tempfile::TempDir, CheckoutSemantic) {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for i in 0..2048 {
        std::fs::write(
            root.path().join(format!("file_{i:04}.rs")),
            format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    let runtime = open(storage.path(), "pins", root.path());
    (storage, root, runtime)
}

/// Fills every file of a large fixture in one round. With
/// `large_pin_fixture` every file is inherited from the checkout's base, so
/// this is the base owner's fill; with `large_own_pin_fixture` it is the
/// checkout's own fill.
fn fill_large(runtime: &CheckoutSemantic) {
    let model = Model::default();
    let report = runtime
        .refresh(
            FillBudget {
                max_files: 4096,
                ..FillBudget::default()
            },
            &mut |texts| model.embed(texts),
        )
        .unwrap();
    assert_eq!(
        (
            report.installed,
            report.embedded_keys,
            report.embedded_texts
        ),
        (2048, 2048, 2048)
    );
}

#[test]
fn large_semantic_fill_batches_pin_writes() {
    use crate::pins::work_counters::{key_file_bytes, key_file_work};
    let (_storage, root, runtime) = large_pin_fixture();
    let (writes, syncs) = key_file_work();
    let bytes = key_file_bytes();
    fill_large(&runtime);
    let after = key_file_work();
    eprintln!(
        "fill pins: writes={} bytes={} key syncs={}",
        after.0 - writes,
        key_file_bytes() - bytes,
        after.1 - syncs
    );
    let answer = query(&runtime, "item");
    assert!(answer.complete());
    assert_eq!(
        rows(root.path(), &answer.results),
        cold(root.path(), "item").0
    );
    assert_eq!((after.0 - writes, after.1 - syncs), (1, 0));
    // File keys and chunk-vector keys share one linear pin write.
    assert_eq!(key_file_bytes() - bytes, 2048 * 2 * 65);
}

/// The large fixture with every file rewritten after the first bind, so the
/// 2048 entries are the checkout's own rather than inherited from its base:
/// their completions are folded into the checkout's worker generation, not
/// published as a base successor.
fn large_own_pin_fixture() -> (tempfile::TempDir, tempfile::TempDir, CheckoutSemantic) {
    let (storage, root, runtime) = large_pin_fixture();
    let paths: Vec<_> = (0..2048)
        .map(|i| root.path().join(format!("file_{i:04}.rs")))
        .collect();
    {
        let _intent = crate::views::intent::record_paths(paths.iter().map(PathBuf::as_path));
        for (i, path) in paths.iter().enumerate() {
            std::fs::write(path, format!("pub fn own_item_{i:04}() -> u32 {{ {i} }}\n")).unwrap();
        }
    }
    (storage, root, runtime)
}

#[test]
fn large_semantic_materialization_batches_pins_and_touches() {
    let _profile = crate::blob_store::v3::work_profile::Report;
    use crate::pins::work_counters::{key_file_bytes, key_file_work};
    let (_storage, root, runtime) = large_own_pin_fixture();
    let publications = crate::blob_store::v3::work_profile::count("semantic_put_publication");
    fill_large(&runtime);
    let publications =
        crate::blob_store::v3::work_profile::count("semantic_put_publication") - publications;
    assert!(
        publications <= 128,
        "2048 file and chunk rows need model batches, not {publications} per-key publications"
    );
    let answer = query(&runtime, "item");
    let (writes, syncs) = key_file_work();
    let bytes = key_file_bytes();
    crate::blob_store::v2::take_touch_transactions();
    runtime.fold().unwrap();
    let after = key_file_work();
    let transactions = crate::blob_store::v2::take_touch_transactions();
    let written_bytes = key_file_bytes() - bytes;
    eprintln!(
        "fold pins: writes={} bytes={} key syncs={} touch transactions={transactions}",
        after.0 - writes,
        written_bytes,
        after.1 - syncs
    );
    assert_eq!(pending_semantic(&runtime), 0);
    assert_eq!(
        rows(root.path(), &query(&runtime, "item").results),
        rows(root.path(), &answer.results)
    );
    let manifest = runtime
        .installed()
        .generation()
        .flatten()
        .unwrap()
        .to_json_bytes()
        .unwrap();
    let store = SemanticRows::of(&runtime);
    let blobs: Vec<_> = runtime
        .installed()
        .generation()
        .flatten()
        .unwrap()
        .ready_keys()
        .filter(|key| key.plane() == crate::blob_store::v2::FamilyPlane::Semantic)
        .map(|key| (key, store.get(&key).unwrap().unwrap()))
        .collect();
    runtime.load().unwrap();
    assert_eq!(
        runtime
            .installed()
            .generation()
            .flatten()
            .unwrap()
            .to_json_bytes()
            .unwrap(),
        manifest
    );
    for (key, payload) in blobs {
        assert_eq!(store.get(&key).unwrap().unwrap(), payload);
    }
    // One key-list write each for the trigram and semantic planes.
    assert_eq!((after.0 - writes, after.1 - syncs), (2, 0));
    // The semantic plane's file and chunk-vector keys plus the trigram
    // plane's keys for the same 2048 entries, each plane in one write.
    assert_eq!(written_bytes, 2048 * 65 * 3);
    // Every key was just stored in this epoch, so folding needs no writer.
    assert_eq!(transactions, 0);
}

/// A large, already-filled checkout must not read or embed unchanged sources
/// again just to publish one edit. Compare to the strict loader and an
/// independent cold index, not to another copy of the fast path.
#[test]
fn large_checkout_edit_counts_source_work_and_preserves_outputs() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    for i in 0..2048 {
        let path = root.join(format!("file_{i:04}.rs"));
        std::fs::write(&path, format!("pub fn item_{i:04}() -> u32 {{ {i} }}\n")).unwrap();
        // Windows refuses to change times through a read-only handle.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(60)),
            )
            .unwrap();
    }
    let runtime = open(storage.path(), "large", &root);
    let model = Model::default();
    let budget = FillBudget {
        max_files: 4096,
        ..FillBudget::default()
    };
    runtime
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    runtime.driver().take_reconcile_work();
    let before = model.texts();
    let target = root.join("file_0000.rs");
    let changed = "pub fn rewritten_item() -> u32 { 9999 }\n";
    {
        let _intent = crate::views::intent::record_paths([target.as_path()]);
        std::fs::write(&target, changed).unwrap();
    }
    let report = runtime
        .refresh(budget, &mut |texts| model.embed(texts))
        .unwrap();
    let work = runtime.driver().take_reconcile_work();
    eprintln!(
        "edit: {work:?}; embedded texts={}, keys={}",
        model.texts() - before,
        report.embedded_keys
    );
    assert_eq!(
        model.texts() - before,
        1,
        "unchanged content was re-embedded"
    );
    let answer = query(&runtime, "rewritten item");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(&root, &answer.results),
        cold(&root, "rewritten item").0
    );
    let snapshot = runtime.installed();
    let manifest = snapshot
        .generation()
        .flatten()
        .unwrap()
        .to_json_bytes()
        .unwrap();
    let store = SemanticRows::of(&runtime);
    let blobs: Vec<_> = snapshot
        .generation()
        .flatten()
        .unwrap()
        .ready_keys()
        .filter(|key| key.plane() == crate::blob_store::v2::FamilyPlane::Semantic)
        .map(|key| (key, store.get(&key).unwrap().unwrap()))
        .collect();
    runtime.load().unwrap();
    assert_eq!(
        runtime
            .installed()
            .generation()
            .flatten()
            .unwrap()
            .to_json_bytes()
            .unwrap(),
        manifest
    );
    for (key, payload) in blobs {
        assert_eq!(store.get(&key).unwrap().unwrap(), payload);
    }
    assert_eq!(
        rows(&root, &query(&runtime, "rewritten item").results),
        rows(&root, &answer.results)
    );
    assert_eq!(
        work.walks, 1,
        "an edit needs one strict membership reconciliation, not fold walks"
    );
    #[cfg(unix)]
    {
        assert_eq!(
            work.reads, 1,
            "unchanged files should cost a stat, not a source read"
        );
        assert_eq!(work.content_hash_bytes, changed.len());
        // One attachment per registered plane (trigram, semantic and call
        // graph) for the one file read.
        assert_eq!(work.attachments, 3);
        assert_eq!(
            work.stats, 2049,
            "one stat per member plus a post-read stability stat"
        );
    }
}

/// A catch-up that takes several budgeted rounds serves each round's vectors
/// at once but publishes them in one fold at the end, because every fold
/// re-walks the whole checkout.
#[test]
fn budgeted_rounds_publish_base_successors_and_never_fold_the_worker_for_base_work() {
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
    // Every file is inherited from the base, so each budgeted round is base
    // owner work: it publishes one readiness successor of the base, and the
    // checkout's own worker generation is never republished for it.
    let generation = runtime.installed().generation().name().to_owned();
    assert_eq!(runtime.base_successor_count(), 1);
    assert_eq!(pending_semantic(&runtime), 2);
    assert_eq!(query(&runtime, "alpha").pending.len(), 2);
    runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!(runtime.installed().generation().name(), generation);
    assert_eq!(pending_semantic(&runtime), 1);
    let last = runtime
        .refresh(one_file, &mut |texts| model.embed(texts))
        .unwrap();
    assert_eq!((last.installed, last.deferred), (1, 0));
    assert_eq!(runtime.base_successor_count(), 3);
    assert_eq!(runtime.installed().generation().name(), generation);
    assert_eq!(pending_semantic(&runtime), 0);
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

#[test]
fn small_chunk_reuse_fill_completes_with_cold_cap_held() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = "pub fn reused() -> u8 { 1 }\n";
    write_tree(root.path(), &[("file.rs", source)]);
    let _watcher = steady_watcher(root.path());
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let embedded = model.texts();
    let path = root.path().join("file.rs");
    std::fs::write(&path, format!("\n\n{source}")).unwrap();
    runtime.driver().record_absolute_change(&path);
    runtime.load().unwrap();
    assert_eq!(pending_semantic(&runtime), 1);
    runtime.driver().take_reconcile_work();
    runtime.plane().take_source_reads();
    let schedule = fast_schedule(root.path());
    let cold = Arc::clone(&schedule.limiter);
    let held = schedule.limiter.try_acquire().unwrap();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            super::super::semantic::before_claim_for_test(|| {
                std::thread::sleep(Duration::from_millis(30));
            });
            let (outcome, lines) = crate::logging::capture_log_lines(|| {
                catch_up(
                    &weak,
                    epoch,
                    &runtime,
                    &schedule,
                    FillBudget::default(),
                    &mut |texts| model.embed(texts),
                )
            });
            tx.send((matches!(outcome, FillOutcome::Settled), lines))
                .unwrap();
        })
    };
    // Decide by what the fill does, not by how long it takes: it either
    // finishes, or it queues on the held cold-build cap, which is the defect.
    // A slow machine only makes the loop run longer; the test runner's
    // overall timeout bounds a genuine hang elsewhere.
    let completed = loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => break Some(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break None,
        }
        if !cold.census().queued.is_empty() {
            slot.clear();
            break None;
        }
    };
    drop(held);
    worker.join().unwrap();
    let (settled, lines) = completed.expect("reuse-only fill waited for the cold-build cap");
    assert!(settled);
    assert_eq!(model.texts(), embedded);
    assert!(query(&runtime, "reused").complete());
    let work = runtime.driver().take_reconcile_work();
    assert_eq!(
        (work.walks, work.reads),
        (0, 0),
        "fold repeated source reconciliation: {work:?}"
    );
    // The v3 owner uses a canonical root, which has a verbatim prefix on
    // Windows. Compare the entire map under that identity so extra reads of
    // this file or any other file cannot go unnoticed.
    let source_reads = runtime.plane().take_source_reads();
    let canonical_path = std::fs::canonicalize(&path).unwrap();
    assert_eq!(source_reads, BTreeMap::from([(canonical_path, 1)]));
    let summary = lines
        .iter()
        .find(|line| line.contains("INFO semantic view fill"))
        .unwrap();
    assert!(
        summary.contains("chunk_reused=1") && summary.contains("calls=0"),
        "{summary}"
    );
    assert!(summary.contains("cold_admitted=false"), "{summary}");
    let lookup_ms: u128 = summary
        .split_whitespace()
        .find_map(|part| part.strip_prefix("file_lookup_ms="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        lookup_ms >= 30,
        "lookup delay was not attributed: {summary}"
    );
    eprintln!(
        "{summary}; reconcile_walks={} reconcile_reads={} fill_sources={source_reads:?}",
        work.walks, work.reads
    );
}

/// Most fills in a running daemon are woken by an edit and start with that
/// edit still unreconciled. This test leaves such an edit in place before
/// `catch_up`. The edit only moved chunks whose vectors are stored, so the fill
/// embeds nothing and must finish without queueing for a cold-build slot.
#[test]
fn unreconciled_reuse_only_edit_completes_with_cold_cap_held() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = "pub fn reused_after_edit() -> u8 { 1 }\n";
    write_tree(root.path(), &[("file.rs", source)]);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let embedded = model.texts();
    let path = root.path().join("file.rs");
    std::fs::write(&path, format!("\n\n{source}")).unwrap();
    runtime.driver().record_absolute_change(&path);
    assert!(
        super::needs_reload(&runtime.installed()),
        "the fixture must leave the edit unreconciled"
    );
    let schedule = fast_schedule(root.path());
    let cold = Arc::clone(&schedule.limiter);
    let held = schedule.limiter.try_acquire().unwrap();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            let (outcome, lines) = crate::logging::capture_log_lines(|| {
                catch_up(
                    &weak,
                    epoch,
                    &runtime,
                    &schedule,
                    FillBudget::default(),
                    &mut |texts| model.embed(texts),
                )
            });
            tx.send((matches!(outcome, FillOutcome::Settled), lines))
                .unwrap();
        })
    };
    // Decide by what the fill does, not by how long it takes: it either
    // finishes, or it queues on the held cold-build cap, which is the defect.
    // The deadline only guards against a worker that stalls before doing
    // either. On expiry the lane is cleared and the held permit released
    // before the test fails; the worker stops at its next currency check.
    let deadline = Instant::now() + Duration::from_secs(60);
    let completed = loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => break Some(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break None,
        }
        if !cold.census().queued.is_empty() {
            slot.clear();
            break None;
        }
        if Instant::now() >= deadline {
            slot.clear();
            drop(held);
            panic!(
                "hang guard: the fill neither finished nor queued on the held \
                 cold-build cap within 60 s"
            );
        }
    };
    drop(held);
    worker.join().unwrap();
    let (settled, lines) =
        completed.expect("reuse-only fill with an unreconciled edit waited for the cold-build cap");
    assert!(settled);
    assert_eq!(model.texts(), embedded, "the edit only moved stored chunks");
    assert!(query(&runtime, "reused").complete());
    let summary = lines
        .iter()
        .find(|line| line.contains("INFO semantic view fill"))
        .unwrap();
    assert!(
        summary.contains("calls=0") && summary.contains("cold_admitted=false"),
        "{summary}"
    );
}

/// More unreconciled edits than the small-preparation file bound still take a
/// cold-build slot before the reconcile starts, as a first load does.
#[test]
fn many_unreconciled_edits_take_cold_slot_before_reconciling() {
    let _profile = crate::blob_store::v3::work_profile::Report;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let files: Vec<(std::path::PathBuf, String)> = (0..=SMALL_PREPARATION_FILES)
        .map(|i| {
            (
                root.path().join(format!("file_{i}.rs")),
                format!("pub fn edited_{i}() {{}}\n"),
            )
        })
        .collect();
    for (path, source) in &files {
        std::fs::write(path, source).unwrap();
    }
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    for (path, source) in &files {
        std::fs::write(path, format!("\n\n{source}")).unwrap();
        runtime.driver().record_absolute_change(path);
    }
    assert!(super::needs_reload(&runtime.installed()));
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let held = limiter.try_acquire().unwrap();
    runtime.driver().take_reconcile_work();
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| model.embed(texts),
            )
        })
    };
    let queued = wait_for_fill_queue(&limiter);
    let work_while_held = runtime.driver().take_reconcile_work();
    drop(held);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert!(queued, "many unreconciled edits bypassed cold admission");
    assert_eq!(
        work_while_held.walks, 0,
        "reconciled before cold admission: {work_while_held:?}"
    );
}

/// One edited file larger than the small-preparation byte bound takes a
/// cold-build slot before the reconcile reads it, even though it is a single
/// path. A small edit still reconciles on the preparation lane; see
/// `unreconciled_reuse_only_edit_completes_with_cold_cap_held`.
#[test]
fn large_unreconciled_edit_takes_cold_slot_before_reconciling() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.rs");
    std::fs::write(&path, "pub fn large_edit() {}\n").unwrap();
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let padding = "// padding\n".repeat((SMALL_PREPARATION_BYTES as usize) / 11 + 1);
    std::fs::write(&path, format!("pub fn large_edit() {{}}\n{padding}")).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > SMALL_PREPARATION_BYTES);
    runtime.driver().record_absolute_change(&path);
    assert!(super::needs_reload(&runtime.installed()));
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let held = limiter.try_acquire().unwrap();
    runtime.driver().take_reconcile_work();
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| model.embed(texts),
            )
        })
    };
    let queued = wait_for_fill_queue(&limiter);
    let work_while_held = runtime.driver().take_reconcile_work();
    drop(held);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert_eq!(
        work_while_held.walks, 0,
        "a large edit was reconciled on the preparation lane while every cold slot was held: \
         {work_while_held:?}"
    );
    assert!(queued, "a large edit bypassed cold admission");
}

/// A fill that leaves work for another budgeted round releases its cold-build
/// slot between rounds. When another request is queued by then, the fill's next
/// round queues behind it instead of retaking the slot it just freed.
#[test]
fn multi_round_fill_yields_to_a_queued_waiter_between_rounds() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(
        root.path(),
        &[
            ("first.rs", "pub fn first_round() {}\n"),
            ("second.rs", "pub fn second_round() {}\n"),
        ],
    );
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let model = Arc::new(Model::default());
    let mut queued_waiter = None;
    let mut queue_holder = None;
    let outcome = catch_up(
        &Arc::downgrade(&slot),
        epoch,
        &runtime,
        &schedule,
        FillBudget {
            max_files: 1,
            ..FillBudget::default()
        },
        &mut |texts| {
            if queued_waiter.is_none() {
                // Occupy the yielded slot so the maintenance request queues
                // before the first round returns from the backend.
                queue_holder = limiter.try_acquire();
                let waiter_limiter = Arc::clone(&limiter);
                let waiter_model = Arc::clone(&model);
                queued_waiter = Some(std::thread::spawn(move || {
                    let permit = crate::cold_build_limiter::acquire_blocking_while_with_limiter(
                        &waiter_limiter,
                        "queued maintenance",
                        || true,
                    )
                    .expect("queued request is admitted");
                    let calls_at_admission = waiter_model.calls();
                    drop(permit);
                    calls_at_admission
                }));
                let deadline = Instant::now() + Duration::from_secs(10);
                while !limiter
                    .census()
                    .queued
                    .iter()
                    .any(|job| job.kind == "queued maintenance")
                {
                    assert!(Instant::now() < deadline, "the other request never queued");
                    std::thread::yield_now();
                }
            }
            let result = model.embed(texts);
            drop(queue_holder.take());
            result
        },
    );
    assert!(matches!(outcome, FillOutcome::Settled));
    assert_eq!(model.calls(), 2, "each file takes its own round");
    let calls_at_admission = queued_waiter
        .expect("the first round queued another request")
        .join()
        .unwrap();
    assert_eq!(
        calls_at_admission, 1,
        "the fill retook the slot for its second round ahead of the queued request"
    );
}

fn wait_for_fill_queue(limiter: &crate::cold_build_limiter::ColdBuildLimiter) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !limiter.census().queued.is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn semantic_fill_releases_cold_slot_during_model_call_and_readmits_before_store() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), &[("file.rs", "pub fn delayed_vector() {}\n")]);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Model::default().embed(texts)
                },
            )
        })
    };
    entered_rx.recv().unwrap();
    let non_semantic = limiter.try_acquire();
    let admitted_during_model_call = non_semantic.is_some();
    if admitted_during_model_call {
        let graph_storage = tempfile::tempdir().unwrap();
        let graph_root = tempfile::tempdir().unwrap();
        write_tree(
            graph_root.path(),
            &[
                ("a.ts", "export function target() {}\n"),
                (
                    "b.ts",
                    "import { target } from './a';\nexport function caller() { target(); }\n",
                ),
            ],
        );
        build_independent_callgraph(graph_root.path(), graph_storage.path());
    }
    let pending_during_call = query(&runtime, "delayed vector").pending.len();
    release_tx.send(()).unwrap();
    let (readmitted, pending_before_store) = if admitted_during_model_call {
        (
            wait_for_fill_queue(&limiter),
            query(&runtime, "delayed vector").pending.len(),
        )
    } else {
        (false, 0)
    };
    drop(non_semantic);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert!(
        admitted_during_model_call,
        "non-semantic cold build was blocked by an embedding wait"
    );
    assert_eq!(pending_during_call, 1, "embedding wait was reported ready");
    assert!(
        readmitted,
        "fill stored vectors without local-work admission"
    );
    assert_eq!(pending_before_store, 1);
    assert!(query(&runtime, "delayed vector").complete());
}

fn build_independent_callgraph(root: &Path, storage: &Path) {
    let graph = super::super::callgraph::CallgraphView::new(
        root.to_path_buf(),
        crate::root_cache::StoragePaths::same(storage),
        "independent-callgraph".into(),
        crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        },
    )
    .unwrap();
    let reader = graph.reader().unwrap();
    assert_eq!(
        reader
            .callers_of(Path::new("a.ts"), "target", 1)
            .unwrap()
            .callers
            .len(),
        1
    );
}

#[test]
fn semantic_fill_inflight_memory_is_bounded_before_large_source_preparation() {
    const ROOTS: usize = 5;
    let storage: Vec<_> = (0..ROOTS).map(|_| tempfile::tempdir().unwrap()).collect();
    let roots: Vec<_> = (0..ROOTS).map(|_| tempfile::tempdir().unwrap()).collect();
    let cold = crate::cold_build_limiter::isolated_limiter(2);
    let preparation = crate::cold_build_limiter::isolated_limiter(PREPARATION_LIMIT);
    let inflight = crate::cold_build_limiter::isolated_limiter(INFLIGHT_FILL_LIMIT);
    let (entered_tx, entered_rx) = crossbeam_channel::unbounded();
    let (release_tx, release_rx) = crossbeam_channel::unbounded();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut lanes = Vec::new();
    let mut workers = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        // More than 32 queued files requires cold admission before source reads.
        for file in 0..33 {
            write_tree(
                root.path(),
                &[(
                    &format!("file_{file}.rs"),
                    &format!("pub fn root_{i}_group_{}() {{}}\n", file % 2),
                )],
            );
        }
        let (slot, epoch, wake, runtime) = served_lane(storage[i].path(), root.path());
        runtime.plane().take_source_reads();
        let mut schedule = fast_schedule(root.path());
        schedule.limiter = Arc::clone(&cold);
        let weak = Arc::downgrade(&slot);
        let worker_runtime = Arc::clone(&runtime);
        let preparation = Arc::clone(&preparation);
        let inflight = Arc::clone(&inflight);
        let entered = entered_tx.clone();
        let release = release_rx.clone();
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        workers.push(std::thread::spawn(move || {
            let mut first_batch = true;
            catch_up_with_limiters(
                &weak,
                epoch,
                &worker_runtime,
                &schedule,
                FillBudget {
                    max_batch: 1,
                    ..FillBudget::default()
                },
                &mut |texts| {
                    assert_eq!(texts.len(), 1, "model request exceeded batch bound");
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    if first_batch {
                        first_batch = false;
                        entered.send(i).unwrap();
                        release.recv().unwrap();
                    }
                    let result = Model::default().embed(texts);
                    active.fetch_sub(1, Ordering::SeqCst);
                    result
                },
                &preparation,
                &inflight,
            )
        }));
        lanes.push((slot, wake, runtime));
    }
    let first: Vec<_> = (0..INFLIGHT_FILL_LIMIT)
        .map(|_| entered_rx.recv().unwrap())
        .collect();
    wait_until("other fills waiting for memory admission", || {
        inflight.census().queued.len() == ROOTS - INFLIGHT_FILL_LIMIT
    });
    assert_eq!(inflight.census().holders.len(), INFLIGHT_FILL_LIMIT);
    assert!(
        cold.census().holders.is_empty(),
        "backend waits held cold slots"
    );
    assert!(
        entered_rx.try_recv().is_err(),
        "third batch entered backend"
    );
    for (i, (_, _, runtime)) in lanes.iter().enumerate() {
        let reads = runtime.plane().take_source_reads();
        assert_eq!(reads.len(), if first.contains(&i) { 33 } else { 0 });
        assert_eq!(query(runtime, "root").pending.len(), 33);
    }
    // Only each root's first call is blocked; later batches still count toward
    // the peak while the same refresh retains its memory admission.
    for _ in 0..ROOTS {
        release_tx.send(()).unwrap();
    }
    for worker in workers {
        assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    }
    assert_eq!(peak.load(Ordering::SeqCst), INFLIGHT_FILL_LIMIT);
    assert!(inflight.census().holders.is_empty());
    assert!(lanes
        .iter()
        .all(|(_, _, runtime)| query(runtime, "root").complete()));
}

#[test]
fn semantic_fill_chunk_gate_waiter_yields_cold_slot_to_returning_backend() {
    semantic_fill_waiter_yields_cold_slot(false);
}

#[test]
fn semantic_fill_file_claim_waiter_yields_cold_slot_to_returning_backend() {
    semantic_fill_waiter_yields_cold_slot(true);
}

fn semantic_fill_waiter_yields_cold_slot(shared_content: bool) {
    let storage = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let cold = crate::cold_build_limiter::isolated_limiter(1);
    let preparation = crate::cold_build_limiter::isolated_limiter(PREPARATION_LIMIT);
    let inflight = crate::cold_build_limiter::isolated_limiter(2);
    let (entered_tx, entered_rx) = crossbeam_channel::unbounded();
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let (done_tx, done_rx) = crossbeam_channel::unbounded();
    let mut lanes = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        for file in 0..33 {
            write_tree(
                root.path(),
                &[(
                    &format!("file_{file}.rs"),
                    &format!(
                        "pub fn sibling_{}() {{}}\n",
                        if shared_content { 0 } else { i }
                    ),
                )],
            );
        }
        let slot = Arc::new(CheckoutSemanticSlot::default());
        let (epoch, wake) = slot.begin();
        let runtime = Arc::new(open(storage.path(), &format!("sibling-{i}"), root.path()));
        assert!(slot.install(epoch, Arc::clone(&runtime)));
        lanes.push((slot, epoch, wake, runtime));
    }
    let mut workers = Vec::new();
    for (i, (slot, epoch, _, runtime)) in lanes.iter().enumerate() {
        let weak = Arc::downgrade(slot);
        let epoch = *epoch;
        let runtime = Arc::clone(runtime);
        let mut schedule = fast_schedule(roots[i].path());
        schedule.limiter = Arc::clone(&cold);
        let preparation = Arc::clone(&preparation);
        let inflight = Arc::clone(&inflight);
        let entered = entered_tx.clone();
        let release = release_rx.clone();
        let done = done_tx.clone();
        workers.push(std::thread::spawn(move || {
            let outcome = catch_up_with_limiters(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| {
                    entered.send(i).unwrap();
                    if i == 0 {
                        release.recv().unwrap();
                    }
                    Model::default().embed(texts)
                },
                &preparation,
                &inflight,
            );
            done.send(matches!(outcome, FillOutcome::Settled)).unwrap();
        }));
        if i == 0 {
            assert_eq!(entered_rx.recv().unwrap(), 0);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let yielded = loop {
        if cold.admission_events().len() >= 2 && cold.census().holders.is_empty() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let both_waiting = done_rx.is_empty();
    release_tx.send(()).unwrap();
    let first_done = done_rx.recv_timeout(Duration::from_secs(10));
    let second_done = done_rx.recv_timeout(Duration::from_secs(10));
    if first_done.is_err() || second_done.is_err() {
        for (slot, _, _, _) in &lanes {
            slot.clear();
        }
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(yielded, "shared-work waiter retained the only cold slot");
    assert!(both_waiting, "sibling did not wait for the in-flight work");
    assert_eq!(first_done.unwrap(), true);
    assert_eq!(second_done.unwrap(), true);
    assert!(lanes
        .iter()
        .all(|(_, _, _, runtime)| query(runtime, "sibling").complete()));
}

#[test]
#[ignore = "fixed-delay before/after measurement; run explicitly with --ignored --nocapture"]
fn semantic_fill_cold_slot_fixed_delay_measurement() {
    const ROOTS: usize = 6;
    const GRAPHS: usize = 8;
    const DELAY: Duration = Duration::from_millis(150);
    let cold = crate::cold_build_limiter::isolated_limiter(2);
    let preparation = crate::cold_build_limiter::isolated_limiter(PREPARATION_LIMIT);
    let inflight = crate::cold_build_limiter::isolated_limiter(INFLIGHT_FILL_LIMIT);
    let storage: Vec<_> = (0..ROOTS).map(|_| tempfile::tempdir().unwrap()).collect();
    let roots: Vec<_> = (0..ROOTS).map(|_| tempfile::tempdir().unwrap()).collect();
    let graph_storage: Vec<_> = (0..GRAPHS).map(|_| tempfile::tempdir().unwrap()).collect();
    let graph_roots: Vec<_> = (0..GRAPHS).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut lanes = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        for file in 0..4 {
            write_tree(
                root.path(),
                &[(
                    &format!("file_{file}.rs"),
                    &format!("pub fn root_{i}_file_{file}() {{}}\n"),
                )],
            );
        }
        lanes.push(served_lane(storage[i].path(), root.path()));
    }
    for root in &graph_roots {
        write_tree(
            root.path(),
            &[
                ("a.ts", "export function target() {}\n"),
                (
                    "b.ts",
                    "import { target } from './a';\nexport function caller() { target(); }\n",
                ),
            ],
        );
    }
    let barrier = Arc::new(std::sync::Barrier::new(ROOTS + 1));
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = crossbeam_channel::unbounded();
    let mut fills = Vec::new();
    for (i, (slot, epoch, _, runtime)) in lanes.iter().enumerate() {
        let weak = Arc::downgrade(slot);
        let epoch = *epoch;
        let runtime = Arc::clone(runtime);
        let mut schedule = fast_schedule(roots[i].path());
        schedule.limiter = Arc::clone(&cold);
        let preparation = Arc::clone(&preparation);
        let inflight = Arc::clone(&inflight);
        let barrier = Arc::clone(&barrier);
        let calls = Arc::clone(&calls);
        let entered = entered_tx.clone();
        fills.push(std::thread::spawn(move || {
            barrier.wait();
            let started = Instant::now();
            let outcome = catch_up_with_limiters(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget {
                    max_batch: 1,
                    ..FillBudget::default()
                },
                &mut |texts| {
                    assert_eq!(texts.len(), 1);
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.send(()).unwrap();
                    std::thread::sleep(DELAY);
                    Model::default().embed(texts)
                },
                &preparation,
                &inflight,
            );
            assert!(matches!(outcome, FillOutcome::Settled));
            assert!(query(&runtime, "root file").complete());
            started.elapsed()
        }));
    }
    barrier.wait();
    // Start unrelated cold builds only after both admitted fills reach the backend.
    entered_rx.recv().unwrap();
    entered_rx.recv().unwrap();
    let mut graphs = Vec::new();
    for (root, storage) in graph_roots.iter().zip(&graph_storage) {
        let root = root.path().to_path_buf();
        let storage = storage.path().to_path_buf();
        let cold = Arc::clone(&cold);
        graphs.push(std::thread::spawn(move || {
            let started = Instant::now();
            let _permit = crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
                &cold,
                "call-graph build",
                &root,
                || true,
            )
            .unwrap();
            let wait = started.elapsed();
            build_independent_callgraph(&root, &storage);
            wait
        }));
    }
    let fill_time = fills
        .into_iter()
        .map(|fill| fill.join().unwrap())
        .max()
        .unwrap();
    let mut waits: Vec<_> = graphs
        .into_iter()
        .map(|graph| graph.join().unwrap())
        .collect();
    waits.sort();
    let p95 = waits[(waits.len() * 95).div_ceil(100) - 1];
    assert_eq!(calls.load(Ordering::SeqCst), ROOTS * 4);
    eprintln!(
        "fixed-delay fixture: semantic_roots={ROOTS} files_per_root=4 max_batch=1 backend_delay_ms={} cold_slots=2 callgraph_builds={GRAPHS} non_semantic_wait_p95_ms={:.3} total_fill_ms={:.3}",
        DELAY.as_millis(), p95.as_secs_f64() * 1000.0, fill_time.as_secs_f64() * 1000.0,
    );
}

#[test]
fn small_missing_vectors_wait_for_cold_slot_and_stay_pending() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), &[("file.rs", "pub fn new_vector() {}\n")]);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let limiter = Arc::clone(&schedule.limiter);
    let held = limiter.try_acquire().unwrap();
    let model = Arc::new(Model::default());
    let worker = {
        let weak = Arc::downgrade(&slot);
        let runtime = Arc::clone(&runtime);
        let model = Arc::clone(&model);
        let limiter = Arc::clone(&limiter);
        std::thread::spawn(move || {
            catch_up(
                &weak,
                epoch,
                &runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| {
                    assert!(
                        limiter.census().holders.is_empty(),
                        "model wait retained a cold slot after preparation admission"
                    );
                    model.embed(texts)
                },
            )
        })
    };
    let queued = wait_for_fill_queue(&limiter);
    let calls_while_held = model.calls();
    let pending_while_held = query(&runtime, "new vector").pending.len();
    let reads_while_held = runtime.plane().take_source_reads();
    drop(held);
    assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    assert!(queued);
    assert_eq!(calls_while_held, 0, "queued fill called the model");
    assert_eq!(pending_while_held, 1, "queued fill was reported as ready");
    let canonical_path = std::fs::canonicalize(root.path().join("file.rs")).unwrap();
    assert_eq!(
        reads_while_held,
        BTreeMap::from([(canonical_path, 1)]),
        "small preparation did not reach the vector miss"
    );
    assert_eq!(model.calls(), 1);
    assert!(query(&runtime, "new vector").complete());
}

#[test]
fn large_fill_size_gate_takes_cold_slot_before_source_preparation() {
    for many_files in [true, false] {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        if many_files {
            for i in 0..33 {
                std::fs::write(
                    root.path().join(format!("file_{i}.rs")),
                    "pub fn large_queue() {}\n",
                )
                .unwrap();
            }
        } else {
            std::fs::write(
                root.path().join("large.rs"),
                format!("pub fn large_input() {{}}\n{}", " ".repeat(256 * 1024)),
            )
            .unwrap();
        }
        let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
        let schedule = fast_schedule(root.path());
        let limiter = Arc::clone(&schedule.limiter);
        let held = limiter.try_acquire().unwrap();
        runtime.plane().take_source_reads();
        let worker = {
            let weak = Arc::downgrade(&slot);
            let runtime = Arc::clone(&runtime);
            std::thread::spawn(move || {
                catch_up(
                    &weak,
                    epoch,
                    &runtime,
                    &schedule,
                    FillBudget {
                        max_files: 1,
                        ..FillBudget::default()
                    },
                    &mut |texts| Model::default().embed(texts),
                )
            })
        };
        let queued = wait_for_fill_queue(&limiter);
        let reads_while_held = runtime.plane().take_source_reads();
        let pending_while_held = query(&runtime, "large").pending.len();
        drop(held);
        assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
        assert!(
            queued,
            "large fill bypassed cold admission: many_files={many_files}"
        );
        assert!(
            reads_while_held.is_empty(),
            "large fill prepared before admission: {reads_while_held:?}"
        );
        assert_eq!(pending_while_held, if many_files { 33 } else { 1 });
        assert!(query(&runtime, "large").complete());
    }
}

#[test]
fn small_preparation_bounds_reads_when_queued_file_grows() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = "pub fn small_input() {}\n";
    write_tree(root.path(), &[("file.rs", source)]);
    // With a steady subscription the refresh does not rebind, so the file
    // grows after the size probe saw it small, which is the case under test.
    let _watcher = steady_watcher(root.path());
    let (_slot, _epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let report = runtime
        .refresh_admitted(
            FillBudget::default(),
            &mut |_| panic!("moved source reached the model"),
            &mut |small| {
                assert!(small, "fixture did not reach small preparation");
                std::fs::write(root.path().join("file.rs"), " ".repeat(1024 * 1024)).unwrap();
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        (
            report.queued,
            report.moved,
            report.installed,
            report.model_calls
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(report.source_reads, 1);
    assert_eq!(
        report.source_bytes,
        source.len() as u64 + 1,
        "small preparation read unbounded growth"
    );
    assert_eq!(query(&runtime, "small input").pending.len(), 1);
}

#[test]
fn fifth_small_fill_waits_for_preparation_capacity_and_stays_pending() {
    let storage: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let roots: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let preparation = crate::cold_build_limiter::isolated_limiter(PREPARATION_LIMIT);
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(5);
    let (release_tx, release_rx) = crossbeam_channel::bounded(5);
    let mut lanes = Vec::new();
    let mut workers = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        write_tree(
            root.path(),
            &[("file.rs", &format!("pub fn small_{i}() {{}}\n"))],
        );
        let (slot, epoch, _wake, runtime) = served_lane(storage[i].path(), root.path());
        let schedule = fast_schedule(root.path());
        let weak = Arc::downgrade(&slot);
        let worker_runtime = Arc::clone(&runtime);
        let preparation = Arc::clone(&preparation);
        let entered = entered_tx.clone();
        let release = release_rx.clone();
        workers.push(std::thread::spawn(move || {
            super::super::semantic::before_claim_for_test(move || {
                entered.send(i).unwrap();
                // Keep the permit until the main thread has observed all pending
                // work, even when fixture setup or queries are slow on Windows.
                // The test runner's overall timeout bounds a genuine hang.
                release.recv().unwrap();
            });
            catch_up_with_preparation_limiter(
                &weak,
                epoch,
                &worker_runtime,
                &schedule,
                FillBudget::default(),
                &mut |texts| Model::default().embed(texts),
                &preparation,
            )
        }));
        lanes.push((slot, runtime));
    }
    drop(entered_tx);
    let first_four: Vec<_> = (0..4)
        .map(|_| entered_rx.recv().expect("fill did not enter preparation"))
        .collect();
    let census = loop {
        let census = preparation.census();
        if !census.queued.is_empty() {
            break census;
        }
        assert!(
            workers.iter().all(|worker| !worker.is_finished()),
            "fill exited before the preparation wait was visible"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let queued = !census.queued.is_empty();
    let holders = census.holders.len();
    let early_fifth = entered_rx.try_recv().ok();
    let pending: Vec<_> = lanes
        .iter()
        .map(|(_, runtime)| query(runtime, "small").pending.len())
        .collect();
    let queued_visible = roots.iter().any(|root| {
        crate::cold_build_limiter::progress::snapshot_for_root(root.path(), 0)
            .queued
            .iter()
            .any(|job| job.kind == PREPARATION_KIND)
    });
    release_tx.send(()).unwrap();
    let fifth = entered_rx.recv().ok();
    for _ in 0..4 {
        release_tx.send(()).unwrap();
    }
    for worker in workers {
        assert!(matches!(worker.join().unwrap(), FillOutcome::Settled));
    }
    assert_eq!(first_four.len(), 4);
    assert!(queued && queued_visible, "preparation wait was not visible");
    assert_eq!(holders, 4);
    assert!(
        early_fifth.is_none(),
        "fifth fill exceeded preparation capacity"
    );
    assert!(
        fifth.is_some_and(|i| !first_four.contains(&i)),
        "fifth fill did not resume after a preparation finished"
    );
    assert_eq!(pending, vec![1; 5], "preparations were reported as filled");
    assert!(lanes
        .iter()
        .all(|(_, runtime)| query(runtime, "small").complete()));
}

/// A real HTTP backend that reserves its port even while offline, so another
/// test cannot steal the address between startup and recovery attempts.
struct RecoveringBackend {
    addr: std::net::SocketAddr,
    socket: Option<tokio::net::TcpSocket>,
    status: Arc<AtomicUsize>,
    probes: Arc<AtomicUsize>,
    texts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RecoveringBackend {
    fn offline() -> Self {
        let (socket, addr) = crate::semantic_index::reserved_refused_backend_for_test();
        Self {
            addr,
            socket: Some(socket),
            status: Arc::new(AtomicUsize::new(0)),
            probes: Arc::new(AtomicUsize::new(0)),
            texts: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    fn config(&self) -> crate::config::SemanticBackendConfig {
        crate::config::SemanticBackendConfig {
            backend: crate::config::SemanticBackend::OpenAiCompatible,
            base_url: Some(format!("http://{}", self.addr)),
            model: "retry-test".into(),
            timeout_ms: 1000,
            ..Default::default()
        }
    }

    fn start(&mut self, status: u16) {
        use std::io::{Read, Write};
        self.status.store(usize::from(status), Ordering::SeqCst);
        if self.worker.is_some() {
            return;
        }
        let listener = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
            .block_on(async {
                self.socket
                    .take()
                    .expect("reserved backend socket")
                    .listen(128)?
                    .into_std()
            })
            .unwrap();
        listener.set_nonblocking(true).unwrap();
        let probes = Arc::clone(&self.probes);
        let texts = Arc::clone(&self.texts);
        let status = Arc::clone(&self.status);
        let stop = Arc::clone(&self.stop);
        self.worker = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("accept test backend connection: {error}"),
                };
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // Windows inherits the listener's nonblocking mode on accept.
                // Use blocking I/O so HTTP reads wait for request bytes.
                stream.set_nonblocking(false).unwrap();
                let response_status = status.load(Ordering::SeqCst) as u16;
                if response_status == 0 {
                    // Keep the port reserved between retries, but close requests
                    // immediately while the test backend is offline.
                    continue;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                let (start, length) = loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let length: usize = String::from_utf8_lossy(&bytes[..end])
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break (end + 4, length);
                        }
                    }
                };
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[start..start + length]).unwrap();
                let inputs = body["input"].as_array().unwrap();
                let data: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, input)| {
                        let text = input.as_str().unwrap();
                        if text == "semantic index fingerprint probe" {
                            probes.fetch_add(1, Ordering::SeqCst);
                        } else {
                            texts.fetch_add(1, Ordering::SeqCst);
                        }
                        serde_json::json!({"index": index, "embedding": vector(text)})
                    })
                    .collect();
                let body = if response_status == 200 {
                    serde_json::json!({"data": data}).to_string()
                } else {
                    "wrong model".to_string()
                };
                write!(stream, "HTTP/1.1 {response_status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }));
    }

    fn shutdown(&mut self) {
        self.status.store(0, Ordering::SeqCst);
    }
}

impl Drop for RecoveringBackend {
    fn drop(&mut self) {
        self.shutdown();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn offline_backend_port_cannot_be_stolen_before_start() {
    let mut backend = RecoveringBackend::offline();
    let refusal = std::net::TcpStream::connect_timeout(&backend.addr, Duration::from_millis(100))
        .unwrap_err();
    // A bound, non-listening socket can refuse a connection or leave its SYN
    // unanswered (as on Darwin). Neither outcome makes the backend available.
    assert!(
        matches!(
            refusal.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
        ),
        "unexpected offline connection error: {refusal}"
    );
    let stolen = std::net::TcpListener::bind(backend.addr);
    if let Ok(stolen) = stolen {
        let start = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backend.start(503);
        }));
        drop(stolen);
        let panic = start.expect_err("a competing listener must block backend startup");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains("AddrInUse"),
            "unexpected startup panic: {message}"
        );
        panic!("the offline backend port was stolen before start");
    }
    let error = stolen.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    backend.start(503);
    backend.shutdown();
}

#[test]
fn transient_fingerprint_failure_recovers_without_edit_or_rebind() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let _watcher = steady_watcher(root.path());
    let mut backend = RecoveringBackend::offline();
    // A previous session already stored every vector. Retrying identity must
    // reuse these vectors, not re-embed the checkout when the backend returns.
    backend.start(200);
    let semantic = backend.config();
    let mut model = crate::semantic_index::EmbeddingModel::from_config(&semantic).unwrap();
    let fingerprint = model.fingerprint(&semantic).unwrap();
    let seed = CheckoutSemantic::new(
        &crate::root_cache::StoragePaths::same(storage.path()),
        "family",
        "scope",
        root.path(),
        SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps),
        Weak::new(),
    )
    .unwrap();
    seed.load().unwrap();
    seed.refresh(FillBudget::default(), &mut |texts| model.embed(texts))
        .unwrap();
    let seed_work = seed.driver().take_reconcile_work();
    let stored_texts = backend.texts.load(Ordering::SeqCst);
    drop(seed);
    drop(model);
    backend.shutdown();
    let observer = FillObserver::new(root.path().to_path_buf());
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let status = Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled));
    let drivers = InstalledDriver::default();
    let config = WorkerConfig {
        root: root.path().to_path_buf(),
        storage: crate::root_cache::StoragePaths::same(storage.path()),
        family: "family".into(),
        scope: "scope".into(),
        semantic: backend.config(),
        trigram_policy: crate::blob_store::v2::TrigramPolicy {
            max_file_size: crate::config::Config::default().search_index_max_file_size,
        },
        status: Arc::clone(&status),
        drivers: drivers.clone(),
        schedule: fast_schedule(root.path()),
    };
    let weak = Arc::downgrade(&slot);
    let worker = std::thread::spawn(move || run_worker(weak, epoch, wake, config));
    assert_eq!(observer.next(), "unavailable");
    assert!(
        matches!(&*status.read().unwrap(), crate::context::SemanticIndexStatus::Failed(reason) if reason.contains("next retry due") && reason.contains("embedding model"))
    );
    // Observe worker transitions, not retry latency: recovery also loads the
    // stored view, and slow I/O must not be mistaken for a latched retry. These
    // event waits have only a hang guard; no edit, wake or rebind drives recovery.
    let hang_guard = Duration::from_secs(60);
    // A second refused connection proves a timer fires without any wake.
    let retried = observer.rx.recv_timeout(hang_guard).ok();
    if retried != Some("unavailable") {
        slot.clear();
        worker.join().unwrap();
        panic!("the failed fingerprint never retried without a wake: {retried:?}");
    }
    assert!(
        drivers.get().is_none(),
        "a failed probe must not walk the checkout"
    );
    backend.start(200);
    let deadline = Instant::now() + hang_guard;
    let recovered = loop {
        match observer.rx.recv_deadline(deadline) {
            Ok("unavailable") => continue,
            event => break event.ok(),
        }
    };
    let runtime = slot.runtime();
    slot.clear();
    worker.join().unwrap();
    assert_eq!(
        recovered,
        Some("settled"),
        "the failed fingerprint latched the semantic lane"
    );
    assert!(matches!(
        *status.read().unwrap(),
        crate::context::SemanticIndexStatus::Ready { .. }
    ));
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        2,
        "one seed probe and one recovery probe"
    );
    assert_eq!(
        backend.texts.load(Ordering::SeqCst),
        stored_texts,
        "recovery re-embedded stored sources"
    );
    let runtime = runtime.unwrap();
    assert!(query(&runtime, "alpha").complete());
    assert!(
        runtime.plane().take_source_reads().is_empty(),
        "recovery fill read sources whose vectors were already stored"
    );
    let work = runtime.driver().take_reconcile_work();
    assert_eq!(work.walks, 1, "failed probes re-walked the checkout");
    assert!(
        work.reads <= seed_work.reads,
        "recovery re-read unchanged files: {work:?}"
    );
}

#[test]
fn permanent_fingerprint_failure_does_not_retry() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let mut backend = RecoveringBackend::offline();
    backend.start(400);
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let status = Arc::new(RwLock::new(crate::context::SemanticIndexStatus::Disabled));
    let config = WorkerConfig {
        root: root.path().to_path_buf(),
        storage: crate::root_cache::StoragePaths::same(storage.path()),
        family: "family".into(),
        scope: "scope".into(),
        semantic: backend.config(),
        trigram_policy: crate::blob_store::v2::TrigramPolicy {
            max_file_size: crate::config::Config::default().search_index_max_file_size,
        },
        status: Arc::clone(&status),
        drivers: InstalledDriver::default(),
        schedule: fast_schedule(root.path()),
    };
    let (done, finished) = crossbeam_channel::bounded(1);
    let weak = Arc::downgrade(&slot);
    let worker = std::thread::spawn(move || {
        run_worker(weak, epoch, wake, config);
        done.send(()).unwrap();
    });
    let stopped = finished.recv_timeout(Duration::from_secs(5)).is_ok();
    let state = slot.state();
    let armed = lock(&slot.retry).is_some();
    slot.clear();
    worker.join().unwrap();
    assert!(stopped, "a permanent failure kept the retry worker alive");
    assert!(!armed, "a permanent failure armed a retry");
    assert!(
        matches!(state, Some(CheckoutSemanticState::Unavailable(reason)) if reason.contains("wrong model") && !reason.contains("next retry"))
    );
    assert!(
        matches!(&*status.read().unwrap(), crate::context::SemanticIndexStatus::Failed(reason) if reason.contains("wrong model"))
    );
    assert_eq!(backend.probes.load(Ordering::SeqCst), 1);
}

#[test]
fn view_fingerprint_retry_sends_one_probe_even_for_server_errors() {
    let mut backend = RecoveringBackend::offline();
    backend.start(503);
    let config = backend.config();
    let mut model = crate::semantic_index::EmbeddingModel::from_config(&config).unwrap();
    let error = model.fingerprint_for_view(&config).unwrap_err();
    assert!(
        crate::semantic_index::embedding_failure_is_transient(&error),
        "{error}"
    );
    assert_eq!(
        backend.probes.load(Ordering::SeqCst),
        1,
        "one scheduled attempt sent multiple probes"
    );
    backend.shutdown();
    backend.start(200);
    model.fingerprint_for_view(&config).unwrap();
    assert_eq!(backend.probes.load(Ordering::SeqCst), 2);
    assert_eq!(backend.texts.load(Ordering::SeqCst), 0);
}

#[test]
fn query_retry_is_rate_limited_and_only_advances_transient_deadlines() {
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (epoch, wake) = slot.begin();
    let weak = Arc::downgrade(&slot);
    assert!(!slot.request_retry_at(Instant::now()));
    schedule_retry(
        &weak,
        epoch,
        "[transient] offline",
        Duration::from_secs(300),
        Duration::from_secs(5),
    );
    let allowed = lock(&slot.retry).as_ref().unwrap().query_allowed_at;
    assert!(!slot.request_retry_at(allowed - Duration::from_nanos(1)));
    assert!(slot.request_retry_at(allowed));
    assert!(
        !slot.request_retry_at(allowed),
        "one query burst bypassed the limiter"
    );
    assert_eq!(lock(&slot.retry).as_ref().unwrap().due, allowed);
    clear_retry(&weak, epoch);
    assert!(!slot.request_retry_at(allowed + Duration::from_secs(10)));
    slot.clear();
    assert!(!wait_retry(&weak, epoch, &wake));
}

#[test]
fn semantic_query_status_sample_expedites_retry_without_a_loaded_runtime() {
    let storage = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config {
            storage_dir: Some(storage.path().to_path_buf()),
            ..Default::default()
        },
    );
    let slot = ctx.checkout_semantic();
    let (epoch, wake) = slot.begin();
    let reason = "semantic: unavailable: embedding model offline".to_owned();
    *ctx.semantic_index_status().write().unwrap() =
        crate::context::SemanticIndexStatus::Failed(reason.clone());
    slot.fail(epoch, reason);
    // Advance only the rate-limit clock, without sleeping or contacting a
    // backend. The query must consume this allowance through the real context.
    let now = Instant::now();
    *lock(&slot.retry) = Some(RetryDeadline {
        due: now + Duration::from_secs(300),
        query_allowed_at: now - Duration::from_secs(1),
    });
    assert!(slot.runtime().is_none());
    assert!(matches!(
        *ctx.semantic_index_status().read().unwrap(),
        crate::context::SemanticIndexStatus::Failed(_)
    ));
    assert_eq!(
        wake.try_recv(),
        Ok(()),
        "query readiness did not wake the failed lane"
    );
    ctx.semantic_index_status();
    assert_eq!(
        wake.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty),
        "query burst bypassed the rate limit"
    );
    slot.clear();
}

#[test]
fn transient_fill_recovery_does_not_reread_or_reembed_stored_content() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let _watcher = steady_watcher(root.path());
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let model = Arc::new(Model::default());
    refresh(&runtime, &model);
    let stored_texts = model.texts();
    let target = root.path().join("src/gamma.rs");
    std::fs::write(&target, "pub fn new_gamma() {}\n").unwrap();
    runtime.driver().record_absolute_change(&target);
    let observer = FillObserver::new(root.path().to_path_buf());
    let online = Arc::new(AtomicBool::new(false));
    let status = Arc::new(RwLock::new(crate::context::SemanticIndexStatus::ready()));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let root = root.path().to_path_buf();
        let online = Arc::clone(&online);
        let model = Arc::clone(&model);
        let status = Arc::clone(&status);
        std::thread::spawn(move || {
            serve_fills_with_status(
                &weak,
                epoch,
                &wake,
                &fast_schedule(&root),
                Some(&status),
                &mut |texts| {
                    if !online.load(Ordering::SeqCst) {
                        Err("[transient] embedding backend unreachable".into())
                    } else {
                        model.embed(texts)
                    }
                },
            );
        })
    };
    assert_eq!(observer.next(), "retry");
    assert!(
        matches!(&*status.read().unwrap(), crate::context::SemanticIndexStatus::Failed(reason) if reason.contains("next retry due"))
    );
    assert!(query(&runtime, "alpha").unavailable.is_some());
    runtime.driver().take_reconcile_work();
    runtime.plane().take_source_reads();
    online.store(true, Ordering::SeqCst);
    // A loaded Windows runner can take more than 5 s to reach the next retry
    // and finish the fill; a fill that never settles still fails here.
    let deadline = Instant::now() + Duration::from_secs(30);
    let settled = loop {
        match observer.rx.recv_deadline(deadline) {
            Ok("retry") => continue,
            event => break event.ok(),
        }
    };
    slot.clear();
    worker.join().unwrap();
    assert_eq!(settled, Some("settled"));
    let work = runtime.driver().take_reconcile_work();
    assert_eq!(
        (work.walks, work.reads),
        (0, 0),
        "retry repeated source work: {work:?}"
    );
    let reads = runtime.plane().take_source_reads();
    let canonical_target = std::fs::canonicalize(&target).unwrap();
    assert_eq!(
        reads.keys().collect::<Vec<_>>(),
        vec![&canonical_target],
        "retry read an unchanged, already-embedded source: {reads:?}"
    );
    assert_eq!(
        model.texts() - stored_texts,
        1,
        "retry re-embedded unchanged sources"
    );
    assert!(query(&runtime, "gamma").complete());
    assert!(matches!(
        *status.read().unwrap(),
        crate::context::SemanticIndexStatus::Ready { .. }
    ));
}

#[test]
fn permanent_fill_failure_preserves_reason_without_retry() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    let observer = FillObserver::new(root.path().to_path_buf());
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = {
        let weak = Arc::downgrade(&slot);
        let root = root.path().to_path_buf();
        let calls = Arc::clone(&calls);
        std::thread::spawn(move || {
            serve_fills(&weak, epoch, &wake, &fast_schedule(&root), &mut |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("embedding dimension mismatch".into())
            });
        })
    };
    let event = observer.next();
    let armed = lock(&slot.retry).is_some();
    slot.clear();
    worker.join().unwrap();
    assert_eq!(event, "settled");
    assert!(!armed, "dimension mismatch armed a retry");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.unavailable_reason().as_deref(),
        Some("embedding dimension mismatch")
    );
}

#[test]
fn live_view_fill_progress_is_visible_before_each_model_call() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let mut calls = 0;
    let mut saw_finished_file = false;
    let outcome = catch_up(
        &Arc::downgrade(&slot),
        epoch,
        &runtime,
        &schedule,
        FillBudget {
            max_files: 1,
            max_batch: 1,
            ..FillBudget::default()
        },
        &mut |texts| {
            calls += 1;
            let snapshot = crate::cold_build_limiter::progress::snapshot_for_root(root.path(), 0);
            let job = snapshot
                .running
                .iter()
                .find(|job| job.root == root.path().to_string_lossy())
                .unwrap();
            assert_eq!(job.kind, "semantic view fill");
            assert_eq!(job.phase, "waiting for embedding backend");
            assert_eq!(job.total, Some(3));
            if calls > 1 {
                assert_eq!(job.chunks_embedded, Some((calls - 1) as u64));
            }
            saw_finished_file |= job.done > 0;
            Ok(texts.iter().map(|text| vector(text)).collect())
        },
    );
    assert!(matches!(outcome, FillOutcome::Settled));
    assert!(saw_finished_file);
    assert!(
        !crate::cold_build_limiter::progress::snapshot_for_root(root.path(), 0)
            .running
            .iter()
            .any(|job| job.root == root.path().to_string_lossy())
    );
}

struct DelayedLocalProvider {
    delay: Duration,
}

impl LocalEmbeddingProvider for DelayedLocalProvider {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        std::thread::sleep(self.delay);
        Ok(texts.iter().map(|text| vector(text)).collect())
    }
}

#[test]
fn semantic_view_fill_summary_reports_backend_embedding_timing() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, _wake, runtime) = served_lane(storage.path(), root.path());
    let schedule = fast_schedule(root.path());
    let delay = Duration::from_millis(30);
    let mut model = SemanticEmbeddingModel::from_local_provider_for_test(
        Box::new(DelayedLocalProvider { delay }),
        root.path().to_path_buf(),
    );
    let mut calls = 0;

    let (outcome, lines) = crate::logging::capture_log_lines(|| {
        catch_up(
            &Arc::downgrade(&slot),
            epoch,
            &runtime,
            &schedule,
            FillBudget {
                max_files: 1,
                max_batch: 1,
                ..FillBudget::default()
            },
            &mut |texts| {
                calls += 1;
                model.embed(texts)
            },
        )
    });

    assert!(matches!(outcome, FillOutcome::Settled));
    assert!(calls > 0, "{lines:?}");
    let summary = lines
        .iter()
        .find(|line| line.contains("INFO semantic view fill"))
        .unwrap_or_else(|| panic!("missing view fill summary in {lines:?}"));
    for field in [
        "elapsed_ms=",
        "embed_ms=",
        "embed_max_ms=",
        "admission_wait_ms=",
        "mutex_wait_ms=",
        "reconcile_ms=",
        "queue_setup_ms=",
        "read_hash_ms=",
        "file_lookup_ms=",
        "chunk_ms=",
        "chunk_wait_ms=",
        "vector_lookup_ms=",
        "store_write_ms=",
        "shared_wait_ms=",
        "fold_ms=",
        "source_reads=",
        "source_bytes=",
    ] {
        assert!(summary.contains(field), "{summary}");
    }
    let field = |name: &str| -> u128 {
        summary
            .split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("missing {name} in {summary}"))
            .parse()
            .unwrap()
    };
    assert!(field("embed_ms") >= delay.as_millis(), "{summary}");
    assert!(field("embed_max_ms") >= delay.as_millis(), "{summary}");
    assert!(field("elapsed_ms") >= field("embed_ms"), "{summary}");
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
            &crate::root_cache::StoragePaths::same(storage),
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

/// A steady, complete watcher subscription for `root`, as a running watcher
/// keeps on Linux and Windows. With one, a verified bind of an unchanged
/// checkout reads only the paths the watcher or an AFT write reported instead
/// of walking the whole checkout, which the source-work assertions of the
/// tests that take one rely on. Without one (macOS, or no watcher) every bind
/// walks the checkout; `no_subscription_refresh_installs_and_answers_complete`
/// covers that case.
fn steady_watcher(root: &Path) -> crate::watcher_backend::subscription::Subscription {
    crate::watcher_backend::subscription::start(
        &std::fs::canonicalize(root).unwrap(),
        crate::watcher_backend::InitialCoverage::Complete,
    )
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
                    return Err("[transient] embedding backend unreachable".to_string());
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

/// Every `CheckoutSemantic::new` holds its view once and every drop releases
/// it once, keyed by the plane's never-reused id rather than its address.
#[test]
fn live_view_holders_match_runtimes_and_planes_have_unique_ids() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let first = open(storage.path(), "holders", root.path());
    let plane = Arc::clone(first.plane());
    let access = first.access().clone();
    assert_eq!(live_holders(&plane, &access), 1);
    let second = open(storage.path(), "holders", root.path());
    assert_eq!(live_holders(&plane, &access), 2);
    drop(first);
    assert_eq!(live_holders(&plane, &access), 1);
    drop(second);
    assert_eq!(live_holders(&plane, &access), 0);

    let id = plane.id();
    drop(plane);
    // A plane built after the previous one was freed, possibly at the same
    // address, still gets an id of its own.
    let replacement = shared_plane(storage.path(), producer());
    assert_ne!(replacement.id(), id);
}

#[test]
fn only_busy_and_checkout_races_are_transient_load_errors() {
    for transient in [
        super::super::first_load::CHECKOUT_CHANGED_DURING_RECONCILE,
        super::super::first_load::CHECKOUT_WRITE_ACTIVE,
        super::super::first_load::CHECKOUT_CHANGED_BEFORE_INSTALL,
        "trigram plane: database is locked",
    ] {
        assert!(is_transient_reason(transient), "{transient}");
    }
    for permanent in [
        "semantic plane: file is not a database",
        "trigram plane: Permission denied (os error 13)",
        "semantic plane: semantic producer mismatch",
        "unsupported persisted format: written by a newer build",
    ] {
        assert!(!is_transient_reason(permanent), "{permanent}");
    }
}

/// A refresh error no timed retry would fix (here the view store cannot be
/// written) becomes the checkout's unavailable reason after one attempt;
/// the worker does not retry it on the backoff. An embedding failure, by contrast, is
/// retried (`transient_embed_errors_retry_until_complete_without_a_wake`).
#[cfg(unix)]
#[test]
fn store_errors_name_the_gap_without_a_retry_loop() {
    use std::os::unix::fs::PermissionsExt;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let (slot, epoch, wake, runtime) = served_lane(storage.path(), root.path());
    // An edit makes the next refresh reload, which publishes into the view
    // directory; an owner-only read-only directory refuses that publication.
    // Public read/execute bits would be repaired when the store is reopened.
    std::fs::write(root.path().join("src/gamma.rs"), "pub fn gamma_edit() {}\n").unwrap();
    runtime
        .driver()
        .record_absolute_change(&root.path().join("src/gamma.rs"));
    // The view's v3 member directory, where its worker generations publish.
    let view_dir =
        crate::blob_store::v3::view_dir(storage.path(), "family", &super::view_scope("scope"))
            .unwrap();
    let original = std::fs::metadata(&view_dir).unwrap().permissions();
    std::fs::set_permissions(&view_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let worker = {
        let weak = Arc::downgrade(&slot);
        let root = root.path().to_path_buf();
        std::thread::spawn(move || {
            let schedule = fast_schedule(&root);
            serve_fills(&weak, epoch, &wake, &schedule, &mut |texts: Vec<String>| {
                Ok(texts.iter().map(|text| vector(text)).collect())
            });
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.unavailable_reason().is_none() {
        assert!(Instant::now() < deadline, "the store error was never named");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The backoff (20 ms doubling to 80 ms) would allow several refresh
    // attempts in this wait if the error were treated as transient.
    std::thread::sleep(Duration::from_millis(500));
    let attempts = runtime.refresh_attempts();
    let answer = query(&runtime, "gamma");
    std::fs::set_permissions(&view_dir, original).unwrap();
    slot.clear();
    worker.join().unwrap();
    assert_eq!(attempts, 1, "a store error was retried on the backoff");
    assert!(!answer.complete());
    assert!(
        answer
            .unavailable
            .as_deref()
            // The v3 member publishes through its SQLite store, which reports
            // the read-only directory as a read-only database.
            .is_some_and(|reason| reason.contains("readonly database")),
        "{answer:?}"
    );
}

/// A worker superseded by a lane restart (here a model change) is held just
/// before its final publication while the new lane publishes; when released,
/// its publication is refused and the view still names the new producer. No
/// wait for the old worker is involved: the fence alone keeps the old
/// producer's manifest from ever replacing the new one.
#[test]
fn superseded_lane_cannot_publish_over_its_replacement() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let lane = |model: &str, slot: &Arc<CheckoutSemanticSlot>, epoch: u64| {
        let runtime = CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(storage.path()),
            "family",
            "scope",
            root.path(),
            SemanticProducer::current(model, EmbedTextCaps::default()),
            Arc::downgrade(slot),
        )
        .unwrap();
        runtime
            .driver()
            .set_publish_fence(Arc::new(LaneFence::new(Arc::downgrade(slot), epoch)));
        Arc::new(runtime)
    };
    let slot = Arc::new(CheckoutSemanticSlot::default());
    let (old_epoch, _old_wake) = slot.begin();
    let old = lane("old-model", &slot, old_epoch);
    old.load().unwrap();

    // The old worker's final fold, held at a barrier before it publishes.
    let (release, barrier) = crossbeam_channel::bounded::<()>(0);
    let final_fold = {
        let old = Arc::clone(&old);
        std::thread::spawn(move || {
            barrier.recv().unwrap();
            old.load()
        })
    };
    let (new_epoch, _new_wake) = slot.begin();
    let new = lane("new-model", &slot, new_epoch);
    new.load().unwrap();
    // An edit only the old lane sees gives its final bind a generation to
    // publish; a bind with nothing new publishes nothing.
    let edited = root.path().join("src/gamma.rs");
    std::fs::write(&edited, "pub fn gamma_superseded() {}\n").unwrap();
    old.driver().record_absolute_change(&edited);
    release.send(()).unwrap();
    let refused = final_fold.join().unwrap();

    let error = refused.expect_err("a superseded lane published its view");
    assert!(
        error.contains(super::super::first_load::SUPERSEDED_PUBLISH),
        "{error}"
    );
    // The member's published generation is still the replacement's.
    let producers = |model: &str| Producers {
        trigram: crate::blob_store::v2::TrigramPolicy {
            max_file_size: crate::config::Config::default().search_index_max_file_size,
        }
        .fingerprint_hex(),
        semantic: Some(SemanticProducer::current(model, EmbedTextCaps::default()).id()),
        callgraph: super::super::callgraph::PRODUCER.into(),
    };
    let member = super::view_scope("scope");
    assert!(super::super::base_refs::open_foreign_v3(
        storage.path(),
        "family",
        &member,
        &producers("new-model")
    )
    .unwrap()
    .is_some());
    assert!(
        super::super::base_refs::open_foreign_v3(
            storage.path(),
            "family",
            &member,
            &producers("old-model")
        )
        .is_err(),
        "the view names the superseded producer"
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
        &crate::root_cache::StoragePaths::same(storage.path()),
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

/// Every file under `path`, relative to it, for a writer-stop inventory.
fn files_under(path: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry = entry.path();
            if entry.is_dir() {
                stack.push(entry);
            } else {
                found.push(entry.strip_prefix(path).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// The semantic view is a v3 member: its first bind bootstraps a base whose
/// semantic entries are pending, the checkout holding the base's claim fills
/// them and publishes a readiness successor, and every later checkout of the
/// same bytes (and a restart) serves those vectors without embedding again.
/// Nothing is written to the v2 view registry or v2 stores.
#[test]
fn semantic_member_base_owner_fills_its_base_and_writes_nothing_outside_v3() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let first_root = std::fs::canonicalize({
        let root = temp.path().join("first");
        write_tree(&root, FILES);
        root
    })
    .unwrap();
    let second_root = std::fs::canonicalize({
        let root = temp.path().join("second");
        write_tree(&root, FILES);
        root
    })
    .unwrap();
    let model = Model::default();
    let (expected, unique_texts) = cold(&first_root, "alpha total");

    let first = open(&storage, "first", &first_root);
    assert!(
        first.installed().generation().base_header().is_some(),
        "a v3 member composes a base"
    );
    // Before the owner fills, the inherited files are pending, never absent.
    let before = query(&first, "alpha total");
    assert!(!before.pending.is_empty(), "{before:?}");
    assert!(!before.complete(), "{before:?}");
    assert!(!first
        .plane()
        .inherited_pending(&first.installed())
        .is_empty());

    refresh(&first, &model);
    assert_eq!(first.base_successor_count(), 1);
    assert!(first
        .plane()
        .inherited_pending(&first.installed())
        .is_empty());
    assert_eq!(model.texts(), unique_texts, "each chunk is embedded once");
    let filled = query(&first, "alpha total");
    assert!(filled.complete(), "{filled:?}");
    assert_eq!(rows(&first_root, &filled.results), expected);
    assert!(first.search_count() >= 2);

    // A second checkout of the same bytes reuses the stored vectors: its
    // base (another root has another base identity) is filled from the
    // family's rows without calling the model.
    let second = open(&storage, "second", &second_root);
    refresh(&second, &model);
    assert_eq!(model.texts(), unique_texts, "the second checkout embedded");
    assert!(second
        .plane()
        .inherited_pending(&second.installed())
        .is_empty());
    let answer = query(&second, "alpha total");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(rows(&second_root, &answer.results), expected);

    // A restart resumes from the published member and the filled base.
    drop(first);
    let restarted = open(&storage, "first", &first_root);
    refresh(&restarted, &model);
    assert_eq!(model.texts(), unique_texts, "the restart embedded");
    let answer = query(&restarted, "alpha total");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(rows(&first_root, &answer.results), expected);

    // Writer stop: no v2 view, v2 store or legacy import ledger.
    let outside_v3 = files_under(&storage)
        .into_iter()
        .filter(|path| {
            !Path::new(path).starts_with(Path::new("blobs").join("v3"))
                && !path.ends_with("reader-floor.json")
        })
        .collect::<Vec<_>>();
    assert_eq!(outside_v3, Vec::<String>::new());
}

#[test]
fn base_successor_rebind_keeps_filled_answers_vouched_without_a_subscription() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    assert!(crate::watcher_backend::subscription::current(root.path()).is_none());
    let runtime = open(storage.path(), "scope", root.path());
    let binds = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&binds);
    *lock(&runtime.before_bind_verify) = Some(Box::new(move |runtime| {
        // The first bind reconciles the refresh's subscription check. The
        // second only picks up the base readiness successor after filling.
        if observed.fetch_add(1, Ordering::SeqCst) == 1 {
            let answer = query(runtime, "alpha");
            assert!(
                answer.complete(),
                "base rebind invalidated filled vectors: {answer:?}"
            );
            assert_eq!(
                rows(runtime.root(), &answer.results),
                cold(runtime.root(), "alpha").0
            );
            // Skipping a redundant check must not hide an actual delivery gap.
            runtime.driver().record_watcher_overflow();
            assert!(query(runtime, "alpha").unvouched);
        }
    }));
    let report = refresh(&runtime, &Model::default());
    assert_eq!(report.deferred, 0, "{report:?}");
    assert_eq!(binds.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.base_successor_count(), 1);
    assert!(query(&runtime, "alpha").complete());
}

/// A checkout without a steady watcher subscription (macOS, or a root whose
/// watcher is not running) is the case where the driver's subscription check
/// marks the checkout overflowed on every check. Searches read the installed
/// cut without that check, so reading answers neither bumps the live epoch nor
/// leaves every answer unvouched, and a refresh after searches and an edit
/// still installs: its bind applies the check once, before it verifies the
/// tree, and its fold installs without repeating it.
#[test]
fn no_subscription_refresh_installs_and_answers_complete() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    assert!(crate::watcher_backend::subscription::current(
        &std::fs::canonicalize(root.path()).unwrap()
    )
    .is_none());
    let runtime = open(storage.path(), "scope", root.path());
    let model = Model::default();
    let first = refresh(&runtime, &model);
    assert_eq!(first.deferred, 0);
    let epoch = runtime.installed().epoch();
    for _ in 0..3 {
        let answer = query(&runtime, "alpha");
        assert!(answer.complete(), "{answer:?}");
        assert_eq!(
            rows(root.path(), &answer.results),
            cold(root.path(), "alpha").0
        );
    }
    assert_eq!(
        runtime.installed().epoch(),
        epoch,
        "a search marked the checkout overflowed"
    );
    let target = root.path().join("src/gamma.rs");
    std::fs::write(
        &target,
        "pub fn gamma_rewritten(input: &str) -> usize {\n    input.len()\n}\n",
    )
    .unwrap();
    runtime.driver().record_absolute_change(&target);
    // Searches between the edit and the refresh must not make the refresh's
    // installs refuse their cut.
    let _ = query(&runtime, "gamma");
    let second = refresh(&runtime, &model);
    assert_eq!(second.deferred, 0);
    assert!(second.installed > 0, "{second:?}");
    let answer = query(&runtime, "gamma rewritten");
    assert!(answer.complete(), "{answer:?}");
    assert_eq!(
        rows(root.path(), &answer.results),
        cold(root.path(), "gamma rewritten").0
    );
    assert_eq!(pending_semantic(&runtime), 0);
}
