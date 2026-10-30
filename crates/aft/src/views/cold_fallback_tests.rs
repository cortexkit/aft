//! Publications that cannot or should not patch the current generation build
//! the derived database cold instead of failing.
use super::*;

const FAMILY: &str = "cold-family";
const SCOPE: &str = "cold-scope";

fn git(project: &Path, args: &[&str]) {
    assert!(std::process::Command::new("git")
        .current_dir(project)
        .args(args)
        .status()
        .unwrap()
        .success());
}

/// A committed TypeScript repository with one cross-file call per pair.
pub(super) fn repository(files: usize) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    for index in 0..files {
        fs::write(
            project.path().join(format!("file_{index}.ts")),
            format!(
                "import {{ target_{next} }} from './file_{next}';\n\
                 export function target_{index}() {{ return {index}; }}\n\
                 export function caller_{index}() {{ return target_{next}(); }}\n",
                next = (index + 1) % files
            ),
        )
        .unwrap();
    }
    git(project.path(), &["init", "--quiet"]);
    git(project.path(), &["add", "."]);
    git(
        project.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    );
    project
}

fn request(project: &Path, storage: &Path, head: &str, changed: &[String]) -> AssemblyRequest {
    AssemblyRequest {
        storage: storage.to_path_buf(),
        project_root: project.to_path_buf(),
        family: FAMILY.into(),
        scope: SCOPE.into(),
        desired_head: head.into(),
        changed_paths: changed
            .iter()
            .map(|path| path.as_bytes().to_vec())
            .collect(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    }
}

#[test]
fn views_committed_sqlite_members_are_excluded_before_reading() {
    let project = repository(1);
    let root = project.path().canonicalize().unwrap();
    fs::create_dir(root.join("fixtures")).unwrap();
    for name in [
        "example.db",
        "example.sqlite",
        "example.sqlite3",
        "example.db-wal",
        "example.db-shm",
        "example.db-journal",
    ] {
        fs::write(root.join("fixtures").join(name), "not project source").unwrap();
    }
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixtures",
        ],
    );
    let storage = tempfile::tempdir().unwrap();
    let probe = crate::project_content::Probe::new(&root);
    let report = publish_checkout(&request(&root, storage.path(), "fixtures", &[])).unwrap();
    assert!(report.published);
    let view = ViewStore::open(storage.path(), SCOPE).unwrap();
    let manifest = view
        .load_manifest(report.generation.as_deref().unwrap())
        .unwrap();
    let paths: Vec<_> = manifest
        .entries()
        .map(|(path, _)| path.as_bytes().to_vec())
        .collect();
    assert_eq!(paths, vec![b"file_0.ts".to_vec()]);
    assert_eq!(probe.paths(), vec![root.join("file_0.ts")]);
}

#[test]
fn views_newly_excluded_storage_members_are_not_reused() {
    let project = repository(1);
    let root = project.path().canonicalize().unwrap();
    let internal = root.join("internal");
    fs::create_dir(&internal).unwrap();
    fs::write(
        internal.join("generated.ts"),
        "export function generated() {}\n",
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "generated",
        ],
    );
    let storage = tempfile::tempdir().unwrap();
    let initial =
        publish_checkout(&request(&root, storage.path(), "before-exclusion", &[])).unwrap();
    assert_eq!(initial.manifest.unwrap().entries().count(), 2);
    crate::project_content::register_storage_root(&internal);
    let probe = crate::project_content::Probe::new(&root);
    let report = publish_checkout(&request(
        &root,
        storage.path(),
        "after-exclusion",
        &["file_0.ts".into()],
    ))
    .unwrap();
    assert!(report.published);
    let paths: Vec<_> = report
        .manifest
        .unwrap()
        .entries()
        .map(|(path, _)| path.as_bytes().to_vec())
        .collect();
    assert_eq!(paths, vec![b"file_0.ts".to_vec()]);
    assert_eq!(probe.paths(), vec![root.join("file_0.ts")]);
}

// Windows cannot compare file identities here and refuses known SQLite names, not this `.rs` hard-link alias.
#[cfg(unix)]
#[test]
fn views_live_sqlite_hard_link_is_skipped_before_read() {
    assert_live_sqlite_hard_link_is_skipped(&[]);
}

