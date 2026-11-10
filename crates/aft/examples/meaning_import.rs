//! Gate for reusing the old meaning index's vectors: fills one checkout's
//! vectors twice, each time into a fresh store, once taking what the old
//! meaning index under `<storage>` computed and once from the model alone,
//! then compares the two stores' vectors and the answers of meaning
//! searches over them.
//!
//! ```text
//! cargo run --profile stage -p agent-file-tools --example meaning_import -- \
//!     <checkout> <storage> <empty work dir> [queries]
//! ```
//!
//! `<storage>` is an AFT storage folder holding the old index's snapshot of
//! the checkout at `semantic/<project key>/semantic.bin`. `queries` is how
//! many searches to compare (300 by default), each the words of one chunk
//! name of the checkout. The model is the configured default (the local
//! MiniLM). Prints one JSON object to stdout and progress to stderr.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aft::config::SemanticBackendConfig;
use aft::data_layer::checkout::{Checkout, DataLayer};
use aft::data_layer::old_index::{snapshot_path, OldIndex, OldSnapshot};
use aft::data_layer::vectors::{cut_file, Cut, FillBudget, FillReport, Producer, VectorPlane};
use aft::semantic_index::{SemanticEmbeddingModel, SemanticIndexFingerprint, SemanticResult};
use aft_store::ids::{Digest128, FamilyId};
use serde_json::json;

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// The process's peak resident memory so far, in bytes.
#[cfg(unix)]
fn peak_rss() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes the struct it is given.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    let max = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    // Linux reports kilobytes, macOS bytes.
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

/// The process's peak resident memory so far; not measured here.
#[cfg(not(unix))]
fn peak_rss() -> u64 {
    0
}

/// Binds `root` to a fresh store at `store` and builds it.
fn bind(layer: &Arc<DataLayer>, store: &Path, root: &Path) -> Arc<Checkout> {
    let family = FamilyId::from_digest(Digest128::of(b"meaning import gate"));
    let checkout = Checkout::bind(Arc::clone(layer), store, family, root).unwrap();
    checkout.build().unwrap();
    Arc::new(checkout)
}

/// Fills `plane`, recording each text sent to the model.
fn fill(
    label: &str,
    plane: &VectorPlane,
    model: &Mutex<SemanticEmbeddingModel>,
    budget: FillBudget,
) -> (FillReport, f64, HashSet<String>) {
    let started = Instant::now();
    let mut sent = HashSet::new();
    let report = plane
        .fill(
            &mut |texts| {
                sent.extend(texts.iter().cloned());
                model.lock().unwrap().embed(texts)
            },
            budget,
            &mut |step| {
                eprintln!(
                    "{label}: {}/{} files, {} chunks imported, {} embedded, {:.0} s",
                    step.files_done,
                    step.files_total,
                    step.chunks_imported,
                    step.chunks_embedded,
                    started.elapsed().as_secs_f64()
                );
            },
        )
        .unwrap();
    assert!(report.error.is_none(), "{label}: {report:?}");
    (report, ms(started), sent)
}

/// Every distinct chunk text of the checkout's files, cut as a fill cuts
/// them, with each chunk's name.
fn chunks_of(root: &Path, fingerprint: &SemanticIndexFingerprint) -> BTreeMap<String, String> {
    let listed = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .unwrap();
    let mut chunks = BTreeMap::new();
    for path in listed.stdout.split(|byte| *byte == 0) {
        if path.is_empty() {
            continue;
        }
        let Ok(bytes) = std::fs::read(root.join(String::from_utf8_lossy(path).as_ref())) else {
            continue;
        };
        if let Cut::Chunks(cut) = cut_file(root, path, &bytes, fingerprint.embed_text_caps) {
            for chunk in cut {
                chunks.insert(chunk.text, chunk.detail.name);
            }
        }
    }
    chunks
}

/// A search result as its fields and score bits.
fn row(result: &SemanticResult) -> String {
    format!(
        "{} {} {:?} {:?} {}-{} {:08x} {:08x}",
        result.file.display(),
        result.name,
        result.qualified_name,
        result.kind,
        result.start_line,
        result.end_line,
        result.score.to_bits(),
        result.rank_score.to_bits(),
    )
}

