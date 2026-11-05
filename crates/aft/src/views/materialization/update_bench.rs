//! Offline measurement of incremental call-graph updates on this repository.
//!
//! `bench_repository_incremental_update` clones the repository that contains
//! this crate (`git clone --shared`, read-only on the source), publishes two
//! revisions into private view storage to obtain real manifests and blobs, and
//! derives synthetic edit manifests from the newer revision. It then times a
//! cold materialization of each base and the incremental update of a clone of
//! that base, the same `materialize(.., Some(base))` call publication makes.
//! Run with `AFT_VIEW_PROFILE=1` to print every stage, including the method
//! dispatch pass and the surface replay. Nothing touches live storage.
//!
//! Environment (all optional):
//! - `AFT_UPDATE_BENCH_OLD` / `AFT_UPDATE_BENCH_NEW`: revisions of the branch
//!   switch (defaults `HEAD~50` and `HEAD`), resolved in the source checkout.
//! - `AFT_UPDATE_BENCH_EDITS`: comma-separated synthetic edit sizes
//!   (default `1,10,100`), applied to the newer revision.
//! - `AFT_UPDATE_BENCH_OUT`: parent directory for the scratch tree (default:
//!   the system temporary directory).
//! - `AFT_UPDATE_BENCH_PARITY=1`: also cold-build every target and require
//!   every derived table to equal the incremental result.
//! - `AFT_UPDATE_BENCH_MEMO_COMPARE=1`: also cold-build the newer revision
//!   with the resolver index's crate-wide lookups scanned instead of
//!   memoized, and require both databases to be equal.
//! - `AFT_UPDATE_BENCH_DISPATCH_COMPARE=1`: also cold-build the newer
//!   revision with the method-call pass writing every site row by row, and
//!   require the database to equal the one written by differences.
//! - `AFT_UPDATE_BENCH_SYNC_CLONE=1`: also run each incremental case (first,
//!   label suffix `-synced`) after flushing every pending write with sync(2).
//! - `AFT_UPDATE_BENCH_TABLE_SIZES=1`: checkpoint the newer revision's cold
//!   database and print each table's and index's size in the main file.
//! - `AFT_UPDATE_BENCH_ONLY`: comma-separated case labels to run
//!   (`switch-<first 12 hex of the target revision>` or `edit-<size>`).
//! - `AFT_UPDATE_BENCH_READER=1`: compare full and selective v3 blob-store reader patches,
//!   including single-file Rust/config edits and a Rust file rename.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use super::*;
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};

/// User plus system CPU seconds and peak resident set size for this process.
fn usage() -> (f64, i64) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes a complete rusage for RUSAGE_SELF.
    let usage = unsafe {
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()), 0);
        usage.assume_init()
    };
    let seconds =
        |time: libc::timeval| time.tv_sec as f64 + f64::from(time.tv_usec as i32) / 1_000_000.0;
    (
        seconds(usage.ru_utime) + seconds(usage.ru_stime),
        usage.ru_maxrss as i64,
    )
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Inserts a comment line at the middle of the file, which shifts the line of
/// every later definition and call site, and appends one new function. This is
/// the shape of an ordinary edit: changed positions plus a new definition.
fn edited_source(source: &str, language: &str, index: usize) -> String {
    let lines = source.split_inclusive('\n').collect::<Vec<_>>();
    let middle = lines.len() / 2;
    let mut edited = String::with_capacity(source.len() + 128);
    for (number, line) in lines.iter().enumerate() {
        if number == middle {
            edited.push_str(&format!("// aft update bench edit {index}\n"));
        }
        edited.push_str(line);
    }
    if !edited.ends_with('\n') {
        edited.push('\n');
    }
    if language == "rust" {
        edited.push_str(&format!("pub fn aft_update_bench_probe_{index}() {{}}\n"));
    } else {
        edited.push_str(&format!(
            "export function aftUpdateBenchProbe{index}() {{}}\n"
        ));
    }
    edited
}

