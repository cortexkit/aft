//! `aft cache prune-legacy [--yes]`: deletes the index folders AFT used before
//! version 0.64. The checks and the deletion live in
//! `aft::root_cache::prune_legacy_stores`; this file parses arguments and
//! prints what happened.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

use aft::root_cache::{LegacyStore, PruneLegacyError, PruneLegacyEvent, PruneLegacyReport};

/// The run failed with an unexpected error.
const EXIT_ERROR: i32 = 1;
/// The arguments were wrong.
const EXIT_USAGE: i32 = 2;
/// The run refused before changing anything.
const EXIT_REFUSED: i32 = 3;
/// At least one folder was (or in a dry run, would be) left in place.
const EXIT_SKIPPED: i32 = 5;

const USAGE: &str = "Usage: aft cache prune-legacy [--yes]

Deletes the index folders AFT used before version 0.64. AFT 0.64 and later
keep all indexes in the data/ folder and never read the old ones.

Without --yes, it only lists the folders and their sizes. Nothing is deleted.

With --yes, it moves each folder into a .prune-legacy-<time> folder beside
it, then deletes that folder. A folder that cannot be moved is skipped and
listed with the reason. If a run stops early, the next run deletes what it
left behind.

It looks in the AFT storage folder (AFT_STORAGE_DIR, or the default) and in
the cache folder set by storage.cache_dir, if there is one.

It deletes views/, blobs/, callgraph/, index/, symbols/, artifact-owners/,
migration/, semantic-batch-leases-v1/, everything in semantic/ except
semantic/models/, and the callgraph/ folder inside each harness folder. It
never deletes data/, the downloaded models in semantic/models/, your
settings, your backups, or anything else.

If a repository's meaning search has not finished its first fill, deleting
the old indexes in semantic/ means its remaining chunks are embedded again
instead of reused.

AFT can keep running while this command runs.

Exit codes: 0 done, 1 error, 2 wrong arguments, 3 refused (not an AFT
storage folder, or another cleanup is running), 5 a folder was skipped.
";

/// Shown whenever an old index inside `semantic/` is listed or deleted.
const SEMANTIC_NOTE: &str = "If a repository's meaning search has not finished its first fill, deleting the old indexes in semantic/ means its remaining chunks are embedded again instead of reused.";

/// Runs `aft cache <args>` and returns the process exit code.
pub fn run(args: Vec<OsString>) -> i32 {
    let mut args = args.into_iter();
    match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some("prune-legacy") => {}
        Some("--help" | "-h") | None => {
            print!("{USAGE}");
            return 0;
        }
        Some(other) => {
            eprintln!("Unknown cache command: {other}\n\n{USAGE}");
            return EXIT_USAGE;
        }
    }
    let mut yes = false;
    for arg in args {
        match arg.to_str() {
            Some("--yes" | "-y") => yes = true,
            Some("--help" | "-h") => {
                print!("{USAGE}");
                return 0;
            }
            _ => {
                eprintln!("Unknown argument: {}\n\n{USAGE}", arg.to_string_lossy());
                return EXIT_USAGE;
            }
        }
    }
    let storage = aft::bash_background::storage_dir(None);
    if !storage.is_absolute() {
        eprintln!(
            "AFT_STORAGE_DIR must be an absolute path: {}",
            storage.display()
        );
        return EXIT_USAGE;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("Cannot read the current folder: {error}");
            return EXIT_ERROR;
        }
    };
    let tiers = aft::subc_config::read_local_cortexkit_config_tiers(
        aft::subc_config::cortexkit_user_config_path().as_deref(),
        &cwd,
    );
    run_with(&tiers, &storage, yes, &mut io::stdout().lock())
}

/// The command body, with its inputs passed in so tests can run it against
/// a throwaway storage folder.
pub(crate) fn run_with(
    tiers: &[aft::config_resolve::ConfigTier],
    storage: &Path,
    yes: bool,
    out: &mut dyn Write,
) -> i32 {
    render(storage, yes, out, |on_event| {
        aft::root_cache::prune_legacy_stores(tiers, storage, yes, on_event)
    })
}

