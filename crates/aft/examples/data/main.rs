//! Measures the two store-growth fixes on a copy of one family's store
//! folder: folds on behalf of checkouts due one ([`Checkout::fold_other`])
//! and merges of vector segments ([`VectorPlane::merge_segments`]), each
//! with its wall and CPU time and the store's segment count and bytes
//! after it, and checks that meaning search answers the same before and
//! after the merges.
//!
//! `<publisher>` is the root of a checkout the store names, bound as the
//! checkout whose publications trigger both in a running AFT; binding it
//! reads its files (it writes none of them). Each `<searcher>` root is
//! bound too, and its view searched with a fixed query set before the
//! merges and after: stored vectors of the family's own chunks, so no
//! embedder is needed. Everything is written to `<family dir>` only, so
//! give it a copy.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example data -- <family dir> <publisher> [<searcher>...]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft::data_layer::vectors::{Producer, VectorPlane};
use aft_store::cache::BlockCache;
use aft_store::family::Family;
use aft_store::ids::{Digest128, FamilyId};
use aft_store::vector::VectorSegment;

/// User and system CPU time the process spent, in milliseconds.
#[cfg(unix)]
fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a buffer of the size getrusage writes.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return 0.0;
    }
    // SAFETY: the call succeeded, so it filled the buffer.
    let usage = unsafe { usage.assume_init() };
    let ms = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

#[cfg(not(unix))]
fn cpu_ms() -> f64 {
    0.0
}

fn family_id(dir: &Path) -> FamilyId {
    let name = dir.file_name().unwrap().to_str().unwrap();
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&name[2 * index..2 * index + 2], 16).unwrap();
    }
    FamilyId::from_digest(Digest128::from_bytes(bytes))
}

/// The newest generation's segment count and bytes, those holding vectors
/// among them, and the segment files on disk.
fn store_line(dir: &Path, family: FamilyId) -> String {
    let opened = Family::open(dir, family).unwrap();
    let snapshot = opened.snapshot().unwrap();
    let cache = Arc::new(BlockCache::new(1 << 20));
    let known = aft_store::vector::KNOWN;
    let mut vector_segments = 0;
    for entry in snapshot.manifest().segments() {
        let segment = aft_store::segment::Segment::open(
            &dir.join("segments").join(format!("{}.seg", entry.id.get())),
            family,
            &known,
            Arc::clone(&cache),
        );
        if segment.is_ok_and(|segment| VectorSegment::open(&segment).is_ok_and(|v| v.is_some())) {
            vector_segments += 1;
        }
    }
    let bytes = snapshot
        .manifest()
        .segments()
        .iter()
        .map(|entry| entry.len)
        .sum::<u64>();
    let files = std::fs::read_dir(dir.join("segments")).unwrap().count();
    format!(
        "generation {}: {} segments ({} with vectors), {:.1} MB; {} segment files on disk",
        snapshot.generation().get(),
        snapshot.manifest().segments().len(),
        vector_segments,
        bytes as f64 / 1e6,
        files
    )
}

/// The model of the newest vector segment, and up to `count` of the
/// family's stored vectors, spread over its vector segments, as queries.
fn model_and_queries(
    dir: &Path,
    family: FamilyId,
    count: usize,
) -> (aft_store::vector::ModelIdentity, Vec<Vec<f32>>) {
    let opened = Family::open(dir, family).unwrap();
    let snapshot = opened.snapshot().unwrap();
    let cache = Arc::new(BlockCache::new(16 << 20));
    let mut model = None;
    let mut vectors = Vec::new();
    let mut segments = Vec::new();
    for entry in snapshot.manifest().segments() {
        // A segment with sections of other planes does not open with the
        // vector sections alone; it holds no vectors.
        if let Ok(segment) =
            snapshot.open_segment(entry.id, &aft_store::vector::KNOWN, Arc::clone(&cache))
        {
            segments.push(segment);
        }
    }
    for segment in segments.iter().rev() {
        let Some(stored) = VectorSegment::open(segment).unwrap() else {
            continue;
        };
        let identity = stored.model().clone();
        if model.is_none() {
            model = Some(identity.clone());
        }
        if Some(&identity) != model.as_ref() || stored.is_empty() {
            continue;
        }
        // A few from each segment, from its start and middle.
        for ordinal in [0, stored.len() / 2] {
            if vectors.len() < count {
                vectors.push(stored.vector(ordinal).unwrap());
            }
        }
    }
    (model.expect("the family holds vectors"), vectors)
}

