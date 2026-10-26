//! The plane adapters on v3 storage: a v3 generation's trigram, callgraph
//! and semantic artifacts are read from and written to the v3 plane stores
//! and the v3 member directory, never the v2 family store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use crate::blob_store::v2::{ContentHash, FamilyPlane, TrigramPolicy};
use crate::blob_store::v3;
use crate::views::base_builder::{build_base, publish_successor, BaseBuild, BaseBuildInput};
use crate::views::base_refs::{
    register_view_in, resolve_base, with_base_barrier, BaseIdentity, WorkerOutcome,
    WorkerPublication,
};
use crate::views::contracts::{PlaneAdapter, ViewAccess};
use crate::views::eviction::DiskBudget;
use crate::views::first_load::{CallgraphBridge, CompositePlane};
use crate::views::manifest_v2::{EntryPlanes, EntryV2, ManifestV2, PublishedFrontier};
use crate::views::readiness::PlaneState;
use crate::views::snapshot::{LiveDelta, LiveEntry};
use crate::views::RelPath;

const FAMILY: &str = "00112233445566ff";
const BASE_FILES: &[(&str, &[u8])] = &[
    ("src/a.rs", b"pub fn alpha() -> u32 { 1 }\n"),
    ("src/b.rs", b"pub fn beta() -> u32 { crate::a::alpha() }\n"),
];
const CHANGED: &[u8] = b"pub fn gamma() -> u32 { 3 }\n";

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
    }
}

fn rel(name: &str) -> RelPath {
    RelPath::new(name.as_bytes().to_vec()).unwrap()
}

fn write_files(root: &Path, files: &[(&str, &[u8])]) -> Vec<RelPath> {
    files
        .iter()
        .map(|(name, bytes)| {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
            rel(name)
        })
        .collect()
}

struct Fixture {
    storage: tempfile::TempDir,
    worker_root: tempfile::TempDir,
    identity: BaseIdentity,
    claim: crate::views::base_builder::BuildClaim,
}

fn fixture() -> Fixture {
    let storage = tempfile::tempdir().unwrap();
    let base_root = tempfile::tempdir().unwrap();
    let paths = write_files(base_root.path(), BASE_FILES);
    let BaseBuild::Published {
        identity, claim, ..
    } = build_base(
        BaseBuildInput {
            storage: storage.path(),
            family: FAMILY,
            root: base_root.path(),
            paths: &paths,
            policy: policy(),
            semantic_producer: None,
            callgraph_producer: crate::views::callgraph::PRODUCER.into(),
            ignore_fingerprint: None,
            history: None,
            budget: DiskBudget::default(),
        },
        None,
    )
    .unwrap()
    else {
        panic!("the only builder publishes")
    };
    let worker_root = tempfile::tempdir().unwrap();
    write_files(worker_root.path(), BASE_FILES);
    write_files(worker_root.path(), &[("src/a.rs", CHANGED)]);
    with_base_barrier(storage.path(), FAMILY, |tx| {
        register_view_in(tx, storage.path(), FAMILY, "worker", worker_root.path()).map(|_| ())
    })
    .unwrap();
    Fixture {
        storage,
        worker_root,
        identity,
        claim,
    }
}

impl Fixture {
    fn route(&self) -> crate::views::snapshot::V3Route {
        crate::views::snapshot::V3Route {
            storage: self.storage.path().to_path_buf(),
            family: FAMILY.into(),
            scope: "worker".into(),
        }
    }

    fn delta(&self) -> ManifestV2 {
        let base = resolve_base(self.storage.path(), FAMILY, &self.identity.scope).unwrap();
        let mut delta = ManifestV2::new_v3(
            base.manifest().header().clone(),
            Some(base.header()),
            PublishedFrontier::default(),
        );
        delta
            .insert(
                rel("src/a.rs"),
                EntryV2::regular(
                    ContentHash::of(CHANGED),
                    CHANGED.len() as u64,
                    EntryPlanes {
                        trigram: Some(PlaneState::pending("not built")),
                        semantic: None,
                        callgraph: Some(PlaneState::pending("not built")),
                    },
                ),
            )
            .unwrap();
        delta
    }