/// Prints the progress and result of `prune` and returns the exit code.
/// Tests pass a `prune` that returns a prepared result.
fn render(
    storage: &Path,
    yes: bool,
    out: &mut dyn Write,
    prune: impl FnOnce(
        &mut dyn FnMut(PruneLegacyEvent<'_>),
    ) -> Result<PruneLegacyReport, PruneLegacyError>,
) -> i32 {
    // Progress lines are written as the run goes; a failed write only loses
    // output, never changes what the run does.
    let result = prune(&mut |event| {
        let line = match event {
            PruneLegacyEvent::Deleting(store) => {
                format!("Deleting {} ({})...", store.path.display(), size(store))
            }
            PruneLegacyEvent::Removed(store) => format!("Deleted {}.", store.path.display()),
            PruneLegacyEvent::Skipped(skip) => {
                format!("Skipped {}. {}", skip.path.display(), skip.reason)
            }
        };
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
    });
    let code = match result {
        Ok(report) => {
            let _ = print_report(out, storage, yes, &report);
            if report.skipped.is_empty() && report.notes.is_empty() {
                0
            } else {
                EXIT_SKIPPED
            }
        }
        Err(PruneLegacyError::Refused(reason)) => {
            let _ = writeln!(out, "Refused: {reason} Nothing was changed.");
            EXIT_REFUSED
        }
        Err(PruneLegacyError::Failed(error)) => {
            let _ = writeln!(
                out,
                "aft cache prune-legacy stopped: {error}. Folders already moved aside stay in .prune-legacy-* folders, and the next run deletes them."
            );
            EXIT_ERROR
        }
    };
    let _ = out.flush();
    code
}

fn print_report(
    out: &mut dyn Write,
    storage: &Path,
    yes: bool,
    report: &PruneLegacyReport,
) -> io::Result<()> {
    if report.roots.is_empty() {
        writeln!(
            out,
            "There is no AFT storage folder at {}. Nothing to delete.",
            storage.display()
        )?;
        return Ok(());
    }
    if !yes {
        for root in &report.roots {
            let rows = rows(root, &report.stores);
            if rows.is_empty() {
                writeln!(out, "No old index folders in {}.", root.display())?;
                continue;
            }
            writeln!(out, "Old index folders in {}:", root.display())?;
            for row in &rows {
                match &row.note {
                    Some(note) => writeln!(out, "  {}  {}  ({note})", row.label, row.total.size())?,
                    None => writeln!(out, "  {}  {}", row.label, row.total.size())?,
                }
            }
        }
    }
    if !report.skipped.is_empty() {
        writeln!(
            out,
            "{}",
            if yes {
                "Left in place:"
            } else {
                "Will be left in place:"
            }
        )?;
        for skip in &report.skipped {
            writeln!(out, "  {}: {}", skip.path.display(), skip.reason)?;
        }
    }
    for note in &report.notes {
        writeln!(out, "{note}")?;
    }
    if report.stores.iter().any(is_semantic_store) {
        writeln!(out, "{SEMANTIC_NOTE}")?;
    }
    let total = Total::of(if yes { &report.removed } else { &report.stores });
    if !yes {
        if report.stores.is_empty() {
            writeln!(out, "Nothing to delete.")?;
        } else {
            writeln!(
                out,
                "Total: {} in {}.",
                total.size(),
                folders(report.stores.len())
            )?;
            writeln!(
                out,
                "This was a dry run. Nothing was deleted. Run `aft cache prune-legacy --yes` to delete these folders."
            )?;
        }
        return Ok(());
    }
    if report.stores.is_empty() && report.skipped.is_empty() {
        writeln!(out, "No old index folders found. Nothing was deleted.")?;
        return Ok(());
    }
    writeln!(
        out,
        "Deleted {}, {}.",
        folders(report.removed.len()),
        total.size()
    )?;
    if !report.skipped.is_empty() {
        writeln!(
            out,
            "Left {} in place. Fix the reason shown and run the command again.",
            folders(report.skipped.len())
        )?;
    }
    Ok(())
}

/// A sum of sizes that remembers whether any part was only a lower bound.
struct Total {
    bytes: u64,
    complete: bool,
}

impl Total {
    fn of<'a>(stores: impl IntoIterator<Item = &'a LegacyStore>) -> Self {
        let mut total = Self {
            bytes: 0,
            complete: true,
        };
        for store in stores {
            total.bytes = total.bytes.saturating_add(store.bytes);
            total.complete &= store.complete;
        }
        total
    }

    fn size(&self) -> String {
        sized(self.bytes, self.complete)
    }
}