#[test]
#[ignore = "offline measurement: publishes two revisions of this repository into scratch storage"]
fn bench_repository_incremental_update() {
    let source = Path::new(crate::test_support::manifest_dir()).join("../..");
    let source = source.canonicalize().unwrap();
    let old_revision = std::env::var("AFT_UPDATE_BENCH_OLD").unwrap_or_else(|_| "HEAD~50".into());
    let new_revision = std::env::var("AFT_UPDATE_BENCH_NEW").unwrap_or_else(|_| "HEAD".into());
    let old_sha = git(&source, &["rev-parse", &old_revision]);
    let new_sha = git(&source, &["rev-parse", &new_revision]);
    let edit_sizes = std::env::var("AFT_UPDATE_BENCH_EDITS")
        .unwrap_or_else(|_| "1,10,100".into())
        .split(',')
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<usize>().expect("edit size"))
        .collect::<Vec<_>>();
    let parity = std::env::var_os("AFT_UPDATE_BENCH_PARITY").is_some();
    let only = std::env::var("AFT_UPDATE_BENCH_ONLY").ok().map(|value| {
        value
            .split(',')
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    });
    let out = tempfile::tempdir_in(
        std::env::var_os("AFT_UPDATE_BENCH_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir),
    )
    .unwrap();
    println!(
        "update bench: source={} old={old_sha} new={new_sha} scratch={}",
        source.display(),
        out.path().display()
    );
    let checkout = out.path().join("checkout");
    let status = std::process::Command::new("git")
        .args(["clone", "--shared", "--quiet", "--no-checkout"])
        .arg(&source)
        .arg(&checkout)
        .status()
        .unwrap();
    assert!(status.success(), "git clone --shared");
    let storage = out.path().join("storage");
    let publish = |revision: &str| -> Manifest {
        git(&checkout, &["checkout", "--quiet", "--force", revision]);
        let started = Instant::now();
        let request = crate::views::assembly::AssemblyRequest {
            storage: storage.clone(),
            project_root: checkout.clone(),
            family: "update-bench".into(),
            scope: "update-bench".into(),
            desired_head: revision.into(),
            changed_paths: BTreeSet::new(),
            semantic_keys: Default::default(),
            require_semantic: false,
            allow_blob_put: true,
            callgraph: true,
        };
        let report = if std::env::var_os("AFT_UPDATE_BENCH_READER").is_some() {
            // Measure the graph patch below, not during manifest publication too.
            let mut prepared =
                crate::views::assembly::prepare_checkout_deferring(&request, true, &mut |_| Ok(()))
                    .unwrap();
            let report = prepared.commit().unwrap();
            prepared.settle_status();
            report
        } else {
            crate::views::assembly::publish_checkout(&request).unwrap()
        };
        println!(
            "published {revision} in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        report
            .manifest
            .expect("distinct revisions publish a manifest")
    };
    let old = publish(&old_sha);
    let new = publish(&new_sha);
    let mut store = crate::blob_store::BlobStore::open(
        &storage,
        "update-bench".to_string(),
        crate::blob_store::BlobPlane::Callgraph,
    )
    .unwrap();
    let blobs = store.path().to_path_buf();

    // Synthetic edits of the newer revision's own source files, spread across
    // the tree. The checkout is at the newer revision after its publication.
    let candidates = {
        let connection = Connection::open(&blobs).unwrap();
        let reader = ManifestViewBlobReader::new(&connection);
        new.entries()
            .filter_map(|(path, entry)| {
                let ManifestEntry::Regular {
                    planes,
                    resolution_input: false,
                    ..
                } = entry
                else {
                    return None;
                };
                let key = planes.callgraph.as_ref()?;
                let blob = reader.read_decoded(key).ok()??;
                let language = blob.parse()?.language.clone();
                matches!(language.as_str(), "rust" | "typescript")
                    .then(|| (path.clone(), entry.clone(), language))
            })
            .collect::<Vec<_>>()
    };
    let mut targets = vec![
        (format!("switch-{}", &new_sha[..12]), &old, new.clone()),
        (format!("switch-{}", &old_sha[..12]), &new, old.clone()),
    ];
    for size in edit_sizes {
        assert!(size <= candidates.len(), "{size} edits");
        let mut replaced = std::collections::BTreeMap::new();
        let mut edited_paths = Vec::new();
        for index in 0..size {
            let (path, entry, language) =
                &candidates[(2 * index + 1) * candidates.len() / (2 * size)];
            let absolute = checkout.join(std::str::from_utf8(path.as_bytes()).unwrap());
            let source = std::fs::read_to_string(&absolute).unwrap();
            let source = edited_source(&source, language, index);
            let key = crate::blob_store::CallgraphKey::from_bytes(
                source.as_bytes(),
                language.as_str(),
                crate::callgraph_extractor::PRODUCER,
            )
            .full_key();
            let payload = join::CallgraphBlob::extract(
                &source,
                language,
                crate::callgraph_extractor::PRODUCER,
            )
            .unwrap()
            .to_bytes()
            .unwrap();
            store.put(&key, &payload).unwrap();
            let ManifestEntry::Regular { mode, planes, .. } = entry else {
                unreachable!("candidates are regular entries");
            };
            replaced.insert(
                RelPath::new(path.as_bytes()).unwrap(),
                ManifestEntry::Regular {
                    mode: *mode,
                    planes: RegularPlanes {
                        callgraph: Some(key.to_hex()),
                        semantic: planes.semantic.clone(),
                    },
                    resolution_input: false,
                },
            );
            edited_paths.push(String::from_utf8_lossy(path.as_bytes()).into_owned());
        }
        let edited = Manifest::new(new.entries().map(|(path, entry)| {
            (
                path.clone(),
                replaced.get(path).cloned().unwrap_or_else(|| entry.clone()),
            )
        }))
        .unwrap();
        println!("edit-{size} paths: {}", edited_paths.join(" "));
        targets.push((format!("edit-{size}"), &new, edited));
    }
    if std::env::var_os("AFT_UPDATE_BENCH_READER").is_some() {
        for (label, path, language, source) in [
            (
                "edit-shared-rust",
                "crates/aft/src/callgraph_store/join.rs",
                "rust",
                format!(
                    "pub fn aft_view_patch_probe() {{}}\n{}",
                    std::fs::read_to_string(
                        checkout.join("crates/aft/src/callgraph_store/join.rs")
                    )
                    .unwrap()
                ),
            ),
            (
                "edit-cargo",
                "Cargo.toml",
                "config",
                format!(
                    "{}\n# offline callgraph parity edit\n",
                    std::fs::read_to_string(checkout.join("Cargo.toml")).unwrap()
                ),
            ),
            ("edit-package", "package.json", "config", {
                let mut package: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(checkout.join("package.json")).unwrap())
                        .unwrap();
                package["version"] = serde_json::json!("0.0.0-parity");
                serde_json::to_string(&package).unwrap()
            }),
        ] {
            let key = crate::blob_store::CallgraphKey::for_current(source.as_bytes(), language)
                .full_key();
            let blob = if language == "config" {
                join::CallgraphBlob::config(source.as_bytes(), crate::callgraph_extractor::PRODUCER)
            } else {
                join::CallgraphBlob::extract(
                    &source,
                    language,
                    crate::callgraph_extractor::PRODUCER,
                )
                .unwrap()
            };
            store.put(&key, &blob.to_bytes().unwrap()).unwrap();
            let edited = Manifest::new(new.entries().map(|(entry_path, entry)| {
                let mut entry = entry.clone();
                if entry_path.as_bytes() == path.as_bytes() {
                    let ManifestEntry::Regular { planes, .. } = &mut entry else {
                        panic!("regular source input")
                    };
                    planes.callgraph = Some(key.to_hex());
                }
                (entry_path.clone(), entry)
            }))
            .unwrap();
            targets.push((label.into(), &new, edited));
        }
        let renamed = Manifest::new(new.entries().map(|(path, entry)| {
            (
                if path.as_bytes() == b"crates/aft/src/views/materialization.rs" {
                    RelPath::new(b"crates/aft/src/views/materialization_renamed.rs").unwrap()
                } else {
                    path.clone()
                },
                entry.clone(),
            )
        }))
        .unwrap();
        targets.push(("rename-rust".into(), &new, renamed));
    }
    drop(store);

    let v3_reader = std::env::var_os("AFT_UPDATE_BENCH_READER").map(|_| {
        let connection = Connection::open(&blobs).unwrap();
        tests::reader_tests::v3_reader(&connection, &out.path().join("v3"))
    });

    let keeper = |path: &Path| {
        let keeper = Connection::open(path).unwrap();
        keeper.pragma_update(None, "journal_mode", "WAL").unwrap();
        keeper
            .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        keeper
    };
    let cold = |label: &str, manifest: &Manifest| -> std::path::PathBuf {
        let path = out.path().join(format!("derived-cold-{label}.sqlite"));
        let _keeper = keeper(&path);
        let (cpu_before, _) = usage();
        let started = Instant::now();
        materialize_manifest_view_database(&path, &blobs, manifest).unwrap();
        let (cpu_after, max_rss_kib) = usage();
        println!(
            "update bench cold label={label} entries={} wall_s={:.3} cpu_s={:.3} max_rss_kib={max_rss_kib}",
            manifest.entries().count(),
            started.elapsed().as_secs_f64(),
            cpu_after - cpu_before
        );
        path
    };
    if std::env::var_os("AFT_UPDATE_BENCH_MEMO_COMPARE").is_some() {
        // The same cold build with the resolver index's crate-wide lookups
        // scanned on every call (the code before they were memoized) must
        // write the same database.
        crate::callgraph_store::SCAN_WITHOUT_CRATE_MEMO.with(|scan| scan.set(true));
        let scanned = cold("new-scanned", &new);
        crate::callgraph_store::SCAN_WITHOUT_CRATE_MEMO.with(|scan| scan.set(false));
        let memoized = cold("new-memoized", &new);
        parity::assert_logical_parity(&memoized, &scanned);
        println!("update bench memo parity equal=true");
        for path in [scanned, memoized] {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
    if std::env::var_os("AFT_UPDATE_BENCH_DISPATCH_COMPARE").is_some() {
        // The same cold build with the method-call pass writing every site row
        // by row (the writer before it wrote only differences) must write the
        // same database.
        dispatch::WRITE_ROW_BY_ROW.with(|row_by_row| row_by_row.set(true));
        let row_by_row = cold("new-row-by-row", &new);
        dispatch::WRITE_ROW_BY_ROW.with(|row_by_row| row_by_row.set(false));
        let differences = cold("new-differences", &new);
        parity::assert_logical_parity(&differences, &row_by_row);
        println!("update bench dispatch writer parity equal=true");
        for path in [row_by_row, differences] {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
    let wanted = |base: &str| {
        targets.iter().any(|(label, target_base, _)| {
            only.as_ref().is_none_or(|only| only.contains(label))
                && (base == "old") == std::ptr::eq(*target_base, &old)
        })
    };
    let old_db = wanted("old").then(|| cold("old", &old));
    let new_db = wanted("new").then(|| cold("new", &new));
    if let Some(path) = new_db
        .as_ref()
        .filter(|_| std::env::var_os("AFT_UPDATE_BENCH_TABLE_SIZES").is_some())
    {
        // Space each table and index takes in the main file once the cold
        // build's WAL has been checkpointed into it.
        let connection = Connection::open(path).unwrap();
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        let total: i64 = connection
            .query_row("SELECT SUM(pgsize) FROM dbstat", [], |row| row.get(0))
            .unwrap();
        let mut sizes = connection
            .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY 2 DESC")
            .unwrap();
        for row in sizes
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
        {
            let (name, bytes) = row.unwrap();
            println!(
                "update bench table_size name={name} bytes={bytes} share={:.4}",
                bytes as f64 / total as f64
            );
        }
        println!(
            "update bench table_size total_bytes={total} file_bytes={} wal_bytes={}",
            std::fs::metadata(path).unwrap().len(),
            super::profile::wal_len(path)
        );
    }
    let variants = if std::env::var_os("AFT_UPDATE_BENCH_SYNC_CLONE").is_some() {
        // The flushed run goes first, in the position where the plain run
        // would otherwise wait behind the cold builds' pending writeback.
        vec![true, false]
    } else {
        vec![false]
    };
    let resolution_modes: &[bool] = if v3_reader.is_some() {
        &[true, false]
    } else {
        &[false]
    };
    let mut expected_by_manifest = BTreeMap::new();
    for (manifest, database) in [(&old, &old_db), (&new, &new_db)] {
        if let Some(database) = database {
            expected_by_manifest.insert(fingerprint(manifest).unwrap(), database.clone());
        }
    }
    for (case, base, target, synced, full_resolution) in
        targets.iter().flat_map(|(label, base, target)| {
            variants.iter().flat_map(move |synced| {
                resolution_modes
                    .iter()
                    .map(move |full| (label, base, target, *synced, *full))
            })
        })
    {
        if only.as_ref().is_some_and(|only| !only.contains(case)) {
            continue;
        }
        let label = &format!(
            "{case}{}{}",
            if synced { "-synced" } else { "" },
            if v3_reader.is_none() {
                ""
            } else if full_resolution {
                "-full-reader"
            } else {
                "-selected-reader"
            }
        );
        let base_db = if std::ptr::eq(*base, &old) {
            old_db.as_ref().unwrap()
        } else {
            new_db.as_ref().unwrap()
        };
        let size = manifest_diff_size(base, target);
        let db = out
            .path()
            .join(format!("derived-incremental-{label}.sqlite"));
        crate::views::generation::clone_derived(base_db, &db).unwrap();
        if synced {
            // Flush every pending write on the machine before timing, so the
            // materialization's first durable commit does not also wait for
            // writeback the preceding cold builds and clone left behind.
            let started = Instant::now();
            // SAFETY: sync(2) takes no arguments and cannot fail.
            unsafe { libc::sync() };
            println!(
                "update bench system_sync label={label} ms={:.3}",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        let incremental_keeper = keeper(&db);
        let (cpu_before, _) = usage();
        let epoch = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64()
        };
        let window_start = epoch();
        let started = Instant::now();
        let previous = tests::FORCE_FULL_RESOLUTION.replace(full_resolution);
        let result = if let Some(reader) = &v3_reader {
            materialize_reading(&db, target, Some(base), reader)
        } else {
            materialize(&db, &blobs, target, Some(base))
        };
        tests::FORCE_FULL_RESOLUTION.set(previous);
        let (stats, timings) = result.unwrap();
        let wall = started.elapsed().as_secs_f64();
        // Wall-clock bounds, to line the run up with system-call traces.
        println!(
            "update bench incremental_window label={label} start_epoch_s={window_start:.6} end_epoch_s={:.6}",
            epoch()
        );
        let (cpu_after, max_rss_kib) = usage();
        println!(
            "update bench incremental label={label} changed={} entries={} may_force_full_resolution={} product_path={} wall_s={wall:.3} cpu_s={:.3} max_rss_kib={max_rss_kib}",
            size.changed,
            size.entries,
            size.may_force_full_resolution,
            if crate::views::assembly::diff_exceeds_incremental_cutoff(size) {
                "cold"
            } else {
                "incremental"
            },
            cpu_after - cpu_before
        );
        println!("update bench stats label={label} {stats:?}");
        println!("update bench timings label={label} {timings:?}");
        println!(
            "update bench patch_wal label={label} bytes={}",
            super::profile::wal_len(&db)
        );
        // Commit cost outside materialization: one small write committed on a
        // connection configured like the materializer's, then the WAL frames
        // a separate PASSIVE checkpoint finds and the time it takes.
        {
            let mut connection = Connection::open(&db).unwrap();
            configure_materialization_connection(&connection).unwrap();
            let wal = super::profile::wal_len(&db);
            let transaction = connection.transaction().unwrap();
            transaction
                .execute(
                    "INSERT OR REPLACE INTO meta(k, v) VALUES('update_bench_probe', '1')",
                    [],
                )
                .unwrap();
            let started = Instant::now();
            transaction.commit().unwrap();
            println!(
                "update bench probe_commit label={label} ms={:.3} wal_bytes_before={wal} wal_bytes_after={}",
                started.elapsed().as_secs_f64() * 1000.0,
                super::profile::wal_len(&db)
            );
            // The probe row is not part of the materialized result that the
            // parity check below compares with a cold build.
            connection
                .execute("DELETE FROM meta WHERE k='update_bench_probe'", [])
                .unwrap();
            let started = Instant::now();
            let (busy, log, checkpointed) = connection
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .unwrap();
            println!(
                "update bench passive_checkpoint label={label} ms={:.3} busy={busy} log_frames={log} checkpointed_frames={checkpointed}",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        drop(incremental_keeper);
        if parity {
            let expected = expected_by_manifest
                .entry(fingerprint(target).unwrap())
                .or_insert_with(|| cold(&format!("target-{case}"), target));
            parity::assert_logical_parity(&db, expected);
            if v3_reader.is_some() {
                let materialization_path = if case == "rename-rust" {
                    "crates/aft/src/views/materialization_renamed.rs"
                } else {
                    "crates/aft/src/views/materialization.rs"
                };
                tests::reader_tests::assert_query_parity(
                    &db,
                    expected,
                    &checkout,
                    &[
                        (materialization_path, "apply_manifest_diff"),
                        (
                            "crates/aft/src/callgraph_store/join.rs",
                            "CallgraphBlob::extract",
                        ),
                        (materialization_path, "materialize_manifest_view_database"),
                    ],
                );
            }
            println!("update bench parity label={label} equal=true");
        }
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }
}