// Windows cannot compare file identities here and refuses known SQLite names, not this `.rs` hard-link alias.
#[cfg(unix)]
#[test]
fn views_live_sqlite_hard_link_is_skipped_before_reuse() {
    assert_live_sqlite_hard_link_is_skipped(&["file_0.ts".to_string()]);
}

#[cfg(unix)]
fn assert_live_sqlite_hard_link_is_skipped(changed: &[String]) {
    let project = repository(1);
    let root = project.path().canonicalize().unwrap();
    let alias = root.join("alias.rs");
    fs::write(&alias, "fn ordinary_source() {}\n").unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "source alias",
        ],
    );
    let storage = tempfile::tempdir().unwrap();
    let initial = publish_checkout(&request(&root, storage.path(), "source", &[])).unwrap();
    assert_eq!(initial.manifest.unwrap().entries().count(), 2);

    let database = root.join("live.db");
    let connection =
        crate::db::file_identity::IdentityConnection::open(&database, "view_alias").unwrap();
    connection
        .execute_batch("CREATE TABLE probe(value TEXT)")
        .unwrap();
    fs::remove_file(&alias).unwrap();
    fs::hard_link(&database, &alias).unwrap();
    let probe = crate::project_content::Probe::new(&root);
    let report = publish_checkout(&request(&root, storage.path(), "live", changed)).unwrap();
    assert!(report.published);
    let paths = report
        .manifest
        .unwrap()
        .entries()
        .map(|(path, _)| path.as_bytes().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec![b"file_0.ts".to_vec()]);
    assert!(!probe.paths().contains(&alias));
}

#[test]
fn empty_desired_head_does_not_take_the_no_op_path() {
    let project = repository(2);
    let storage = tempfile::tempdir().unwrap();
    let first = publish_checkout(&request(project.path(), storage.path(), "head", &[])).unwrap();
    assert!(first.published);
    let second = publish_checkout(&request(project.path(), storage.path(), "", &[])).unwrap();
    assert!(second.published);
    assert_ne!(second.generation, first.generation);
}

#[test]
fn competing_publication_is_reported_as_conflict_not_corruption() {
    let project = repository(2);
    let storage = tempfile::tempdir().unwrap();
    publish_checkout(&request(project.path(), storage.path(), "base", &[])).unwrap();
    let next = request(project.path(), storage.path(), "next", &[]);
    let mut first = prepare_checkout(&next, &mut |_| Ok(())).unwrap();
    let mut second = prepare_checkout(&next, &mut |_| Ok(())).unwrap();
    let winner = first.commit().unwrap().generation.unwrap();
    let error = second.commit().unwrap_err();
    assert!(error.to_string().starts_with("view publication conflict:"));
    assert!(error.to_string().contains(&winner));
    assert!(
        matches!(error, ViewError::PublicationConflict { current_generation: Some(current), .. } if current == winner)
    );
}

#[test]
fn candidate_bytes_are_released_before_derived_materialization() {
    let project = repository(3);
    let storage = tempfile::tempdir().unwrap();
    let request = request(project.path(), storage.path(), "head", &[]);
    let mut measured = false;
    let mut prepared = prepare_checkout(&request, &mut |phase| {
        if phase == "derived" {
            assert_eq!(RETAINED_CANDIDATE_BYTES.with(|count| count.get()), 0);
            measured = true;
        }
        Ok(())
    })
    .unwrap();
    assert!(measured);
    assert!(prepared.commit().unwrap().published);
}