fn size(store: &LegacyStore) -> String {
    sized(store.bytes, store.complete)
}

/// One line of the dry-run list.
struct Row {
    label: String,
    total: Total,
    /// Shown after the size, in parentheses.
    note: Option<String>,
}

/// The dry-run lines for one root, sorted by name. The old per-project
/// folders inside `semantic/` can number in the hundreds, so they share one
/// line.
fn rows(root: &Path, stores: &[LegacyStore]) -> Vec<Row> {
    let in_root = stores
        .iter()
        .filter(|store| store.path.starts_with(root))
        .collect::<Vec<_>>();
    let semantic = in_root
        .iter()
        .copied()
        .filter(|store| is_semantic_store(store))
        .collect::<Vec<_>>();
    let mut rows = in_root
        .iter()
        .filter(|store| !is_semantic_store(store))
        .map(|store| Row {
            label: relative(root, &store.path),
            total: Total::of([*store]),
            note: store.leftover.then(|| "left by an earlier run".to_string()),
        })
        .collect::<Vec<_>>();
    if !semantic.is_empty() {
        rows.push(Row {
            label: "semantic/".to_string(),
            note: Some(format!(
                "{}; semantic/models/ is kept",
                if semantic.len() == 1 {
                    "1 old index".to_string()
                } else {
                    format!("{} old indexes", semantic.len())
                }
            )),
            total: Total::of(semantic),
        });
    }
    rows.sort_by(|left, right| left.label.cmp(&right.label));
    rows
}

/// An old per-project folder inside `semantic/`.
fn is_semantic_store(store: &LegacyStore) -> bool {
    !store.leftover
        && store
            .path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "semantic")
}

/// "12.3 GB", or "at least 12.3 GB" when the size walk stopped at its bound.
/// Units are powers of 1024, as `du -h` shows them.
fn sized(bytes: u64, complete: bool) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let amount = if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    };
    if complete {
        amount
    } else {
        format!("at least {amount}")
    }
}

fn folders(count: usize) -> String {
    if count == 1 {
        "1 folder".to_string()
    } else {
        format!("{count} folders")
    }
}