    fn publish(
        &self,
        delta: ManifestV2,
        before_cas: &mut dyn FnMut(&str) -> crate::views::base_refs::Result<()>,
    ) -> (String, Arc<crate::views::snapshot::ComposedGeneration>) {
        let base = resolve_base(self.storage.path(), FAMILY, &self.identity.scope).unwrap();
        let header = base.header();
        drop(base);
        match crate::views::base_refs::publish_worker_with(
            WorkerPublication {
                storage: self.storage.path(),
                family: FAMILY,
                worker_scope: "worker",
                root: self.worker_root.path(),
                base: &header,
                delta,
                previous: None,
            },
            None,
            before_cas,
        )
        .unwrap()
        {
            WorkerOutcome::Published {
                generation,
                composed,
            } => (generation, composed),
            other => panic!("{other:?}"),
        }
    }

    /// A registry reader of the family. Reading a v3 generation never opens
    /// the v2 store it registers in.
    fn reader(&self) -> ViewAccess {
        let registry =
            crate::views::registry::FamilyRegistry::open(self.storage.path(), FAMILY).unwrap();
        ViewAccess::Reader {
            registration: Arc::new(registry.register_reader("v3-plane-test").unwrap()),
            scope: "worker".into(),
        }
    }
}

fn files_named(root: &Path, prefix: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else if entry.file_name().to_string_lossy().starts_with(prefix) {
                found.push(entry.path());
            }
        }
    }
    found
}

#[test]
fn a_v3_trigram_index_builds_the_base_segment_from_payloads_and_overlays_the_delta() {
    let _serial = v3::serial_for_test();
    let fixture = fixture();
    let mut delta = fixture.delta();
    let mut live =
        crate::pins::LivePin::create_v3(fixture.storage.path(), FAMILY, "worker").unwrap();
    crate::views::trigram::materialize_v3_observed(
        fixture.storage.path(),
        FAMILY,
        fixture.worker_root.path(),
        &mut delta,
        &BTreeMap::new(),
        &mut live,
        policy(),
    )
    .unwrap();
    let (_, composed) = fixture.publish(delta, &mut |_| Ok(()));
    let snapshot = LiveDelta::new(composed).snapshot();
    let adapter =
        crate::views::trigram::TrigramAdapter::new(fixture.storage.path().to_path_buf(), policy());
    let index = adapter.open_index(&fixture.reader(), &snapshot).unwrap();
    let members: BTreeSet<RelPath> = snapshot.membership().into_keys().collect();
    let candidates = |literal: &str| index.candidates(&snapshot, &members, literal).indexed;
    assert!(candidates("gamma").contains(&rel("src/a.rs")));
    assert!(candidates("beta").contains(&rel("src/b.rs")));
    assert!(!candidates("alpha() -> u32 { 1").contains(&rel("src/a.rs")));
    // The segment exists only in memory: v3 keeps no segment rows or files.
    assert!(files_named(fixture.storage.path(), "trigram-seg").is_empty());
    assert!(!fixture
        .storage
        .path()
        .join("blobs/v2/00112233445566ff/trigram.sqlite")
        .exists());
}