/// Publish the initial generation and plant a marker table in its derived
/// database. A patched clone keeps the marker; a cold build starts from an
/// empty file and does not have it.
fn publish_marked_base(project: &Path, storage: &Path) -> (ViewStore, PathBuf) {
    let initial = publish_checkout(&request(project, storage, "head-1", &[])).unwrap();
    assert!(initial.published);
    let view = ViewStore::open(storage, SCOPE).unwrap();
    let owner = view
        .derived_path(initial.generation.as_deref().unwrap())
        .unwrap();
    wait_for_no_connections(&owner);
    let connection = Connection::open(&owner).unwrap();
    connection
        .execute_batch("CREATE TABLE cold_fallback_marker(value); PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(connection);
    (view, owner)
}

/// The deferred checkpoint keeps the published generation's keeper open for a
/// short idle delay. Tests that rewrite the file must wait until it closes.
fn wait_for_no_connections(path: &Path) {
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while crate::db::file_identity::open_connections(path) != 0 {
        assert!(
            Instant::now() < deadline,
            "keeper stayed open on {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn edit(project: &Path, index: usize) -> String {
    let name = format!("file_{index}.ts");
    let path = project.join(&name);
    let mut source = fs::read_to_string(&path).unwrap();
    source.insert_str(0, &format!("export function inserted_{index}() {{}}\n"));
    fs::write(path, source).unwrap();
    name
}

struct Published {
    derived: PathBuf,
    manifest: Manifest,
    counts: ColdBuildCounts,
}

fn publish_edit(project: &Path, storage: &Path, view: &ViewStore, edited: &[usize]) -> Published {
    let changed = edited
        .iter()
        .map(|index| edit(project, *index))
        .collect::<Vec<_>>();
    let report = publish_checkout(&request(project, storage, "head-2", &changed)).unwrap();
    assert!(
        report.published,
        "publication must not fail on an unusable base"
    );
    let generation = report.generation.unwrap();
    let derived = view.derived_path(&generation).unwrap();
    wait_for_no_connections(&derived);
    Published {
        derived,
        manifest: report.manifest.unwrap(),
        counts: cold_build_counts(view.view_dir()),
    }
}

fn has_marker(path: &Path) -> bool {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'cold_fallback_marker'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

/// The published database must equal a cold build of its own manifest.
fn assert_matches_cold(storage: &Path, published: &Published) {
    let callgraph = BlobStore::open(storage, FAMILY.to_string(), BlobPlane::Callgraph).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cold = directory.path().join("cold.sqlite");
    crate::callgraph_store::materialize_manifest_view_database(
        &cold,
        callgraph.path(),
        &published.manifest,
    )
    .unwrap();
    crate::views::materialization::parity::assert_logical_parity(&published.derived, &cold);
}

#[test]
fn corrupt_base_falls_back_to_a_cold_build() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    // Overwrite the header: SQLite reports SQLITE_NOTADB when the clone reads it.
    fs::write(&owner, vec![0x5a_u8; 8192]).unwrap();

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);

    assert_eq!(
        published.counts,
        ColdBuildCounts {
            base_unreadable: 1,
            ..ColdBuildCounts::default()
        }
    );
    assert!(!has_marker(&published.derived), "corrupt base was reused");
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn truncated_base_falls_back_to_a_cold_build() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    // Keep the header page only. The header still claims the full page
    // count, so the clone's backup reports SQLITE_CORRUPT on the short file.
    let file = fs::OpenOptions::new().write(true).open(&owner).unwrap();
    file.set_len(4096).unwrap();
    drop(file);

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);

    assert_eq!(
        published.counts,
        ColdBuildCounts {
            base_unreadable: 1,
            ..ColdBuildCounts::default()
        }
    );
    assert!(!has_marker(&published.derived), "truncated base was reused");
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn damaged_meta_page_in_the_base_falls_back_to_a_cold_build() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    // Overwrite only the root page of `meta`. The schema stays readable, so
    // the backup (which copies pages without parsing them) and the keeper
    // both succeed, and the patch is what reports SQLITE_CORRUPT when it
    // reads the base fingerprint.
    let meta_root: i64 = Connection::open(&owner)
        .unwrap()
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name = 'meta'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut bytes = fs::read(&owner).unwrap();
    let page_size = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
    let start = (meta_root as usize - 1) * page_size;
    assert!(meta_root > 1 && start + page_size <= bytes.len());
    bytes[start..start + page_size].fill(0xa5);
    fs::write(&owner, bytes).unwrap();

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);

    assert_eq!(
        published.counts,
        ColdBuildCounts {
            base_unreadable: 1,
            ..ColdBuildCounts::default()
        }
    );
    assert!(!has_marker(&published.derived), "damaged base was reused");
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn fingerprint_mismatch_falls_back_to_a_cold_build() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    let connection = Connection::open(&owner).unwrap();
    connection
        .execute(
            "UPDATE meta SET v = 'not-the-base-manifest' WHERE k = 'view_manifest_fingerprint'",
            [],
        )
        .unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(connection);

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);

    assert_eq!(
        published.counts,
        ColdBuildCounts {
            base_fingerprint_mismatch: 1,
            ..ColdBuildCounts::default()
        }
    );
    assert!(
        !has_marker(&published.derived),
        "mismatched base was reused"
    );
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn missing_shared_owner_manifest_falls_back_to_a_cold_build() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, _) = publish_marked_base(project.path(), storage.path());
    let owner = view.current_generation().unwrap().unwrap();
    view.reuse_derived("shared", &owner).unwrap();
    fs::copy(
        view.manifest_path(&owner).unwrap(),
        view.manifest_path("shared").unwrap(),
    )
    .unwrap();
    view.open_pointer_connection()
        .unwrap()
        .execute("UPDATE pointer SET generation = 'shared'", [])
        .unwrap();
    fs::remove_file(view.manifest_path(&owner).unwrap()).unwrap();

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);
    assert!(
        !has_marker(&published.derived),
        "a missing owner manifest must not seed an incremental clone"
    );
    assert_eq!(published.counts.base_not_ready, 1);
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn busy_readiness_defers_assembly_without_a_cold_rebuild() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    let initial = view.current_generation().unwrap();
    let writer =
        crate::db::file_identity::IdentityConnection::open(&owner, "busy_readiness_test").unwrap();
    // WAL writers normally allow reads. An exclusive rollback-journal writer
    // forces the readiness SELECT itself to encounter real SQLite contention.
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    let attempt = prepare_checkout(
        &request(project.path(), storage.path(), "head-1", &[]),
        &mut |_| Ok(()),
    );
    match attempt {
        Err(ViewError::Sqlite(error)) if crate::db::is_busy_error(&error) => {}
        Err(error) => panic!("busy readiness must stay a retryable SQLite error, not {error:?}"),
        Ok(_) => panic!("busy readiness must not permit a publication or cold rebuild"),
    }
    assert_eq!(view.current_generation().unwrap(), initial);
    assert_eq!(
        cold_build_counts(view.view_dir()),
        ColdBuildCounts::default()
    );
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    let repeat = publish_checkout(&request(project.path(), storage.path(), "head-1", &[])).unwrap();
    assert!(!repeat.published);
    assert_eq!(repeat.generation, initial);
    assert_eq!(
        cold_build_counts(view.view_dir()),
        ColdBuildCounts::default()
    );
}

