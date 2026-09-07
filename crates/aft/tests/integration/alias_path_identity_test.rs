use std::collections::BTreeSet;
use std::process::Command;

use aft::alias::{
    git_blob_oid, head_tree_entries, AliasSkip, AliasStore, AliasWrite, GitMode, Manifest,
    ManifestEntry, ManifestRecord, ManifestSqliteStore, PlaneKeys, TrackedPath,
    PATH_IDENTITY_VERSION,
};
use rusqlite::Connection;

fn regular_path(path: &[u8], bytes: &[u8]) -> TrackedPath {
    TrackedPath::new(
        path.to_vec(),
        GitMode::Regular { executable: false },
        git_blob_oid(bytes),
    )
    .expect("valid tracked path")
}

#[test]
fn aliases_require_a_regular_unfiltered_file_and_a_proven_git_blob_oid() {
    let storage = tempfile::tempdir().expect("temporary storage");
    let mut aliases = AliasStore::open(storage.path(), "family-a").expect("open alias store");
    let bytes = b"fn main() {}\n";
    assert_eq!(
        git_blob_oid(b"hello\n").to_hex(),
        "ce013625030ba8dba906f756967f9e9ca394464a",
        "Git blob IDs include the `blob <len>\\0` header"
    );

    let regular = regular_path(b"src/main.rs", bytes);
    assert_eq!(
        aliases
            .seed_proven_alias(&regular, bytes)
            .expect("seed regular alias"),
        AliasWrite::Inserted(*blake3::hash(bytes).as_bytes())
    );
    assert_eq!(
        aliases.resolve(regular.git_oid).expect("resolve alias"),
        Some(*blake3::hash(bytes).as_bytes())
    );
    assert_eq!(
        aliases
            .seed_proven_alias(&regular, bytes)
            .expect("reuse regular alias"),
        AliasWrite::Reused(*blake3::hash(bytes).as_bytes())
    );
    let executable = TrackedPath::new(
        b"bin/aft".to_vec(),
        GitMode::Regular { executable: true },
        git_blob_oid(bytes),
    )
    .expect("valid executable path");
    assert_eq!(
        aliases
            .seed_proven_alias(&executable, bytes)
            .expect("reuse executable alias"),
        AliasWrite::Reused(*blake3::hash(bytes).as_bytes())
    );

    let wrong_oid = regular_path(b"src/wrong.rs", b"different bytes");
    assert_eq!(
        aliases
            .seed_proven_alias(&wrong_oid, bytes)
            .expect("reject mismatched Git blob hash"),
        AliasWrite::Skipped(AliasSkip::GitOidMismatch)
    );

    let symlink = TrackedPath::new(
        b"src/link".to_vec(),
        GitMode::Symlink,
        git_blob_oid(b"main.rs"),
    )
    .expect("valid symlink path");
    let gitlink = TrackedPath::new(
        b"vendor/module".to_vec(),
        GitMode::Gitlink,
        git_blob_oid(b"unrelated commit bytes"),
    )
    .expect("valid gitlink path");
    let filtered = regular_path(b"large.bin", bytes).with_filter_status(true);
    let lfs_pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:1234\nsize 4\n";
    let lfs = regular_path(b"lfs.dat", lfs_pointer);

    for (path, contents, expected) in [
        (&symlink, b"main.rs".as_slice(), AliasSkip::Symlink),
        (
            &gitlink,
            b"unrelated commit bytes".as_slice(),
            AliasSkip::Gitlink,
        ),
        (&filtered, bytes.as_slice(), AliasSkip::Filtered),
        (&lfs, lfs_pointer.as_slice(), AliasSkip::LfsPointer),
    ] {
        assert_eq!(
            aliases
                .seed_proven_alias(path, contents)
                .expect("reject ineligible alias"),
            AliasWrite::Skipped(expected)
        );
    }
    assert_eq!(aliases.alias_count().expect("count aliases"), 1);
}