/// `views/` for `<root>/views`, or the full path when it is not under `root`.
fn relative(root: &Path, path: &Path) -> String {
    format!("{}/", path.strip_prefix(root).unwrap_or(path).display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft::root_cache::LegacyStoreSkip;
    use std::fs;

    /// A throwaway AFT storage folder with one old store, two old semantic
    /// indexes and the live model cache beside them.
    fn storage() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        // On Windows canonical paths carry the verbatim `\\?\` prefix, and
        // the command prints the plain spelling.
        #[cfg(windows)]
        let base = aft::windows_path::normalize_windows_path(&base);
        let storage = base.join("storage");
        for (relative, contents) in [
            ("aft.db", "db"),
            ("views/view-1/derived.sqlite", "old view"),
            ("semantic/key-1/semantic.bin", "old index"),
            ("semantic/key-2/semantic.bin", "old index"),
            ("semantic/models/model.onnx", "model"),
            ("data/store.sqlite", "new store"),
        ] {
            let path = storage.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        (temp, storage)
    }

    fn text(output: Vec<u8>) -> String {
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn prune_legacy_cli_dry_run_lists_sizes_and_deletes_nothing() {
        let (_temp, storage) = storage();
        let mut output = Vec::new();
        assert_eq!(run_with(&[], &storage, false, &mut output), 0);
        let output = text(output);
        let expected = format!(
            "Old index folders in {}:\n  semantic/  18 bytes  (2 old indexes; semantic/models/ is kept)\n  views/  8 bytes\n{SEMANTIC_NOTE}\nTotal: 26 bytes in 3 folders.\nThis was a dry run. Nothing was deleted. Run `aft cache prune-legacy --yes` to delete these folders.\n",
            storage.display()
        );
        assert_eq!(output, expected);
        assert!(storage.join("views/view-1/derived.sqlite").is_file());
        assert!(storage.join("semantic/key-1/semantic.bin").is_file());
    }

    #[test]
    fn prune_legacy_cli_yes_deletes_and_reports_each_folder() {
        let (_temp, storage) = storage();
        let mut output = Vec::new();
        assert_eq!(run_with(&[], &storage, true, &mut output), 0);
        let output = text(output);
        for name in ["semantic/key-1", "semantic/key-2", "views"] {
            // Joined part by part, so Windows gets its own separators.
            let path = name.split('/').fold(storage.clone(), |path, part| path.join(part));
            assert!(
                output.contains(&format!("Deleted {}.", path.display())),
                "{output}"
            );
            assert!(!path.exists());
        }
        assert!(
            output.ends_with("Deleted 3 folders, 26 bytes.\n"),
            "{output}"
        );
        assert_eq!(
            fs::read_to_string(storage.join("semantic/models/model.onnx")).unwrap(),
            "model"
        );
        assert_eq!(
            fs::read_to_string(storage.join("data/store.sqlite")).unwrap(),
            "new store"
        );

        let mut again = Vec::new();
        assert_eq!(run_with(&[], &storage, true, &mut again), 0);
        assert_eq!(
            text(again),
            "No old index folders found. Nothing was deleted.\n"
        );
    }

    #[test]
    fn prune_legacy_cli_exit_code_says_a_folder_was_skipped() {
        let storage = Path::new("/storage");
        let index = LegacyStore {
            path: storage.join("index"),
            bytes: 2048,
            complete: false,
            leftover: false,
        };
        let views = LegacyStore {
            path: storage.join("views"),
            bytes: 1024,
            complete: true,
            leftover: false,
        };
        let mut output = Vec::new();
        let code = render(storage, true, &mut output, |on_event| {
            let skip = LegacyStoreSkip {
                path: index.path.clone(),
                reason: "It could not be moved aside, so nothing in it was deleted: busy"
                    .to_string(),
            };
            on_event(PruneLegacyEvent::Deleting(&views));
            on_event(PruneLegacyEvent::Removed(&views));
            on_event(PruneLegacyEvent::Skipped(&skip));
            Ok(PruneLegacyReport {
                roots: vec![storage.to_path_buf()],
                stores: vec![index.clone(), views.clone()],
                removed: vec![views.clone()],
                skipped: vec![skip],
                notes: Vec::new(),
            })
        });
        assert_eq!(code, EXIT_SKIPPED);
        let (index, views) = (index.path.display(), views.path.display());
        let reason = "It could not be moved aside, so nothing in it was deleted: busy";
        assert_eq!(
            text(output),
            format!(
                "Deleting {views} (1.0 KB)...\n\
                 Deleted {views}.\n\
                 Skipped {index}. {reason}\n\
                 Left in place:\n  \
                 {index}: {reason}\n\
                 Deleted 1 folder, 1.0 KB.\n\
                 Left 1 folder in place. Fix the reason shown and run the command again.\n"
            )
        );
    }

    #[test]
    fn prune_legacy_cli_refusal_and_bad_arguments_have_their_own_exit_codes() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        fs::create_dir_all(storage.join("views")).unwrap();
        let mut output = Vec::new();
        assert_eq!(run_with(&[], &storage, true, &mut output), EXIT_REFUSED);
        assert!(text(output).starts_with("Refused: "));
        assert!(storage.join("views").is_dir());

        assert_eq!(
            run(vec!["prune-legacy".into(), "--force".into()]),
            EXIT_USAGE
        );
        assert_eq!(run(vec!["prune".into()]), EXIT_USAGE);
    }

    #[test]
    fn prune_legacy_cli_sizes_say_at_least_when_the_walk_stopped_early() {
        assert_eq!(sized(512, true), "512 bytes");
        assert_eq!(sized(3 * 1024 * 1024 * 1024 / 2, true), "1.5 GB");
        assert_eq!(sized(2048, false), "at least 2.0 KB");
    }
}