#[test]
fn a_v3_worker_graph_is_built_from_v3_blobs_into_its_member_directory() {
    let _serial = v3::serial_for_test();
    let fixture = fixture();
    let bridge = CallgraphBridge::default();
    let attachment = |name: &str, bytes: &[u8]| {
        let attachment = bridge.attachment(&rel(name), bytes).unwrap();
        attachment
            .downcast_ref::<crate::views::callgraph::CallgraphAttachment>()
            .unwrap()
            .clone()
    };
    // The base owner completes the base graph: blobs first, then a successor
    // whose derived graph carries the claim's attempt and token.
    let base_graph: Vec<_> = BASE_FILES
        .iter()
        .map(|(name, bytes)| (rel(name), attachment(name, bytes)))
        .collect();
    v3::publish(
        fixture.storage.path(),
        FAMILY,
        1 << 22,
        None,
        |publication| {
            for (_, attachment) in &base_graph {
                publication.put(
                    &attachment.key,
                    &attachment.blob.to_bytes().unwrap(),
                    FamilyPlane::Callgraph.payload_schema(),
                )?;
            }
            Ok(())
        },
    )
    .unwrap();
    let successor = publish_successor(
        &fixture.claim,
        |manifest| {
            for (path, attachment) in &base_graph {
                if let Some(EntryV2::Regular { planes, .. }) = manifest.get_mut(path) {
                    planes.callgraph = Some(PlaneState::ready(&attachment.key));
                }
            }
        },
        None,
    )
    .unwrap();
    let base_dir = v3::view_dir(fixture.storage.path(), FAMILY, &fixture.identity.scope).unwrap();
    let owner = format!(
        "{successor}-a{}-{}",
        fixture.claim.attempt(),
        fixture.claim.token()
    );
    assert!(base_dir.join(format!("derived-{owner}.sqlite")).is_file());
    assert_eq!(
        std::fs::read_to_string(base_dir.join(format!("derived-{successor}.ref"))).unwrap(),
        owner
    );
    // The worker's delta: its changed file's blob goes to v3, and the graph
    // over the composed generation is built in the worker's member directory.
    let registry =
        crate::views::registry::FamilyRegistry::open(fixture.storage.path(), FAMILY).unwrap();
    let owner_registration = registry
        .register_view_with_storage(fixture.storage.path(), "worker", fixture.worker_root.path())
        .unwrap();
    let mut observed = BTreeMap::new();
    let mut entry = LiveEntry::new(crate::views::snapshot::DiskState::of_bytes(CHANGED), 0);
    entry.attachments.insert(
        FamilyPlane::Callgraph,
        bridge.attachment(&rel("src/a.rs"), CHANGED).unwrap(),
    );
    observed.insert(rel("src/a.rs"), entry);
    let mut delta = fixture.delta();
    let mut live =
        crate::pins::LivePin::create_v3(fixture.storage.path(), FAMILY, "worker").unwrap();
    let base = resolve_base(fixture.storage.path(), FAMILY, &fixture.identity.scope)
        .unwrap()
        .into_generation(fixture.storage.path(), FAMILY);
    let route = fixture.route();
    let _staging_pin = crate::pins::AssemblyPin::create(
        &route.member_dir(),
        FAMILY,
        "worker",
        "staging-graph",
        &[],
    )
    .unwrap();
    bridge
        .materialize_v3(
            &owner_registration,
            &route,
            "staging-graph",
            &base,
            &observed,
            &mut delta,
            &mut live,
        )
        .unwrap();
    assert!(matches!(
        delta
            .get(&rel("src/a.rs"))
            .unwrap()
            .plane_state(FamilyPlane::Callgraph),
        Some(PlaneState::Ready { .. })
    ));
    let worker_dir = route.member_dir();
    assert!(worker_dir.join("derived-staging-graph.sqlite").is_file());
    let (generation, composed) = fixture.publish(delta, &mut |generation| {
        bridge
            .finish_generation_v3(&route, "staging-graph", generation)
            .map_err(|error| {
                crate::views::base_refs::BaseRefError::Io(std::io::Error::other(error.reason))
            })
    });
    assert!(worker_dir
        .join(format!("derived-{generation}.sqlite"))
        .is_file());
    // A foreign reader opens the worker's graph from the v3 member directory.
    let access = fixture.reader();
    bridge.adapter.open_generation(&access, &composed).unwrap();
    let snapshot = LiveDelta::new(composed).snapshot();
    bridge.adapter.reader(&access, &snapshot).unwrap();
    drop(owner_registration);
    assert!(!fixture
        .storage
        .path()
        .join("blobs/v2/00112233445566ff/callgraph.sqlite")
        .exists());
}