#[test]
fn zero_read_report_has_a_non_vacuous_95_percent_eligible_ratio_and_exclusions() {
    let storage = tempfile::tempdir().expect("temporary storage");
    let mut aliases = AliasStore::open(storage.path(), "family-a").expect("open alias store");
    let mut paths = Vec::new();

    for index in 0..20 {
        let bytes = format!("source-{index}").into_bytes();
        let path = regular_path(format!("src/{index}.rs").as_bytes(), &bytes)
            .with_previous_generation(true);
        if index < 19 {
            assert!(matches!(
                aliases
                    .seed_proven_alias(&path, &bytes)
                    .expect("seed eligible alias"),
                AliasWrite::Inserted(_)
            ));
        }
        paths.push(path);
    }

    paths.push(
        regular_path(b"filtered.rs", b"filtered")
            .with_previous_generation(true)
            .with_filter_status(true),
    );
    paths.push(
        TrackedPath::new(b"link".to_vec(), GitMode::Symlink, git_blob_oid(b"target"))
            .expect("valid symlink"),
    );
    paths.push(
        TrackedPath::new(
            b"submodule".to_vec(),
            GitMode::Gitlink,
            git_blob_oid(b"gitlink"),
        )
        .expect("valid gitlink"),
    );
    paths.push(regular_path(b"new.rs", b"new"));

    let report = aliases
        .zero_read_checkout_report(&paths)
        .expect("measure zero-read checkout");
    assert_eq!(report.numerator, 19);
    assert_eq!(report.denominator, 20);
    assert!(report.meets_95_percent());
    assert_eq!(report.excluded.filtered, 1);
    assert_eq!(report.excluded.symlink, 1);
    assert_eq!(report.excluded.gitlink, 1);
    assert_eq!(report.excluded.not_previously_indexed, 1);
    let rendered = report.to_string();
    assert!(rendered.contains("numerator=19 denominator=20"));
    assert!(rendered.contains("filtered=1"));
    assert!(rendered.contains("symlink=1"));
    assert!(rendered.contains("gitlink=1"));
}

#[test]
fn head_tree_reads_only_git_metadata_and_marks_filter_attributes() {
    let _git_environment = crate::test_helpers::hermetic_git_env_guard();
    let repo = tempfile::tempdir().expect("temporary repository");
    run_git(repo.path(), &["init"]);
    std::fs::write(repo.path().join("regular.txt"), "regular\n").expect("write regular file");
    std::fs::write(
        repo.path().join("lfs-pointer.txt"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n",
    )
    .expect("write LFS pointer");
    std::fs::write(
        repo.path().join(".gitattributes"),
        "lfs-pointer.txt filter=lfs\n",
    )
    .expect("write attributes");
    run_git(repo.path(), &["add", "."]);
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );

    let entries = head_tree_entries(repo.path()).expect("read Git metadata");
    let regular = entries
        .iter()
        .find(|entry| entry.rel_path == b"regular.txt")
        .expect("regular path from tree");
    let lfs = entries
        .iter()
        .find(|entry| entry.rel_path == b"lfs-pointer.txt")
        .expect("LFS path from tree");
    assert!(regular.mode.is_regular());
    assert!(!regular.filtered);
    assert!(lfs.filtered);

    let previous = BTreeSet::from([b"regular.txt".to_vec(), b"lfs-pointer.txt".to_vec()]);
    let storage = tempfile::tempdir().expect("temporary alias storage");
    let mut aliases = AliasStore::open(storage.path(), "family-a").expect("open aliases");
    aliases
        .seed_proven_alias(regular, b"regular\n")
        .expect("seed regular tree entry");
    std::fs::remove_file(repo.path().join("regular.txt"))
        .expect("remove working-tree regular file");
    std::fs::remove_file(repo.path().join("lfs-pointer.txt"))
        .expect("remove working-tree LFS pointer");
    let report = aliases
        .report_head_checkout(repo.path(), &previous)
        .expect("resolve aliases from HEAD metadata");
    assert_eq!(report.numerator, 1);
    assert_eq!(report.denominator, 1);
    assert_eq!(report.excluded.filtered, 1);
    assert_eq!(report.excluded.not_previously_indexed, 1);
}

