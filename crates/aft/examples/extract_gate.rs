//! Extraction gate: the single-walk call-graph extractor against the one it
//! replaced, and incremental reparsing against full parsing.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example extract_gate -- repo <root>
//! cargo run --release -p agent-file-tools --example extract_gate -- repo <name> <root> <file list>
//! cargo run --release -p agent-file-tools --example extract_gate -- replay <edit pack.jsonl.gz>
//! ```
//!
//! `repo` compares both extractors on every file named in the list (one
//! path per line, relative to the root; given only a root, every file Git
//! tracks there) and times each on 1 and 8 threads. It first counts the
//! listed files of each language the call graph parses, then requires every
//! one of them to be compared: a file it cannot read, a file on which both
//! extractors fail, and a list with no parsed file at all each fail the gate.
//! Files above the checkout scan's size limit are compared too, though no
//! checkout extracts them, so that no file of a language goes unchecked.
//! `replay` replays an edit pack, checking each incremental record against
//! a full extraction, and fails when no edit was checked.
//! Each prints one JSON object on stdout, a per-language table on stderr,
//! and exits non-zero when any check fails.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::collections::BTreeMap;
use std::path::Path;

use aft::callgraph_store::join::CallgraphBlob;
use aft::data_layer::content::{is_parsed_language, language_for};
use aft::data_layer::extract_gate::{self, CorpusFile, Extractor};

/// The checkout scan's size limit: it never reads a larger file, so no
/// checkout extracts one. The gate compares such files anyway and counts
/// them.
const SCAN_LIMIT: usize = 1024 * 1024;

/// The extractor version both extractors are given; neither their output
/// equality nor their errors depend on it.
const GATE_VERSION: &str = "extract-gate";

/// What the gate found for one language.
#[derive(Debug, Default, serde::Serialize)]
struct LanguageCounts {
    /// Listed files the call graph parses as this language: the number of
    /// files that must be compared.
    files: usize,
    /// Files on which both extractors gave output, compared field by field.
    compared: usize,
    /// Compared files above the checkout scan's size limit.
    above_scan_limit: usize,
    /// Files that could not be read as text, so neither extractor ran.
    skipped: usize,
    mismatches: usize,
    /// Files on which exactly one extractor failed, or both failed with
    /// different errors.
    error_mismatches: usize,
    /// Files on which both extractors failed the same way.
    both_failed: usize,
}

/// A listed file of a parsed language that could not be compared.
#[derive(Debug, serde::Serialize)]
struct Skipped {
    path: String,
    language: String,
    reason: String,
}

/// The listed files, sorted out before anything is compared.
#[derive(Default)]
struct Inventory {
    /// Files to compare, in language order.
    files: Vec<CorpusFile>,
    skipped: Vec<Skipped>,
    /// Listed paths the call graph does not parse: other languages,
    /// resolver configuration (stored raw), files whose record holds only
    /// their TODO markers, symlinks and folders.
    not_parsed: usize,
}

/// Sorts the files of `root` named in `paths` into those to compare, those
/// of a parsed language that cannot be read, and those the call graph does
/// not parse. Unlike `extract_gate::load`, it drops no file of a parsed
/// language without saying why.
fn inventory(root: &Path, paths: &[String]) -> Inventory {
    let mut found = Inventory::default();
    for path in paths {
        let full = root.join(path);
        let bytes = match std::fs::symlink_metadata(&full) {
            // A checkout scan never follows a symlink or reads a folder.
            Ok(metadata) if !metadata.is_file() => {
                found.not_parsed += 1;
                continue;
            }
            Ok(_) => std::fs::read(&full).map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        // A file that cannot be read gets the language its path names.
        let language = language_for(path, bytes.as_deref().unwrap_or_default())
            .filter(|language| is_parsed_language(language));
        let Some(language) = language else {
            found.not_parsed += 1;
            continue;
        };
        let text = match bytes {
            Ok(bytes) => String::from_utf8(bytes).map_err(|_| "not UTF-8".to_string()),
            Err(error) => Err(format!("unreadable: {error}")),
        };
        match text {
            Ok(text) => found.files.push(CorpusFile {
                path: path.clone(),
                language,
                text,
            }),
            Err(reason) => found.skipped.push(Skipped {
                path: path.clone(),
                language,
                reason,
            }),
        }
    }
    found.files.sort_by(|a, b| a.language.cmp(&b.language));
    found
}

/// Each file of `files` on which both extractors fail with the same error,
/// as `path: error`.
fn both_failed(files: &[CorpusFile]) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| {
            let reference =
                CallgraphBlob::extract_reference(&file.text, &file.language, GATE_VERSION);
            let single = CallgraphBlob::extract(&file.text, &file.language, GATE_VERSION);
            match (reference, single) {
                (Err(left), Err(right)) if left == right => Some(format!("{}: {left}", file.path)),
                _ => None,
            }
        })
        .collect()
}

