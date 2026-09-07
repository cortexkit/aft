use aft::views::{
    ArtifactPlane, ClosureRequirements, Manifest, ManifestEntry, PublicationArtifacts,
    PublicationClosure, PublicationObserver, PublicationRequest, PublicationStep, PublishOutcome,
    RegularPlanes, RelPath, Result, ViewError, ViewStore,
};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use tempfile::tempdir;

fn manifest() -> Manifest {
    Manifest::new([(
        RelPath::new(b"src/main.rs".to_vec()).unwrap(),
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                semantic: Some("semantic-key".to_string()),
                callgraph: Some("callgraph-key".to_string()),
            },
            resolution_input: false,
        },
    )])
    .unwrap()
}

fn sqlite(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE state (value TEXT NOT NULL); INSERT INTO state VALUES ('ready');",
        )
        .unwrap();
}

fn artifacts(root: &Path) -> PublicationArtifacts {
    let blob_semantic = root.join("semantic.sqlite");
    let blob_callgraph = root.join("callgraph.sqlite");
    let derived = root.join("derived.sqlite");
    let aliases = root.join("oid-alias.sqlite");
    for path in [&blob_semantic, &blob_callgraph, &derived, &aliases] {
        sqlite(path);
    }
    let trigram = root.join("trigram.bin");
    fs::write(&trigram, "trigram state").unwrap();
    PublicationArtifacts {
        blob_databases: vec![blob_semantic, blob_callgraph],
        derived_database: derived,
        trigram_artifact: trigram,
        alias_database: aliases,
    }
}

struct CompleteClosure;

impl PublicationClosure for CompleteClosure {
    fn contains_blob(&self, _plane: ArtifactPlane, _full_key: &str) -> Result<bool> {
        Ok(true)
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(true)
    }

    fn contains_alias(&self, _git_oid: &str) -> Result<bool> {
        Ok(true)
    }
}

#[derive(Default)]
struct RecordedSteps(Mutex<Vec<PublicationStep>>);

impl PublicationObserver for RecordedSteps {
    fn reached(&self, step: PublicationStep) {
        self.0.lock().unwrap().push(step);
    }
}

fn request<'a>(
    generation: &'a str,
    base_generation: Option<&'a str>,
    manifest: &'a Manifest,
    artifacts: PublicationArtifacts,
) -> PublicationRequest<'a> {
    PublicationRequest {
        generation,
        base_generation,
        manifest,
        artifacts,
        closure_requirements: ClosureRequirements {
            referenced_aliases: BTreeSet::from(["proven-git-oid".to_string()]),
        },
    }
}

#[test]
fn publication_makes_dependencies_durable_before_sqlite_pointer_visibility() {
    let directory = tempdir().unwrap();
    let store = ViewStore::open(directory.path(), "view-key").unwrap();
    let manifest = manifest();
    let recorded = RecordedSteps::default();

    let durable_inputs = artifacts(directory.path());
    assert_eq!(
        store
            .publish_with_observer(
                &request(
                    "generation-1",
                    None,
                    &manifest,
                    files_for_retry(&durable_inputs),
                ),
                &CompleteClosure,
                Some(&recorded),
            )
            .unwrap(),
        PublishOutcome::Published
    );
    assert_eq!(
        store.current_generation().unwrap().as_deref(),
        Some("generation-1")
    );
    assert_eq!(store.load_manifest("generation-1").unwrap(), manifest);
    assert!(matches!(
        store.publish(
            &request(
                "generation-1",
                Some("generation-1"),
                &manifest,
                durable_inputs,
            ),
            &CompleteClosure,
        ),
        Err(ViewError::ManifestAlreadyExists(generation)) if generation == "generation-1"
    ));
    assert_eq!(
        *recorded.0.lock().unwrap(),
        vec![
            PublicationStep::BlobWalCheckpointed,
            PublicationStep::BlobWalCheckpointed,
            PublicationStep::BlobWalFsync,
            PublicationStep::BlobWalCheckpointed,
            PublicationStep::DerivedAndTrigramDurable,
            PublicationStep::BlobWalCheckpointed,
            PublicationStep::AliasRowsDurable,
            PublicationStep::ClosureProbed,
            PublicationStep::ManifestFileWritten,
            PublicationStep::ManifestParentFsynced,
            PublicationStep::PointerCas,
            PublicationStep::PointerCheckpointed,
            PublicationStep::PointerDatabaseFsynced,
            PublicationStep::PointerDirectoryFsynced,
        ]
    );

    let pragmas = store.pointer_pragmas().unwrap();
    assert_eq!(pragmas.journal_mode, "wal");
    assert_eq!(pragmas.synchronous, 1);
    assert_eq!(pragmas.busy_timeout_ms, 5_000);
    assert_eq!(pragmas.foreign_keys, 0);
}

#[test]
fn same_root_publishers_conflict_then_retry_from_the_winning_base_without_lost_update() {
    let directory = tempdir().unwrap();
    let store = ViewStore::open(directory.path(), "view-key").unwrap();
    let files = artifacts(directory.path());
    let barrier = Arc::new(Barrier::new(2));
    let mut publishers = Vec::new();

    for generation in ["generation-a", "generation-b"] {
        let store = store.clone();
        let files = files.clone();
        let barrier = Arc::clone(&barrier);
        publishers.push(thread::spawn(move || {
            let manifest = manifest();
            barrier.wait();
            store
                .publish(
                    &request(generation, None, &manifest, files),
                    &CompleteClosure,
                )
                .unwrap()
        }));
    }

    let outcomes = publishers
        .into_iter()
        .map(|publisher| publisher.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == PublishOutcome::Published)
            .count(),
        1
    );
    let conflict = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            PublishOutcome::Conflict { current_generation } => current_generation.as_deref(),
            PublishOutcome::Published => None,
        })
        .expect("the competing publisher must observe the winning base");
    assert_eq!(
        store.current_generation().unwrap().as_deref(),
        Some(conflict)
    );
    for generation in ["generation-a", "generation-b"] {
        assert!(store.manifest_path(generation).unwrap().is_file());
    }

    let manifest = manifest();
    assert_eq!(
        store
            .publish(
                &request(
                    "generation-c",
                    Some(conflict),
                    &manifest,
                    files_for_retry(&files),
                ),
                &CompleteClosure,
            )
            .unwrap(),
        PublishOutcome::Published
    );
    assert_eq!(
        store.current_generation().unwrap().as_deref(),
        Some("generation-c")
    );
}

fn files_for_retry(files: &PublicationArtifacts) -> PublicationArtifacts {
    PublicationArtifacts {
        blob_databases: files.blob_databases.clone(),
        derived_database: files.derived_database.clone(),
        trigram_artifact: files.trigram_artifact.clone(),
        alias_database: files.alias_database.clone(),
    }
}