/// Every answer `plane` gives `checkout`'s view to `queries`, top 10 each.
fn answers(plane: &VectorPlane, checkout: &Checkout, queries: &[Vec<f32>]) -> Vec<String> {
    plane.refresh().unwrap();
    let view = checkout.view().expect("published");
    let mut rows = Vec::new();
    for query in queries {
        let found = plane.search(&view, query, 10, &|_| true).unwrap();
        for result in &found.results {
            rows.push(format!(
                "{:?} {:08x} {:08x}",
                result,
                result.score.to_bits(),
                result.rank_score.to_bits()
            ));
        }
        rows.push(format!("coverage {:?}", found.coverage));
    }
    rows
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let dir = PathBuf::from(&args[0]);
    let family = family_id(&dir);
    let publisher_root = PathBuf::from(&args[1]);
    let searcher_roots = args[2..].iter().map(PathBuf::from).collect::<Vec<_>>();
    let layer = Arc::new(DataLayer::new(256 << 20, 4));
    println!("before: {}", store_line(&dir, family));

    let started = Instant::now();
    let publisher =
        Arc::new(Checkout::bind(Arc::clone(&layer), &dir, family, &publisher_root).unwrap());
    println!(
        "bound {} in {:.0} ms ({} changed paths)",
        publisher_root.display(),
        started.elapsed().as_secs_f64() * 1000.0,
        publisher.changed_paths()
    );
    let (model, queries) = model_and_queries(&dir, family, 60);
    println!("{} query vectors of model {}", queries.len(), model.name);
    let producer = Producer::for_stored_model(model);
    let mut searchers = Vec::new();
    for root in &searcher_roots {
        let checkout = if *root == publisher_root {
            Arc::clone(&publisher)
        } else {
            Arc::new(Checkout::bind(Arc::clone(&layer), &dir, family, root).unwrap())
        };
        let plane = VectorPlane::new(Arc::clone(&checkout), producer.clone(), 1_000_000);
        let before = answers(&plane, &checkout, &queries);
        searchers.push((root.clone(), checkout, plane, before));
    }

    // Folds on behalf of other checkouts, one per publication in a running
    // AFT: here one after another until none is due.
    for round in 1..=32 {
        let (wall, cpu) = (Instant::now(), cpu_ms());
        let report = publisher.fold_other().unwrap();
        let (wall, cpu) = (wall.elapsed().as_secs_f64() * 1000.0, cpu_ms() - cpu);
        println!(
            "fold_other {round}: {wall:.0} ms wall, {cpu:.0} ms CPU; examined {} in {:.0} ms; folded {:?} ({} own, {} read): {:?}; skipped {:?}",
            report.examined,
            report.examine_ms,
            report.root,
            report.own_segments,
            report.read_segments,
            report.fold,
            report.skipped
        );
        println!("    {}", store_line(&dir, family));
        if report.fold.is_none() {
            break;
        }
    }

    // Merges of vector segments, one per fill round in a running AFT: here
    // one after another until there is nothing to merge.
    let merger = VectorPlane::new(Arc::clone(&publisher), producer, 1_000_000);
    for round in 1..=32 {
        let (wall, cpu) = (Instant::now(), cpu_ms());
        let report = merger.merge_segments().unwrap();
        let (wall, cpu) = (wall.elapsed().as_secs_f64() * 1000.0, cpu_ms() - cpu);
        println!("merge {round}: {wall:.0} ms wall, {cpu:.0} ms CPU: {report:?}");
        println!("    {}", store_line(&dir, family));
        if report.is_none() {
            break;
        }
    }

    for (root, checkout, plane, before) in &searchers {
        let after = answers(plane, checkout, &queries);
        let same = after == *before;
        let results = before
            .iter()
            .filter(|row| !row.starts_with("coverage"))
            .count();
        println!(
            "meaning search over {}: {} result rows for {} queries, same after the merges: {same}",
            root.display(),
            results,
            queries.len()
        );
        if !same {
            for (left, right) in before.iter().zip(&after) {
                if left != right {
                    println!("    before: {left}\n    after:  {right}");
                    break;
                }
            }
        }
    }
    println!("after: {}", store_line(&dir, family));
}