#[test]
fn stale_build_output_republishes_an_unchanged_manifest() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    let connection = Connection::open(&owner).unwrap();
    connection
        .execute(
            "UPDATE meta SET v = 'previous-build-output' WHERE k = 'fingerprint'",
            [],
        )
        .unwrap();
    drop(connection);

    let report = publish_checkout(&request(project.path(), storage.path(), "head-1", &[])).unwrap();
    assert!(
        report.published,
        "same HEAD and manifest must not suppress a stale derived rebuild"
    );
    let derived = view
        .derived_path(report.generation.as_deref().unwrap())
        .unwrap();
    assert!(
        !has_marker(&derived),
        "stale derived rows must not be reused or patched"
    );
    crate::callgraph_store::ReadonlyCallGraphStore::open_manifest_view(
        project.path().to_path_buf(),
        FAMILY.into(),
        view.view_dir().to_path_buf(),
        report.generation.as_deref().unwrap(),
        None,
    )
    .expect("rebuilt derived generation must satisfy the real reader's readiness check");
}

#[test]
fn callgraph_previous_store_version_rebuilds_existing_view() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    let previous = blake3::hash(
        b"callgraph_store:v2:positional:raw-ref:v19-position-independent-node-identities",
    )
    .to_hex()
    .to_string();
    let connection = Connection::open(&owner).unwrap();
    connection
        .execute(
            "UPDATE meta SET v = ?1 WHERE k = 'fingerprint'",
            [&previous],
        )
        .unwrap();
    connection
        .execute("UPDATE meta SET v = '2' WHERE k = 'schema_version'", [])
        .unwrap();
    drop(connection);
    assert!(!crate::callgraph_store::manifest_view_database_ready(
        view.view_dir(),
        view.current_generation().unwrap().as_deref().unwrap(),
        Instant::now() + crate::db::STEADY_BUSY_TIMEOUT
    )
    .unwrap());

    let report = publish_checkout(&request(project.path(), storage.path(), "head-1", &[])).unwrap();
    assert!(
        report.published,
        "an unchanged manifest must rebuild incompatible graph data"
    );
    let derived = view
        .derived_path(report.generation.as_deref().unwrap())
        .unwrap();
    assert!(!has_marker(&derived), "old derived rows must not be reused");
    assert!(crate::callgraph_store::manifest_view_database_ready(
        view.view_dir(),
        report.generation.as_deref().unwrap(),
        Instant::now() + crate::db::STEADY_BUSY_TIMEOUT
    )
    .unwrap());
}

