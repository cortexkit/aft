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