/// Every file Git tracks under `root`, relative to it.
fn tracked_files(root: &str) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["-C", root, "ls-files", "-z"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git ls-files failed in {root}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

fn repo(name: &str, root: &Path, paths: &[String]) {
    let inventory = inventory(root, paths);
    let mut languages = BTreeMap::<String, LanguageCounts>::new();
    for skipped in &inventory.skipped {
        let counts = languages.entry(skipped.language.clone()).or_default();
        counts.files += 1;
        counts.skipped += 1;
    }
    let mut examples = Vec::new();
    let mut failed_files = Vec::new();
    for files in inventory.files.chunk_by(|a, b| a.language == b.language) {
        let comparison = extract_gate::compare(files, 8);
        let counts = languages.entry(files[0].language.clone()).or_default();
        counts.files += files.len();
        counts.compared = files.len() - comparison.both_failed - comparison.error_mismatches;
        counts.above_scan_limit = files
            .iter()
            .filter(|file| file.text.len() > SCAN_LIMIT)
            .count();
        counts.mismatches = comparison.mismatches;
        counts.error_mismatches = comparison.error_mismatches;
        counts.both_failed = comparison.both_failed;
        examples.extend(comparison.examples);
        if comparison.both_failed > 0 {
            failed_files.extend(both_failed(files));
        }
    }

    let mut timings = Vec::new();
    for threads in [1, 8] {
        for extractor in [Extractor::Reference, Extractor::SingleWalk] {
            let timing = extract_gate::measure(&inventory.files, extractor, threads);
            timings.push(serde_json::json!({
                "extractor": format!("{extractor:?}"),
                "timing": timing,
            }));
        }
    }
    let compared = languages
        .values()
        .map(|counts| counts.compared)
        .sum::<usize>();
    println!(
        "{}",
        serde_json::json!({
            "repo": name,
            "listed": paths.len(),
            "not_parsed": inventory.not_parsed,
            "compared": compared,
            "languages": languages,
            "skipped": inventory.skipped,
            "both_failed": failed_files,
            "examples": examples,
            "timings": timings,
        })
    );

    eprintln!(
        "{name}: {} listed, {} not parsed by the call graph, {compared} compared",
        paths.len(),
        inventory.not_parsed
    );
    eprintln!(
        "{:<14} {:>6} {:>8} {:>11} {:>7} {:>11} {:>10} {:>15}",
        "language",
        "files",
        "compared",
        "above_limit",
        "skipped",
        "both_failed",
        "mismatches",
        "error_mismatches"
    );
    for (language, counts) in &languages {
        eprintln!(
            "{language:<14} {:>6} {:>8} {:>11} {:>7} {:>11} {:>10} {:>15}",
            counts.files,
            counts.compared,
            counts.above_scan_limit,
            counts.skipped,
            counts.both_failed,
            counts.mismatches,
            counts.error_mismatches
        );
    }

    let mut failures = Vec::new();
    if compared == 0 {
        failures.push("no file of a language the call graph parses was compared".to_string());
    }
    for (language, counts) in &languages {
        // Every file of a language present in the list must be compared.
        if counts.compared < counts.files {
            failures.push(format!(
                "{language}: {} of {} files compared",
                counts.compared, counts.files
            ));
        }
        if counts.mismatches > 0 || counts.error_mismatches > 0 {
            failures.push(format!(
                "{language}: the extractors differ on {} files and fail differently on {}",
                counts.mismatches, counts.error_mismatches
            ));
        }
    }
    for skipped in &inventory.skipped {
        failures.push(format!(
            "skipped {} ({}): {}",
            skipped.path, skipped.language, skipped.reason
        ));
    }
    for file in &failed_files {
        failures.push(format!("both extractors failed on {file}"));
    }
    assert!(
        failures.is_empty(),
        "{name}: the extraction gate failed:\n  {}",
        failures.join("\n  ")
    );
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("repo") => match &args[1..] {
            [root] => {
                let name = Path::new(root)
                    .canonicalize()
                    .ok()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| root.clone());
                repo(&name, Path::new(root), &tracked_files(root));
            }
            [name, root, list] => {
                let paths = std::fs::read_to_string(list)
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                repo(name, Path::new(root), &paths);
            }
            _ => usage(),
        },
        Some("replay") => {
            let replay = extract_gate::replay(Path::new(&args[1])).unwrap();
            println!("{}", serde_json::to_string(&replay).unwrap());
            eprintln!(
                "replay: {} base files, {} edits checked, {} skipped, {} mismatches",
                replay.base_files, replay.edits, replay.skipped, replay.mismatches
            );
            assert!(
                replay.edits > 0,
                "the pack has no edit of a file the call graph parses, so nothing was checked"
            );
            assert_eq!(
                replay.mismatches, 0,
                "an incremental record differs from a full extraction"
            );
        }
        _ => usage(),
    }
}

fn usage() -> ! {
    panic!("usage: extract_gate repo <root> | repo <name> <root> <list> | replay <pack>")
}