#[test]
fn callgraph_previous_producer_reextracts_callbacks_and_inline_test_status() {
    use crate::blob_store::{BlobPlane, BlobStore, CallgraphKey};
    use crate::callgraph_store::join::{BlobRefKind, CallgraphBlob};
    use crate::commands::callgraph_store_adapter::{callers_result, impact_result};

    let project = repository(1);
    let source = "fn transform() {}\n#[test]\nfn checks() { spawn(transform); }\n";
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    git(project.path(), &["add", "."]);
    git(
        project.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "rust fixture",
        ],
    );
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    let generation = view.current_generation().unwrap().unwrap();
    let old_producer = "ruled-callgraph-v10-go-alias-and-heading-ranges";
    let old_key = CallgraphKey::from_bytes(source.as_bytes(), "rust", old_producer).full_key();
    let current_key =
        CallgraphKey::from_bytes(source.as_bytes(), "rust", crate::views::callgraph::PRODUCER)
            .full_key();
    // An old producer name still runs today's extractor. Remove the evidence
    // the old extractor lacked before seeding its immutable blob.
    let mut old_payload: serde_json::Value = serde_json::from_slice(
        &CallgraphBlob::extract(source, "rust", old_producer)
            .unwrap()
            .to_bytes()
            .unwrap(),
    )
    .unwrap();
    old_payload["refs"]
        .as_array_mut()
        .unwrap()
        .retain(|reference| reference["kind"] != "callback_ref");
    for symbol in old_payload["symbols"].as_array_mut().unwrap() {
        symbol.as_object_mut().unwrap().remove("is_test");
    }
    let mut blobs = BlobStore::open(storage.path(), FAMILY, BlobPlane::Callgraph).unwrap();
    blobs
        .put(&old_key, &serde_json::to_vec(&old_payload).unwrap())
        .unwrap();
    let blob_path = blobs.path().to_path_buf();
    drop(blobs);
    // Reusing a compatible current blob is correct; force that cache lookup to miss.
    Connection::open(blob_path)
        .unwrap()
        .execute(
            "DELETE FROM blob_payloads WHERE full_key = ?1",
            [current_key.as_bytes().as_slice()],
        )
        .unwrap();

    let manifest = view.load_manifest(&generation).unwrap();
    let old_manifest = Manifest::new(manifest.entries().map(|(path, entry)| {
        let mut entry = entry.clone();
        if path.as_bytes() == b"src/lib.rs" {
            let ManifestEntry::Regular { planes, .. } = &mut entry else {
                panic!("regular Rust fixture")
            };
            planes.callgraph = Some(old_key.to_hex());
        }
        (path.clone(), entry)
    }))
    .unwrap();
    fs::write(
        view.manifest_path(&generation).unwrap(),
        old_manifest.to_json_bytes().unwrap(),
    )
    .unwrap();
    crate::views::assembly::release_manifest_cache(view.view_dir());
    Connection::open(owner)
        .unwrap()
        .execute(
            "UPDATE meta SET v = 'old-build-output' WHERE k = 'fingerprint'",
            [],
        )
        .unwrap();

    let report = publish_checkout(&request(project.path(), storage.path(), "head-1", &[])).unwrap();
    assert!(report.published);
    let manifest = view
        .load_manifest(report.generation.as_deref().unwrap())
        .unwrap();
    let ManifestEntry::Regular { planes, .. } = manifest
        .get(&RelPath::new(b"src/lib.rs".to_vec()).unwrap())
        .unwrap()
    else {
        panic!("regular Rust fixture")
    };
    assert_eq!(
        planes.callgraph.as_deref(),
        Some(current_key.to_hex().as_str())
    );
    let blobs = BlobStore::open(storage.path(), FAMILY, BlobPlane::Callgraph).unwrap();
    let current = CallgraphBlob::from_bytes(&blobs.get(&current_key).unwrap().unwrap()).unwrap();
    let parse = current.parse().unwrap();
    assert!(parse
        .refs
        .iter()
        .any(|reference| reference.kind == BlobRefKind::CallbackRef));
    assert_eq!(
        parse
            .symbols
            .iter()
            .find(|symbol| symbol.name == "checks")
            .unwrap()
            .is_test,
        Some(true)
    );
    let store = crate::callgraph_store::ReadonlyCallGraphStore::open_manifest_view(
        project.path().to_path_buf(),
        FAMILY.into(),
        view.view_dir().to_path_buf(),
        report.generation.as_deref().unwrap(),
        None,
    )
    .unwrap();
    for (op, result, expected) in [
        (
            "callers",
            serde_json::to_value(
                callers_result(&store, Path::new("src/lib.rs"), "transform", 5, true).unwrap(),
            )
            .unwrap(),
            "1 caller (1 by callback reference) · 1 file group\nsrc/lib.rs\n  ↳ checks:3 (callback reference)",
        ),
        (
            "impact",
            serde_json::to_value(
                impact_result(&store, Path::new("src/lib.rs"), "transform", 5, true).unwrap(),
            )
            .unwrap(),
            // Impact shows the caller's source line whenever the file still
            // matches the store, as the views-off render does
            // (callgraph_regression_tests). A v1 view used to fail that check
            // because it records each file's call-graph blob key, not its
            // content hash, which hid the row.
            "1 affected site (1 by callback reference) · 1 file\nsrc/lib.rs:3\n  ↳ checks (callback reference)\n  fn checks() { spawn(transform); }",
        ),
        (
            "callers",
            serde_json::to_value(
                callers_result(&store, Path::new("src/lib.rs"), "transform", 5, false).unwrap(),
            )
            .unwrap(),
            "0 callers · 0 file groups\n1 callers in tests hidden — includeTests: true shows them",
        ),
    ] {
        assert_eq!(
            crate::subc_format::format_callgraph(op, &result, false),
            expected
        );
    }
}

