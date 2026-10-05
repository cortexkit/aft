//! Opt-in callgraph plane exercised through the composite runtime contracts.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aft::blob_store::v2::{FamilyPlane, FamilyStore};
use aft::views::callgraph::{self, BlobReader, CallgraphAttachment, CallgraphPlane};
use aft::views::contracts::{PlaneAdapter, PlaneError, ViewAccess};
use aft::views::first_load::{FirstLoadDriver, QueryState, ReconciledCheckout};
use aft::views::manifest_v2::{
    EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers,
};
use aft::views::readiness::{PlaneReadiness, PlaneState};
use aft::views::registry::FamilyRegistry;
use aft::views::snapshot::{DiskState, LiveDelta, LiveEntry, OpenGeneration, Residency, Snapshot};
use aft::views::RelPath;

const DISABLED: &str = "call graph is disabled (indexes.callgraph=false)";

struct Installed(Snapshot);
impl QueryState for Installed {
    fn installed_state(
        &self,
        _: &ViewAccess,
        _: FamilyPlane,
    ) -> (Snapshot, Vec<std::path::PathBuf>) {
        (self.0.clone(), Vec::new())
    }
}

struct QueryFixture {
    _storage: tempfile::TempDir,
    _root: tempfile::TempDir,
    access: ViewAccess,
    generation: Arc<OpenGeneration>,
    plane: Arc<CallgraphPlane>,
    ctx: aft::context::AppContext,
}

