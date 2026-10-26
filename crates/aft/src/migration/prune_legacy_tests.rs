use super::*;

fn identity(pid: u32, executable: Option<&str>, args: &[&str]) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        executable: executable.map(str::to_owned),
        argv0: args.first().map(|arg| (*arg).to_owned()),
        args: Some(args.iter().map(|arg| (*arg).to_owned()).collect()),
    }
}

#[test]
fn the_census_classifies_aft_processes_and_refuses_lookalikes() {
    let spaced = "/Users/me/Library/Application Support/AFT Host/bin/aft";
    assert_eq!(
        classify(&identity(
            101,
            Some(spaced),
            &[spaced, "--subc", "/tmp/c.json"]
        )),
        Some(CensusFinding::Aft {
            pid: 101,
            daemon: true,
            command: format!("{spaced} --subc /tmp/c.json"),
        }),
        "an executable path with spaces is one field"
    );
    assert!(matches!(
        classify(&identity(102, None, &["ck-aft"])),
        Some(CensusFinding::Aft {
            pid: 102,
            daemon: false,
            ..
        })
    ));
    assert!(matches!(
        classify(&identity(104, Some("/opt/bin/aft-bridge"), &["aft-bridge"])),
        Some(CensusFinding::Unclassifiable { pid: 104, .. })
    ));
    assert!(matches!(
        classify(&ProcessIdentity { pid: 105, ..ProcessIdentity::default() }),
        Some(CensusFinding::Unclassifiable { pid: 105, ref reason, .. })
            if reason.contains("could not be read")
    ));
    assert_eq!(
        classify(&identity(106, Some("/usr/bin/craft"), &["craft"])),
        None
    );
    assert_eq!(
        classify(&identity(
            107,
            Some("/bin/bash"),
            &["/bin/bash", "/home/u/bin/aft"]
        )),
        None,
        "a shell running an aft script is not itself AFT; its aft child is"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_system_census_reads_every_process_and_never_lists_itself() {
    let findings = SystemCensus.take().unwrap();
    let me = std::process::id();
    assert!(findings.iter().all(|finding| match finding {
        CensusFinding::Aft { pid, .. } | CensusFinding::Unclassifiable { pid, .. } => *pid != me,
    }));
    let unreadable = findings
        .iter()
        .filter(|finding| matches!(finding, CensusFinding::Unclassifiable { reason, .. } if reason.contains("could not be read")))
        .collect::<Vec<_>>();
    assert!(unreadable.is_empty(), "{unreadable:?}");
}

#[test]
fn dates_render_in_utc() {
    assert_eq!(format_ms(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_ms(1_798_761_600_000), "2027-01-01T00:00:00Z");
}

#[test]
fn reserved_names_are_not_legacy_sets() {
    let storage = tempfile::tempdir().unwrap();
    fs::create_dir_all(storage.path().join("semantic").join("models")).unwrap();
    fs::create_dir_all(storage.path().join("semantic").join("abc")).unwrap();
    fs::write(
        storage
            .path()
            .join("semantic")
            .join("abc")
            .join("semantic.bin"),
        b"x",
    )
    .unwrap();
    let inventory = inventory(storage.path(), SystemTime::now()).unwrap();
    assert_eq!(
        inventory
            .sets
            .iter()
            .map(|set| set.key.as_str())
            .collect::<Vec<_>>(),
        ["abc"]
    );
    assert_eq!(inventory.sets[0].eligibility, Eligibility::NotImported);
}

#[test]
fn a_tree_walk_stops_at_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..10 {
        fs::write(dir.path().join(format!("f{index}")), b"12345").unwrap();
    }
    let (_, _, complete) = tree_size(dir.path(), 4).unwrap();
    assert!(!complete);
    let (_, entries, complete) = tree_size(dir.path(), 100).unwrap();
    assert!(complete);
    assert_eq!(entries, 10);
}

// ---------------------------------------------------------------------------
// v3 import sources

use std::collections::BTreeMap;

use crate::blob_store::v3;
use crate::migration::v3_import::tests::{checkout, legacy_trigram, snapshot_tree, KEY};
use crate::migration::v3_ledger::{hold_claim_for_test, run_v3_import, V3ImportRequest};

struct Census(Vec<CensusFinding>);

impl ProcessCensus for Census {
    fn take(&self) -> Result<Vec<CensusFinding>, String> {
        Ok(self.0.clone())
    }
}

const FIFTEEN_DAYS: Duration = Duration::from_secs(15 * 24 * 60 * 60);

/// A storage root whose legacy set `KEY` was imported into v3 (and never
/// through v2), plus retained v1 and v2 data and a v3 family.
fn v3_imported_root(dir: &Path) -> PathBuf {
    let root = checkout(dir);
    let storage = dir.join("storage");
    legacy_trigram(&storage, &root);
    let symbols = crate::symbol_cache_disk::cache_path(&storage, KEY);
    fs::create_dir_all(symbols.parent().unwrap()).unwrap();
    fs::write(&symbols, b"not a symbol cache").unwrap();
    let mut request = V3ImportRequest::full_plane(
        storage.clone(),
        KEY.to_owned(),
        KEY.to_owned(),
        root,
        crate::migration::v3_import::tests::POLICY,
        None,
    );
    request.symbols = true;
    run_v3_import(&request, None).unwrap();
    for retained in [
        "views/0123456789abcdef/manifest.json",
        "blobs/0123456789abcdef/semantic.sqlite",
        "blobs/v2/family/semantic.sqlite",
    ] {
        let path = storage.join(retained);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"retained").unwrap();
    }
    storage
}

/// Every file's bytes except SQLite shared-memory files, which a reader
/// updates by design (read marks) without changing any stored data.
fn data_files(storage: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    snapshot_tree(storage)
        .into_iter()
        .filter(|(path, _)| !path.to_string_lossy().ends_with("-shm"))
        .collect()
}

/// Fails naming every file whose presence or bytes differ.
fn assert_same(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
    what: &str,
) {
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| {
            format!(
                "{} ({:?} -> {:?} bytes)",
                path.display(),
                before.get(path).map(Vec::len),
                after.get(path).map(Vec::len)
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(changed.is_empty(), "{what}: {changed:#?}");
}

fn eligibility_of(storage: &Path, now: SystemTime) -> Eligibility {
    inventory(storage, now)
        .unwrap()
        .sets
        .into_iter()
        .find(|set| set.key == KEY)
        .unwrap()
        .eligibility
}

fn run(storage: &Path, yes: bool, census: &Census) -> (PruneExit, String) {
    let mut out = Vec::new();
    let exit = prune_legacy(
        storage,
        &PruneOptions {
            yes,
            now: SystemTime::now() + FIFTEEN_DAYS,
        },
        census,
        None,
        &mut out,
    )
    .unwrap();
    (exit, String::from_utf8(out).unwrap())
}

/// A v3 import's completion makes the set (including its symbol cache)
/// eligible after the retention window, but never while a running import
/// holds a claim on it, whatever its other rows say.
#[test]
fn a_set_a_live_v3_import_reads_is_never_prune_eligible() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let storage = v3_imported_root(dir.path());
    let now = SystemTime::now();
    assert!(matches!(
        eligibility_of(&storage, now),
        Eligibility::Retained { .. }
    ));
    assert!(matches!(
        eligibility_of(&storage, now + FIFTEEN_DAYS),
        Eligibility::Eligible { .. }
    ));
    let set = inventory(&storage, now).unwrap().sets.remove(0);
    assert!(set.dirs.iter().any(|dir| dir.kind == LegacyKind::Symbols));

    let held = hold_claim_for_test(&storage, KEY, KEY);
    let pinned = eligibility_of(&storage, now + FIFTEEN_DAYS);
    assert!(
        matches!(pinned, Eligibility::PinnedSource(ref reason) if reason.contains("v3 import")),
        "{pinned:?}"
    );
    let before = data_files(&storage);
    let (exit, out) = run(&storage, true, &Census(Vec::new()));
    assert_eq!(exit, PruneExit::Clean, "{out}");
    assert!(
        out.contains("kept: still pinned as an import source"),
        "{out}"
    );
    assert_same(&before, &data_files(&storage), "a pinned set lost bytes");
    drop(held);
}

/// The slice-4 base protocol is consulted: a live base build in the family
/// of the same name pins the legacy set; a dead builder does not.
#[test]
fn a_live_v3_base_build_pins_the_family_legacy_set() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let storage = v3_imported_root(dir.path());
    let lock = v3::ReservationLock::acquire(&storage).unwrap();
    let mut registry = v3::open_base_refs_registry(&storage, KEY, &lock, None).unwrap();
    let me = crate::views::registry::current_owner();
    registry
        .with_barrier(|tx| {
            tx.execute(
                "INSERT INTO base_builds (base_scope, owner_pid, owner_start, owner_token, attempt, state, heartbeat_at_ms)
                 VALUES ('base-under-test', ?1, ?2, 'token', 1, 'building', 0)",
                rusqlite::params![me.pid, me.start_time as i64],
            )?;
            Ok(())
        })
        .unwrap();
    drop(registry);
    drop(lock);
    let later = SystemTime::now() + FIFTEEN_DAYS;
    let pinned = eligibility_of(&storage, later);
    assert!(
        matches!(pinned, Eligibility::PinnedSource(ref reason) if reason.contains("base-under-test")),
        "{pinned:?}"
    );
    let lock = v3::ReservationLock::acquire(&storage).unwrap();
    let mut registry = v3::open_base_refs_registry(&storage, KEY, &lock, None).unwrap();
    registry
        .with_barrier(|tx| {
            // A pid no process holds: the builder is gone.
            tx.execute("UPDATE base_builds SET owner_pid = 2147483646", [])?;
            Ok(())
        })
        .unwrap();
    drop(registry);
    drop(lock);
    assert!(matches!(
        eligibility_of(&storage, later),
        Eligibility::Eligible { .. }
    ));
}

/// The safeguards stay: every run prints the stop-all warning, a dry run
/// changes nothing, a live AFT process refuses the run, and `--yes` renames
/// the eligible set aside before deleting it. Retained v1/v2 data is listed
/// and never touched.
#[cfg(unix)]
#[test]
fn prune_keeps_its_safeguards_and_never_touches_retained_legacy_data() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let storage = v3_imported_root(dir.path());
    let retained = data_files(&storage)
        .into_iter()
        .filter(|(path, _)| {
            let relative = path.strip_prefix(&storage).unwrap();
            relative.starts_with("views")
                || relative.starts_with("blobs")
                || relative.starts_with("migration")
        })
        .collect::<BTreeMap<_, _>>();
    let before = data_files(&storage);

    let (exit, out) = run(&storage, false, &Census(Vec::new()));
    assert_eq!(exit, PruneExit::Clean, "{out}");
    assert!(out.starts_with(OPERATOR_CONTRACT), "{out}");
    assert!(out.contains("Dry run: 1 legacy set(s)"), "{out}");
    assert!(out.contains("Retained legacy data"), "{out}");
    assert!(out.contains("v2 family store  blobs/v2/family"), "{out}");
    assert!(out.contains("v1 view  views/0123456789abcdef"), "{out}");
    assert!(out.contains("v3 family present"), "{out}");
    assert_same(&before, &data_files(&storage), "a dry run changed bytes");

    let holder = CensusFinding::Aft {
        pid: 4242,
        daemon: true,
        command: "aft --subc /tmp/c.json".to_owned(),
    };
    let (exit, out) = run(&storage, true, &Census(vec![holder]));
    assert_eq!(exit, PruneExit::Refused, "{out}");
    assert_same(
        &before,
        &data_files(&storage),
        "a refused run changed bytes",
    );

    let (exit, out) = run(&storage, true, &Census(Vec::new()));
    assert_eq!(exit, PruneExit::Clean, "{out}");
    assert!(out.contains(&format!("Moved {KEY} aside")), "{out}");
    for kind in ["index", "symbols"] {
        assert!(!storage.join(kind).join(KEY).exists(), "{kind}: {out}");
    }
    let after = data_files(&storage);
    for (path, bytes) in &retained {
        assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
    }
    assert!(fs::read_dir(&storage).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(PRUNE_DIR_PREFIX)));
}