#[test]
fn stale_build_output_does_not_seed_an_incremental_diff() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = publish_marked_base(project.path(), storage.path());
    let connection = Connection::open(&owner).unwrap();
    connection
        .execute(
            "UPDATE meta SET v = 'previous-build-output' WHERE k = 'fingerprint'",
            [],
        )
        .unwrap();
    drop(connection);

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);
    assert!(
        !has_marker(&published.derived),
        "stale derived rows must not seed an incremental diff"
    );
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn small_diff_patches_a_clone_of_the_base() {
    let project = repository(8);
    let storage = tempfile::tempdir().unwrap();
    let (view, _) = publish_marked_base(project.path(), storage.path());

    let published = publish_edit(project.path(), storage.path(), &view, &[1]);

    assert_eq!(published.counts, ColdBuildCounts::default());
    assert!(
        has_marker(&published.derived),
        "a small diff must patch the clone"
    );
}

#[test]
fn diff_above_the_cutoff_builds_cold() {
    // Enough files that editing just over the cutoff share also clears the
    // minimum changed-entry floor.
    let files = (INCREMENTAL_CUTOFF_MIN_CHANGED * 100).div_ceil(INCREMENTAL_CUTOFF_PERCENT) + 8;
    let edited = files * INCREMENTAL_CUTOFF_PERCENT / 100 + 1;
    assert!(edited >= INCREMENTAL_CUTOFF_MIN_CHANGED && edited <= files);
    let project = repository(files);
    let storage = tempfile::tempdir().unwrap();
    let (view, _) = publish_marked_base(project.path(), storage.path());

    let published = publish_edit(
        project.path(),
        storage.path(),
        &view,
        &(0..edited).collect::<Vec<_>>(),
    );

    assert_eq!(
        published.counts,
        ColdBuildCounts {
            large_diff: 1,
            ..ColdBuildCounts::default()
        }
    );
    assert!(
        !has_marker(&published.derived),
        "an oversized diff must not patch"
    );
    assert_matches_cold(storage.path(), &published);
}

