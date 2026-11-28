use super::*;

/// A protected folder holding one secret, registered as a protected tree, and
/// a separate project folder. Each test uses its own folders, so the
/// process-wide registry never makes one test's files protected for another.
struct Fixture {
    _temp: tempfile::TempDir,
    protected: PathBuf,
    project: PathBuf,
    secret: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let protected = base.join("protected");
        let project = base.join("project");
        std::fs::create_dir(&protected).unwrap();
        std::fs::create_dir(&project).unwrap();
        let secret = protected.join("id_secret");
        std::fs::write(&secret, "secret").unwrap();
        register([protected.clone()], []);
        Self {
            _temp: temp,
            protected,
            project,
            secret,
        }
    }
}

fn refused_as(result: io::Result<File>) -> Option<Option<PathBuf>> {
    let error = result.err()?;
    protected_alias(&error).map(|alias| alias.protected().map(crate::path_identity::normalize_path))
}

fn normalized(path: &Path) -> Option<Option<PathBuf>> {
    Some(Some(crate::path_identity::normalize_path(path)))
}

#[test]
fn opened_hard_link_to_protected_file_is_refused_before_reading() {
    let f = Fixture::new();
    let alias = f.project.join("alias.txt");
    std::fs::hard_link(&f.secret, &alias).unwrap();
    let error = crate::bounded_io::open_content_file(&alias).unwrap_err();
    let refusal = protected_alias(&error).expect("protected alias refusal");
    assert_eq!(
        refusal
            .protected()
            .map(crate::path_identity::normalize_path),
        Some(crate::path_identity::normalize_path(&f.secret))
    );
    assert_eq!(
        error.to_string(),
        format!(
            "path '{}' is denied by the access policy floor",
            alias.display()
        )
    );
    // Every content reader shares the opener.
    assert!(crate::bounded_io::read(&alias, None).is_err());
    assert!(crate::task_temp_reads::open(&alias).is_err());
}

#[test]
fn ordinary_and_directly_named_files_still_open() {
    let f = Fixture::new();
    // A single-link file and a hard link between two allowed names.
    let plain = f.project.join("plain.txt");
    std::fs::write(&plain, "plain").unwrap();
    let linked = f.project.join("linked.txt");
    std::fs::hard_link(&plain, &linked).unwrap();
    assert!(crate::bounded_io::open_content_file(&plain).is_ok());
    assert!(crate::bounded_io::open_content_file(&linked).is_ok());
    // A protected file opened by its own name was judged by that name.
    std::fs::hard_link(&f.secret, f.project.join("alias.txt")).unwrap();
    assert!(crate::bounded_io::open_content_file(&f.secret).is_ok());
}

#[test]
fn links_and_protected_files_made_after_the_index_are_refused() {
    let f = Fixture::new();
    // Build the index first, through an unrelated multi-link open.
    let plain = f.project.join("plain.txt");
    std::fs::write(&plain, "plain").unwrap();
    std::fs::hard_link(&plain, f.project.join("plain-link.txt")).unwrap();
    assert!(crate::bounded_io::open_content_file(&plain).is_ok());

    // A new link to a protected file that had one link when indexed.
    let alias = f.project.join("alias.txt");
    std::fs::hard_link(&f.secret, &alias).unwrap();
    assert_eq!(
        refused_as(crate::bounded_io::open_content_file(&alias)),
        normalized(&f.secret)
    );

    // A protected file created after the index, then linked.
    let later = f.protected.join("later_secret");
    std::fs::write(&later, "later").unwrap();
    let later_alias = f.project.join("later-alias.txt");
    std::fs::hard_link(&later, &later_alias).unwrap();
    assert_eq!(
        refused_as(crate::bounded_io::open_content_file(&later_alias)),
        normalized(&later)
    );
}

#[cfg(unix)]
#[test]
fn a_walk_cut_short_refuses_an_old_alias_to_a_missed_protected_file() {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime};
    let f = Fixture::new();
    let alias = f.project.join("alias.txt");
    std::fs::hard_link(&f.secret, &alias).unwrap();
    let metadata = std::fs::metadata(&alias).unwrap();
    let identity = (metadata.dev(), metadata.ino());
    let roots = Roots {
        trees: [f.protected.clone()].into(),
        data_roots: BTreeSet::new(),
    };
    // The budget admits the root but not the protected file inside it.
    let incomplete = unix::Index::build(0, &roots, 1, Duration::from_secs(5));
    let started = incomplete.walk_started();
    let mut slot = Some(incomplete);
    unix::refresh_index(&mut slot, 0, SystemTime::UNIX_EPOCH, started, || {
        panic!("a fresh incomplete index should not be rebuilt")
    });
    let index = slot.unwrap();
    assert_eq!(index.summary(), (false, 1));
    assert_eq!(index.verdict(identity), Some(None));
}