fn query_fixture(with_graph: bool) -> QueryFixture {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let source = b"function target() { return 1; }\nexport function caller() { return target(); }\ncaller();\n";
    std::fs::write(root_path.join("fixture.ts"), source).unwrap();
    let registry = FamilyRegistry::open(storage.path(), "query-callgraph-test").unwrap();
    let access = ViewAccess::Owner(registry.register_view("scope", &root_path).unwrap());
    let driver = Driver {
        plane: CallgraphPlane::default(),
        source: source.to_vec(),
        revision: Mutex::new(1),
        installed: Mutex::new(None),
    };
    let empty = ManifestV2::new(ManifestHeader {
        producers: driver.producers(&access),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    let generation = if with_graph {
        let mut live = LiveDelta::new(Arc::new(OpenGeneration::new("empty", empty, None)));
        for (path, entry) in driver.reconcile(&access).unwrap().entries {
            live.apply(path, entry);
        }
        driver
            .build_own_generation(&access, &live.snapshot(), false)
            .unwrap()
    } else {
        let mut manifest = empty;
        let DiskState::Present { content, size } = DiskState::of_bytes(source) else {
            unreachable!()
        };
        manifest
            .insert(
                RelPath::new(b"fixture.ts").unwrap(),
                EntryV2::regular(content, size, EntryPlanes::default()),
            )
            .unwrap();
        let ViewAccess::Owner(view) = &access else {
            unreachable!()
        };
        let views = view.view_store().unwrap();
        let name = GenerationName::for_manifest(&manifest).unwrap();
        let database = views.derived_path(&name.to_string()).unwrap();
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let store = view.open_store(FamilyPlane::Callgraph).unwrap();
        callgraph::materialize(&database, &manifest, &BlobReader(store.reader())).unwrap();
        let prepared = views.prepare_v2(&name, None, &manifest, None).unwrap();
        views.commit_v2(prepared, None).unwrap();
        let marker =
            aft::root_cache::ReadMarker::create(view.view_dir(), &name.to_string()).unwrap();
        Arc::new(OpenGeneration::new(
            name.to_string(),
            manifest,
            Some(Residency::Marker(marker)),
        ))
    };
    let plane = Arc::new(driver.plane);
    let mut config = aft::config::Config::default();
    config.project_root = Some(root_path);
    config.storage_dir = Some(storage.path().to_path_buf());
    config.views.enabled = true;
    config.indexes.callgraph = true;
    let ctx = aft::context::AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.install_checkout_query_runtime(Arc::new(
        aft::views::query_wait::CheckoutQueryRuntime::new(
            access.clone(),
            Arc::new(Installed(LiveDelta::new(generation.clone()).snapshot())),
            plane.clone(),
        ),
    ));
    QueryFixture {
        _storage: storage,
        _root: root,
        access,
        generation,
        plane,
        ctx,
    }
}

fn graph_answers(ctx: &aft::context::AppContext) -> Vec<aft::protocol::Response> {
    let file = ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .join("fixture.ts");
    let request = |command| {
        serde_json::from_value(serde_json::json!({
            "id": command,
            "command": command,
            "file": file,
            "symbol": "target",
            "expression": "1",
            "toSymbol": "caller",
            "toFile": file,
        }))
        .unwrap()
    };
    vec![
        aft::commands::callers::handle_callers(&request("callers"), ctx),
        aft::commands::impact::handle_impact(&request("impact"), ctx),
        aft::commands::trace_to::handle_trace_to(&request("trace_to"), ctx),
        aft::commands::trace_to_symbol::handle_trace_to_symbol(&request("trace_to_symbol"), ctx),
        aft::commands::trace_data::handle_trace_data(&request("trace_data"), ctx),
    ]
}

#[test]
fn callgraph_query_runtime_keyless_generation_refuses_navigation_as_disabled() {
    let fixture = query_fixture(false);
    let opened = fixture
        .plane
        .open_generation(&fixture.access, &fixture.generation);
    assert!(
        opened.is_err(),
        "a keyless generation must not install an empty reader"
    );
    assert!(opened.unwrap_err().reason.contains(DISABLED));
    for answer in graph_answers(&fixture.ctx) {
        assert!(!answer.success, "{answer:?}");
        assert_eq!(answer.data["code"], "callgraph_unavailable", "{answer:?}");
        assert!(
            answer.data["message"]
                .as_str()
                .unwrap_or_default()
                .contains(DISABLED),
            "{answer:?}"
        );
    }
    let ready = query_fixture(true);
    ready
        .plane
        .open_generation(&ready.access, &ready.generation)
        .unwrap();
    for answer in graph_answers(&ready.ctx) {
        assert!(answer.success, "a keyed generation must answer: {answer:?}");
    }
    let callers = &graph_answers(&ready.ctx)[0];
    assert_eq!(callers.data["total_callers"], 1, "{callers:?}");
}

fn zoom_answer(ctx: &aft::context::AppContext) -> aft::protocol::Response {
    let request = serde_json::from_value(serde_json::json!({
        "id": "zoom",
        "command": "zoom",
        "file": ctx.config().project_root.as_ref().unwrap().join("fixture.ts"),
        "symbol": "target",
        "callgraph": true,
    }))
    .unwrap();
    aft::commands::zoom::handle_zoom(&request, ctx)
}

#[test]
fn callgraph_zoom_observes_checkout_plane_instead_of_legacy_resident_store() {
    let fixture = query_fixture(true);
    fixture
        .plane
        .open_generation(&fixture.access, &fixture.generation)
        .unwrap();
    assert!(fixture.ctx.callgraph_store().read().unwrap().is_none());
    let answer = zoom_answer(&fixture.ctx);
    assert!(answer.success, "{answer:?}");
    assert_ne!(
        answer.data["callgraph"]["status"], "unavailable",
        "{answer:?}"
    );
    assert_eq!(
        answer.data["annotations"]["called_by"][0]["name"], "caller",
        "{answer:?}"
    );
}

#[test]
fn callgraph_zoom_keyless_checkout_plane_names_disabled_generation() {
    let fixture = query_fixture(false);
    let _ = fixture
        .plane
        .open_generation(&fixture.access, &fixture.generation);
    let answer = zoom_answer(&fixture.ctx);
    assert!(answer.success, "{answer:?}");
    assert_eq!(
        answer.data["callgraph"]["status"], "unavailable",
        "{answer:?}"
    );
    assert!(
        answer.data["callgraph"]["index"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains(DISABLED),
        "{answer:?}"
    );
}

struct Driver {
    plane: CallgraphPlane,
    source: Vec<u8>,
    revision: Mutex<u64>,
    installed: Mutex<Option<Snapshot>>,
}
fn error(e: impl std::fmt::Display) -> PlaneError {
    PlaneError {
        plane: FamilyPlane::Callgraph,
        reason: e.to_string(),
    }
}
impl FirstLoadDriver for Driver {
    fn producers(&self, _: &ViewAccess) -> Producers {
        Producers {
            trigram: "test".into(),
            semantic: None,
            callgraph: callgraph::PRODUCER.into(),
        }
    }
    fn head_tree(&self, _: &ViewAccess) -> Option<String> {
        None
    }
    fn reconcile(&self, access: &ViewAccess) -> Result<ReconciledCheckout, PlaneError> {
        if access.is_read_only() {
            return Err(error("reader cannot reconcile"));
        }
        let mut live = LiveEntry::new(DiskState::of_bytes(&self.source), self.revision(access));
        callgraph::attach(&mut live, &self.source, "typescript")?;
        Ok(ReconciledCheckout {
            revision: self.revision(access),
            entries: BTreeMap::from([(RelPath::new(b"fixture.ts").unwrap(), live)]),
        })
    }
    fn revision(&self, _: &ViewAccess) -> u64 {
        *self.revision.lock().unwrap()
    }
    fn build_own_generation(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        seed_derived: bool,
    ) -> Result<Arc<OpenGeneration>, PlaneError> {
        assert!(!seed_derived);
        let ViewAccess::Owner(view) = access else {
            return Err(error("reader cannot publish"));
        };
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: self.producers(access),
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
        let store: FamilyStore = view.open_store(FamilyPlane::Callgraph).map_err(error)?;
        for (path, live) in snapshot.live_entries() {
            let attachment = live.attachments[&FamilyPlane::Callgraph]
                .downcast_ref::<CallgraphAttachment>()
                .unwrap();
            store
                .put_or_touch(&attachment.key, &attachment.blob.to_bytes().map_err(error)?)
                .map_err(error)?;
            let DiskState::Present { content, size } = live.disk else {
                continue;
            };
            manifest
                .insert(
                    path.clone(),
                    EntryV2::regular(
                        content,
                        size,
                        EntryPlanes {
                            callgraph: Some(PlaneState::ready(&attachment.key)),
                            ..EntryPlanes::default()
                        },
                    ),
                )
                .map_err(error)?;
        }
        let name = GenerationName::for_manifest(&manifest).map_err(error)?;
        let views = view.view_store().map_err(error)?;
        let database = views.derived_path(&name.to_string()).map_err(error)?;
        std::fs::create_dir_all(database.parent().unwrap()).map_err(error)?;
        callgraph::materialize(&database, &manifest, &BlobReader(store.reader()))?;
        let prepared = views
            .prepare_v2(&name, None, &manifest, None)
            .map_err(error)?;
        views.commit_v2(prepared, None).map_err(error)?;
        let marker = aft::root_cache::ReadMarker::create(view.view_dir(), &name.to_string())
            .map_err(error)?;
        Ok(Arc::new(OpenGeneration::new(
            name.to_string(),
            manifest,
            Some(Residency::Marker(marker)),
        )))
    }
    fn install(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        revision: u64,
    ) -> Result<(), PlaneError> {
        let actual = self.revision.lock().unwrap();
        if *actual != revision {
            return Err(error("obsolete revision"));
        }
        self.plane.open_generation(access, snapshot.generation())?;
        *self.installed.lock().unwrap() = Some(snapshot.clone());
        Ok(())
    }
}
impl QueryState for Driver {
    fn installed_state(
        &self,
        _: &ViewAccess,
        _: FamilyPlane,
    ) -> (Snapshot, Vec<std::path::PathBuf>) {
        (
            self.installed.lock().unwrap().as_ref().unwrap().clone(),
            Vec::new(),
        )
    }
}

#[test]
fn callgraph_plane_driver_attaches_materializes_and_installs_pinned_reader() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), "callgraph-test").unwrap();
    let access = ViewAccess::Owner(registry.register_view("scope", root.path()).unwrap());
    let driver = Driver { plane: CallgraphPlane::default(), source: b"interface I { m(): void; }\nclass A implements I { m() {} }\nclass B implements I { m() {} }\nexport function caller(x: I) { x.m(); }".to_vec(), revision: Mutex::new(1), installed: Mutex::new(None) };
    let reconciled = driver.reconcile(&access).unwrap();
    let empty = ManifestV2::new(ManifestHeader {
        producers: driver.producers(&access),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    let mut live = LiveDelta::new(Arc::new(OpenGeneration::new("empty", empty, None)));
    for (path, entry) in reconciled.entries {
        live.apply(path, entry);
    }
    let generation = driver
        .build_own_generation(&access, &live.snapshot(), false)
        .unwrap();
    let snapshot = LiveDelta::new(generation.clone()).snapshot();
    driver
        .install(&access, &snapshot, reconciled.revision)
        .unwrap();
    let (installed, gaps) = driver.installed_state(&access, FamilyPlane::Callgraph);
    assert!(gaps.is_empty());
    assert_eq!(
        driver.plane.readiness(&access, &installed),
        PlaneReadiness::Ready {
            pending: 0,
            failed: 0
        }
    );
    let reader = driver.plane.reader(&access, &installed).unwrap();
    assert_eq!(reader.store.reader_kind(), "view");
    let callers = reader
        .store
        .callers_of(std::path::Path::new("fixture.ts"), "A::m", 1)
        .unwrap();
    assert_eq!(callers.callers.len(), 1);
    assert_eq!(callers.callers[0].provenance, "dispatch");
    assert_eq!(
        callers.callers[0].supplemental_resolution(),
        Some("possible_target (dispatch)")
    );
    *driver.revision.lock().unwrap() = 2;
    assert!(driver.install(&access, &snapshot, 1).is_err());
    driver.plane.release_generation(&access, generation.name());
    assert!(driver.plane.reader(&access, &installed).is_err());
    assert_eq!(
        reader.generation.name(),
        generation.name(),
        "held reader keeps old generation pinned"
    );
}

#[test]
fn store_clone_pins_old_blobs_after_swap_and_gc() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), "callgraph-pin-test").unwrap();
    let view = registry.register_view("scope-pin", root.path()).unwrap();
    let access = ViewAccess::Owner(view.clone());
    let driver = Driver {
        plane: CallgraphPlane::default(),
        source: b"function target() {} export function caller() { target(); }".to_vec(),
        revision: Mutex::new(1),
        installed: Mutex::new(None),
    };
    let reconciled = driver.reconcile(&access).unwrap();
    let empty = ManifestV2::new(ManifestHeader {
        producers: driver.producers(&access),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    let mut live = LiveDelta::new(Arc::new(OpenGeneration::new("empty", empty.clone(), None)));
    for (path, entry) in reconciled.entries {
        live.apply(path, entry);
    }
    let old = driver
        .build_own_generation(&access, &live.snapshot(), false)
        .unwrap();
    let old_snapshot = LiveDelta::new(old.clone()).snapshot();
    driver.install(&access, &old_snapshot, 1).unwrap();
    let reader = driver.plane.reader(&access, &old_snapshot).unwrap();
    let held_store = reader.store.clone();
    let old_name = old.name().to_string();
    let key_hex = old
        .manifest()
        .entries()
        .find_map(|(_, e)| match e.plane_state(FamilyPlane::Callgraph) {
            Some(PlaneState::Ready { key }) => Some(key.clone()),
            _ => None,
        })
        .unwrap();
    let key = aft::blob_store::v2::FamilyKey::new(
        FamilyPlane::Callgraph,
        aft::blob_store::v2::parse_hex32(&key_hex).unwrap(),
    );
    let views = view.view_store().unwrap();
    let name = GenerationName::for_manifest(&empty).unwrap();
    let database = views.derived_path(&name.to_string()).unwrap();
    let store = view.open_store(FamilyPlane::Callgraph).unwrap();
    callgraph::materialize(&database, &empty, &BlobReader(store.reader())).unwrap();
    let prepared = views
        .prepare_v2(&name, Some(&old_name), &empty, None)
        .unwrap();
    views.commit_v2(prepared, None).unwrap();
    let marker = aft::root_cache::ReadMarker::create(view.view_dir(), &name.to_string()).unwrap();
    let successor = LiveDelta::new(Arc::new(OpenGeneration::new(
        name.to_string(),
        empty,
        Some(Residency::Marker(marker)),
    )))
    .snapshot();
    driver.install(&access, &successor, 1).unwrap();
    driver.plane.release_generation(&access, &old_name);
    drop((reader, old_snapshot, old));
    let policy = aft::gc::family::FamilySweepPolicy { byte_budget: 0 };
    aft::gc::family::sweep_family(&registry, None, policy, None).unwrap();
    assert!(
        store.contains(&key).unwrap(),
        "a bare store clone must protect its generation blobs after cache release"
    );
    let callers = held_store
        .callers_of(std::path::Path::new("fixture.ts"), "target", 1)
        .unwrap();
    assert_eq!(callers.callers.len(), 1);
    drop(held_store);
    aft::gc::family::sweep_family(&registry, None, policy, None).unwrap();
    assert!(
        !store.contains(&key).unwrap(),
        "last store clone releases the old generation pin"
    );
}