#[test]
fn configuration_change_lowers_the_cutoff() {
    // Between the two cutoff shares: this many edits patch when no resolver
    // input changed and build cold when package.json changed as well.
    let files = INCREMENTAL_CUTOFF_MIN_CHANGED * 4;
    let edited =
        files * (INCREMENTAL_CUTOFF_PERCENT + INCREMENTAL_CUTOFF_PERCENT_FULL_RESOLUTION) / 200;
    assert!(edited >= INCREMENTAL_CUTOFF_MIN_CHANGED);
    for configuration_changed in [false, true] {
        let project = repository(files);
        fs::write(
            project.path().join("package.json"),
            "{\"name\":\"fixture\"}\n",
        )
        .unwrap();
        git(project.path(), &["add", "package.json"]);
        git(
            project.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "package",
            ],
        );
        let storage = tempfile::tempdir().unwrap();
        let (view, _) = publish_marked_base(project.path(), storage.path());
        let mut changed = (0..edited)
            .map(|index| edit(project.path(), index))
            .collect::<Vec<_>>();
        if configuration_changed {
            fs::write(
                project.path().join("package.json"),
                "{\"name\":\"fixture-renamed\"}\n",
            )
            .unwrap();
            changed.push("package.json".to_string());
        }
        let report =
            publish_checkout(&request(project.path(), storage.path(), "head-2", &changed)).unwrap();
        assert!(report.published);
        let derived = view
            .derived_path(report.generation.as_deref().unwrap())
            .unwrap();
        wait_for_no_connections(&derived);
        let counts = cold_build_counts(view.view_dir());
        if configuration_changed {
            assert_eq!(
                counts,
                ColdBuildCounts {
                    large_diff: 1,
                    ..ColdBuildCounts::default()
                }
            );
            assert!(!has_marker(&derived));
        } else {
            assert_eq!(counts, ColdBuildCounts::default());
            assert!(
                has_marker(&derived),
                "no resolver input changed; the diff must patch"
            );
        }
    }
}

#[test]
fn incremental_cutoff_boundaries() {
    use super::super::materialization::ManifestDiffSize;
    let entries = 10_000;
    for (may_force_full_resolution, percent) in [
        (false, INCREMENTAL_CUTOFF_PERCENT),
        (true, INCREMENTAL_CUTOFF_PERCENT_FULL_RESOLUTION),
    ] {
        let at = |changed, entries| {
            diff_exceeds_incremental_cutoff(ManifestDiffSize {
                changed,
                entries,
                may_force_full_resolution,
            })
        };
        let threshold = entries * percent / 100;
        assert!(
            !at(threshold, entries),
            "exactly at the share still patches (full={may_force_full_resolution})"
        );
        assert!(
            at(threshold + 1, entries),
            "above the share builds cold (full={may_force_full_resolution})"
        );
        assert!(!at(0, entries));
        let small = INCREMENTAL_CUTOFF_MIN_CHANGED - 1;
        assert!(
            !at(small, small),
            "below the floor always patches, even at 100% (full={may_force_full_resolution})"
        );
    }
    const _: () = assert!(INCREMENTAL_CUTOFF_PERCENT_FULL_RESOLUTION < INCREMENTAL_CUTOFF_PERCENT);
}