#[cfg(unix)]
#[test]
fn a_walk_cut_short_refuses_an_unrelated_old_multi_link_file_until_retry() {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime};
    let f = Fixture::new();
    let plain = f.project.join("plain.txt");
    std::fs::write(&plain, "plain").unwrap();
    std::fs::hard_link(&plain, f.project.join("plain-link.txt")).unwrap();
    let metadata = std::fs::metadata(&plain).unwrap();
    let identity = (metadata.dev(), metadata.ino());
    let roots = Roots {
        trees: [f.protected.clone()].into(),
        data_roots: BTreeSet::new(),
    };
    let incomplete = unix::Index::build(0, &roots, 1, Duration::from_secs(5));
    let started = incomplete.walk_started();
    let mut slot = Some(incomplete);
    let old = SystemTime::UNIX_EPOCH;
    unix::refresh_index(&mut slot, 0, old, started, || {
        panic!("a fresh incomplete index should not be rebuilt")
    });
    let index = slot.as_ref().unwrap();
    assert_eq!(index.summary(), (false, 1));
    assert_eq!(index.verdict(identity), Some(None));

    unix::refresh_index(&mut slot, 0, SystemTime::now(), started, || {
        panic!("recent file changes must not bypass the retry interval")
    });
    let after_retry_interval = started + Duration::from_secs(2);
    unix::refresh_index(&mut slot, 0, old, after_retry_interval, || {
        unix::Index::build(0, &roots, 2, Duration::from_secs(5))
    });
    unix::refresh_index(&mut slot, 0, old, after_retry_interval, || {
        panic!("an old file should reuse the complete index")
    });
    let complete = slot.unwrap();
    assert_eq!(complete.summary(), (true, 2));
    assert_eq!(complete.verdict(identity), None);
}

#[cfg(unix)]
#[test]
fn a_root_change_rebuilds_an_incomplete_index_without_waiting() {
    use std::time::{Duration, SystemTime};
    let f = Fixture::new();
    let mut roots = Roots {
        trees: [f.protected.clone()].into(),
        data_roots: BTreeSet::new(),
    };
    let incomplete = unix::Index::build(0, &roots, 1, Duration::from_secs(5));
    let started = incomplete.walk_started();
    let mut slot = Some(incomplete);
    assert_eq!(slot.as_ref().unwrap().summary(), (false, 1));
    roots.trees.insert(f.project);
    unix::refresh_index(&mut slot, 1, SystemTime::UNIX_EPOCH, started, || {
        unix::Index::build(1, &roots, 3, Duration::from_secs(5))
    });
    assert_eq!(slot.unwrap().summary(), (true, 3));
}

#[cfg(unix)]
#[test]
fn cortexkit_data_roots_cover_closed_folders_and_credential_names_only() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("data");
    let project = root.parent().unwrap().join("project");
    for folder in ["run", "signing", "module/deep", "alfonso"] {
        std::fs::create_dir_all(root.join(folder)).unwrap();
    }
    std::fs::create_dir(&project).unwrap();
    let files = [
        ("run/connection.json", true),
        ("signing/key", true),
        ("module/exchange-token", true),
        ("module/notes.txt", false),
        // Deeper module data is not walked.
        ("module/deep/api-token", false),
    ];
    for (index, (name, _)) in files.iter().enumerate() {
        let path = root.join(name);
        std::fs::write(&path, "x").unwrap();
        std::fs::hard_link(&path, project.join(format!("alias-{index}"))).unwrap();
    }
    let roots = Roots {
        trees: BTreeSet::new(),
        data_roots: [root.clone()].into(),
    };
    let index = unix::Index::build(0, &roots, 1_000, std::time::Duration::from_secs(5));
    assert!(index.summary().0);
    for (name, protected) in files {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(root.join(name)).unwrap();
        let verdict = index.verdict((metadata.dev(), metadata.ino()));
        assert_eq!(verdict.is_some(), protected, "{name}");
    }
}