#[test]
fn manifests_use_raw_blob_paths_byte_order_and_b64_for_non_utf8() {
    let gitlink_oid = git_blob_oid(b"gitlink object");
    let manifest = Manifest::new(vec![
        ManifestRecord {
            rel_path: vec![0xff, b'.', b'r', b's'],
            entry: ManifestEntry::regular(
                GitMode::Regular { executable: false },
                PlaneKeys::default(),
                false,
            ),
        },
        ManifestRecord {
            rel_path: b"z.rs".to_vec(),
            entry: ManifestEntry::Symlink {
                target_bytes: vec![b't', b'a', b'r', b'g', b'e', b't', 0xff],
            },
        },
        ManifestRecord {
            rel_path: b"a/module".to_vec(),
            entry: ManifestEntry::Gitlink { oid: gitlink_oid },
        },
    ])
    .expect("construct manifest");

    assert_eq!(manifest.path_identity_version, PATH_IDENTITY_VERSION);
    assert_eq!(
        manifest
            .entries
            .iter()
            .map(|entry| entry.rel_path.as_slice())
            .collect::<Vec<_>>(),
        vec![
            b"a/module".as_slice(),
            b"z.rs".as_slice(),
            &[0xff, b'.', b'r', b's'][..],
        ]
    );
    let json = manifest.to_json_value();
    assert_eq!(json["path_identity_version"], PATH_IDENTITY_VERSION);
    assert_eq!(json["entries"][0]["kind"], "gitlink");
    assert_eq!(json["entries"][1]["kind"], "symlink");
    assert_eq!(json["entries"][1]["target_bytes"]["b64"], "dGFyZ2V0/w==");
    assert_eq!(json["entries"][2]["rel_path"]["b64"], "/y5ycw==");

    let directory = tempfile::tempdir().expect("temporary manifest storage");
    let database = directory.path().join("manifest.sqlite");
    let mut store = ManifestSqliteStore::open(&database).expect("open manifest store");
    store.write(&manifest).expect("write manifest");
    assert_eq!(
        store
            .path_identity_version()
            .expect("read identity version"),
        Some(PATH_IDENTITY_VERSION)
    );
    assert_eq!(
        store.paths().expect("read bytewise paths"),
        vec![
            b"a/module".to_vec(),
            b"z.rs".to_vec(),
            vec![0xff, b'.', b'r', b's']
        ]
    );

    let connection = Connection::open(database).expect("open manifest database");
    let storage_type: String = connection
        .query_row(
            "SELECT typeof(rel_path) FROM manifest_entries WHERE rel_path = ?1",
            [vec![0xff, b'.', b'r', b's']],
            |row| row.get(0),
        )
        .expect("query path storage type");
    assert_eq!(storage_type, "blob");

    assert!(Manifest::new(vec![ManifestRecord {
        rel_path: b"C:/absolute.rs".to_vec(),
        entry: ManifestEntry::regular(
            GitMode::Regular { executable: false },
            PlaneKeys::default(),
            false,
        ),
    }])
    .is_err());
    assert!(Manifest::new(vec![ManifestRecord {
        rel_path: b"src\\windows-separator.rs".to_vec(),
        entry: ManifestEntry::regular(
            GitMode::Regular { executable: false },
            PlaneKeys::default(),
            false,
        ),
    }])
    .is_err());
}

fn run_git(root: &std::path::Path, args: &[&str]) {
    let mut command = Command::new("git");
    let output = crate::test_helpers::apply_hermetic_git_env(command.current_dir(root))
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