#[test]
fn s4_upgrade_old_own_generation_is_rejected_before_clone() {
    let project = repository(4);
    let storage = tempfile::tempdir().unwrap();
    let (view, owner) = crate::callgraph_store::position_identity_tests::with_pre_s4_output(|| {
        publish_marked_base(project.path(), storage.path())
    });
    {
        let conn = Connection::open(&owner).unwrap();
        crate::callgraph_store::position_identity_tests::seed_old_dispatch_hint(&conn);
        crate::callgraph_store::position_identity_tests::stamp_pre_s4(&conn, true);
    }
    assert!(!crate::callgraph_store::manifest_view_database_ready(
        view.view_dir(),
        view.current_generation().unwrap().as_deref().unwrap(),
        Instant::now() + crate::db::STEADY_BUSY_TIMEOUT
    )
    .unwrap());
    let changed = edit(project.path(), 1);
    let mut prepared = prepare_checkout(
        &request(project.path(), storage.path(), "head-2", &[changed]),
        &mut |_| Ok(()),
    )
    .unwrap();
    assert!(!prepared.profile.derived_cloned);
    assert_eq!(prepared.profile.cold_build_reason, "base_not_ready");
    let report = prepared.commit().unwrap();
    let derived = view
        .derived_path(report.generation.as_deref().unwrap())
        .unwrap();
    wait_for_no_connections(&derived);
    assert!(!has_marker(&derived));
    crate::callgraph_store::position_identity_tests::assert_current_graph(
        &Connection::open(&derived).unwrap(),
    );
    let reader = crate::callgraph_store::ReadonlyCallGraphStore::open_manifest_view(
        project.path().to_path_buf(),
        FAMILY.into(),
        view.view_dir().to_path_buf(),
        report.generation.as_deref().unwrap(),
        None,
    )
    .unwrap();
    assert!(
        !crate::callgraph_store::CallGraphRead::edge_snapshot(&reader)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn s4_upgrade_old_sibling_is_rejected_and_checkout_builds_cold() {
    let project = repository(4);
    let sibling = tempfile::tempdir().unwrap();
    let sibling_root = sibling.path().join("linked");
    git(
        project.path(),
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            sibling_root.to_str().unwrap(),
            "HEAD",
        ],
    );
    let storage = tempfile::tempdir().unwrap();
    let mut donor = request(project.path(), storage.path(), "head-1", &[]);
    donor.scope = crate::path_identity::project_scope_key(project.path());
    donor.family = crate::search_index::artifact_cache_key(project.path());
    let report = crate::callgraph_store::position_identity_tests::with_pre_s4_output(|| {
        publish_checkout(&donor)
    })
    .unwrap();
    let view = ViewStore::open(storage.path(), &donor.scope).unwrap();
    let derived = view
        .derived_path(report.generation.as_deref().unwrap())
        .unwrap();
    wait_for_no_connections(&derived);
    {
        let conn = Connection::open(&derived).unwrap();
        crate::callgraph_store::position_identity_tests::seed_old_dispatch_hint(&conn);
        crate::callgraph_store::position_identity_tests::stamp_pre_s4(&conn, true);
    }
    let mut target = donor.clone();
    target.project_root = sibling_root.clone();
    target.scope = crate::path_identity::project_scope_key(&sibling_root);
    assert!(worker_seed::sibling_checkout_seed(
        &target,
        Instant::now() + crate::db::STEADY_BUSY_TIMEOUT
    )
    .unwrap()
    .is_none());
    let mut prepared = prepare_checkout(&target, &mut |_| Ok(())).unwrap();
    assert!(!prepared.profile.derived_cloned);
    assert_eq!(prepared.profile.seed_source, "cold");
    let report = prepared.commit().unwrap();
    let target_view = ViewStore::open(storage.path(), &target.scope).unwrap();
    let database = target_view
        .derived_path(report.generation.as_deref().unwrap())
        .unwrap();
    wait_for_no_connections(&database);
    crate::callgraph_store::position_identity_tests::assert_current_graph(
        &Connection::open(&database).unwrap(),
    );
    let reader = crate::callgraph_store::ReadonlyCallGraphStore::open_manifest_view(
        sibling_root,
        target.family,
        target_view.view_dir().to_path_buf(),
        report.generation.as_deref().unwrap(),
        None,
    )
    .unwrap();
    assert!(
        !crate::callgraph_store::CallGraphRead::edge_snapshot(&reader)
            .unwrap()
            .is_empty()
    );
}