/// The words of an identifier: `parse_rust_file` and `parseRustFile` both
/// give "parse rust file".
fn words(name: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if character == '_' || character == '-' || character == ':' || character == '.' {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower {
            out.push(' ');
        }
        previous_lower = character.is_lowercase() || character.is_ascii_digit();
        out.extend(character.to_lowercase());
    }
    out.trim().to_string()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let storage = PathBuf::from(args.next().unwrap());
    let work = PathBuf::from(args.next().unwrap());
    let query_count: usize = args.next().map_or(300, |count| count.parse().unwrap());

    let config = SemanticBackendConfig::default();
    let model = Mutex::new(SemanticEmbeddingModel::from_config(&config).unwrap());
    let fingerprint = model.lock().unwrap().fingerprint(&config).unwrap();
    let producer = Producer::from_fingerprint(&fingerprint);

    // The old snapshot alone: how long reading it takes and what its index
    // holds, before anything else has grown the process.
    let snapshot = snapshot_path(&storage, &root).unwrap();
    let snapshot_bytes = std::fs::metadata(&snapshot).unwrap().len();
    let snapshot_digest = blake3::hash(&std::fs::read(&snapshot).unwrap());
    let snapshot_modified = std::fs::metadata(&snapshot).unwrap().modified().unwrap();
    let rss_before_open = peak_rss();
    let started = Instant::now();
    let opened = OldSnapshot::open(&snapshot, &fingerprint)
        .unwrap()
        .expect("the old snapshot is there");
    let open_ms = ms(started);
    let rss_after_open = peak_rss();
    let open = json!({
        "path": snapshot,
        "bytes": snapshot_bytes,
        "files": opened.files(),
        "rows": opened.rows(),
        "index_bytes": opened.memory(),
        "read_ms": open_ms,
        "peak_rss_before": rss_before_open,
        "peak_rss_after": rss_after_open,
    });
    drop(opened);
    eprintln!("{open}");

    let threads = std::thread::available_parallelism().map_or(8, usize::from);
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let budget = FillBudget {
        max_batch: model.lock().unwrap().max_batch_size(),
        files_per_round: 256,
    };

    let imported_checkout = bind(&layer, &work.join("imported"), &root);
    let imported = VectorPlane::new(
        Arc::clone(&imported_checkout),
        producer.clone(),
        config.max_files,
    )
    .reusing_old_index(OldIndex::of_checkout(&storage, &root, fingerprint.clone()));
    let (imported_report, imported_ms, imported_sent) = fill("imported", &imported, &model, budget);
    let rss_after_import = peak_rss();

    let embedded_checkout = bind(&layer, &work.join("embedded"), &root);
    let embedded = VectorPlane::new(Arc::clone(&embedded_checkout), producer, config.max_files);
    let (embedded_report, embedded_ms, _) = fill("embedded", &embedded, &model, budget);

    // Every chunk's vector in both stores.
    let chunks = chunks_of(&root, &fingerprint);
    let mut compared = 0;
    let mut taken = 0;
    let mut taken_identical = 0;
    let mut identical = 0;
    let mut missing = 0;
    let mut max_difference = 0.0_f32;
    let mut different_examples = Vec::new();
    for text in chunks.keys() {
        let (Some(left), Some(right)) = (
            imported.stored_vector(text).unwrap(),
            embedded.stored_vector(text).unwrap(),
        ) else {
            missing += 1;
            continue;
        };
        compared += 1;
        let same = left.len() == right.len()
            && left
                .iter()
                .zip(&right)
                .all(|(left, right)| left.to_bits() == right.to_bits());
        let from_old = !imported_sent.contains(text);
        if from_old {
            taken += 1;
        }
        if same {
            identical += 1;
            if from_old {
                taken_identical += 1;
            }
        } else {
            for (left, right) in left.iter().zip(&right) {
                max_difference = max_difference.max((left - right).abs());
            }
            if different_examples.len() < 5 {
                different_examples.push(json!({
                    "from_old_index": from_old,
                    "text": text.chars().take(120).collect::<String>(),
                }));
            }
        }
    }

    // Searches over both stores, for the words of chunk names.
    let names: Vec<String> = chunks
        .values()
        .map(|name| words(name))
        .filter(|words| words.contains(' '))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let stride = (names.len() / query_count.max(1)).max(1);
    let queries: Vec<&String> = names.iter().step_by(stride).take(query_count).collect();
    let imported_view = imported_checkout.view().unwrap();
    let embedded_view = embedded_checkout.view().unwrap();
    let mut same_answers = 0;
    let mut answer_examples = Vec::new();
    for query in &queries {
        let vector = model
            .lock()
            .unwrap()
            .embed(vec![(*query).clone()])
            .unwrap()
            .remove(0);
        let left = imported
            .search(&imported_view, &vector, 20, &|_| true)
            .unwrap();
        let right = embedded
            .search(&embedded_view, &vector, 20, &|_| true)
            .unwrap();
        let left: Vec<String> = left.results.iter().map(row).collect();
        let right: Vec<String> = right.results.iter().map(row).collect();
        if left == right && !left.is_empty() {
            same_answers += 1;
        } else if answer_examples.len() < 3 {
            answer_examples.push(json!({ "query": query, "imported": left, "embedded": right }));
        }
    }

    let unchanged = blake3::hash(&std::fs::read(&snapshot).unwrap()) == snapshot_digest
        && std::fs::metadata(&snapshot).unwrap().modified().unwrap() == snapshot_modified;

    let summary = json!({
        "model": fingerprint.model,
        "dimensions": fingerprint.dimension,
        "root": root,
        "old_snapshot": open,
        "old_snapshot_unchanged": unchanged,
        "imported_fill": {
            "report": imported_report,
            "ms": imported_ms,
            "peak_rss_after": rss_after_import,
        },
        "model_fill": {
            "report": embedded_report,
            "ms": embedded_ms,
        },
        "vectors": {
            "chunk_texts": chunks.len(),
            "compared": compared,
            "missing_in_a_store": missing,
            "bit_identical": identical,
            "taken_from_old_index": taken,
            "taken_and_bit_identical": taken_identical,
            "max_abs_difference": max_difference,
            "different_examples": different_examples,
        },
        "searches": {
            "queries": queries.len(),
            "k": 20,
            "identical_answers": same_answers,
            "different_examples": answer_examples,
        },
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
