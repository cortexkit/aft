//! Measures the symbol-cache prewarm a bind runs (see
//! [`super::outline::root_bound`]) on a real checkout: how long it takes,
//! what it leaves in the cache, and where its time goes per file. Runs from
//! `examples/symbol_prewarm_probe.rs`, which binds and builds the checkouts
//! and counts the process's allocations around each call.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use aft_store::ids::ContentKey;
use serde_json::{json, Value};

use super::checkout::CheckoutView;
use crate::parser::{SharedSymbolCache, SymbolCache};

/// A symbol cache of its own, as a newly configured root starts with.
#[must_use]
pub fn fresh_cache() -> SharedSymbolCache {
    Arc::new(RwLock::new(SymbolCache::new()))
}

/// Runs the prewarm a bind runs on `view` into `cache`, and reports what it
/// did, how long it took and the cache's estimated size after it.
///
/// # Errors
///
/// When a record cannot be read.
pub fn prewarm_into(view: &CheckoutView, cache: &SharedSymbolCache) -> Result<Value, String> {
    let generation = cache.read().map_err(|_| "poisoned")?.generation();
    let started = Instant::now();
    let report = super::outline::prewarm(view, cache, generation, &|| false)
        .map_err(|error| error.to_string())?;
    let elapsed = started.elapsed();
    let memory = cache.read().map_err(|_| "poisoned")?.estimated_memory();
    Ok(json!({
        "loaded": report.loaded,
        "cached": report.cached,
        "left": report.left,
        "ms": millis(elapsed),
        "cache_estimated_bytes": memory.estimated_bytes,
    }))
}

/// Times each step of a prewarm's per-file work on `view` that takes its
/// table from the file's record, into a cache of its own: the stat, the
/// record's read and decode, the file's read and its two hashes, the
/// symbols' conversion and the insert. Also counts the files whose recorded
/// stat evidence still proves their content, which a prewarm can take
/// without reading them.
///
/// # Errors
///
/// When a record cannot be read.
pub fn phases(view: &CheckoutView) -> Result<Value, String> {
    let root = view.root().to_path_buf();
    let mut cache = SymbolCache::new();
    let mut time = Phases::default();
    let mut files = 0u64;
    let mut loaded = 0u64;
    let mut bytes = 0u64;
    let mut symbols = 0u64;
    let mut stat_proven = 0u64;
    for record in view.current_state().paths() {
        if !super::content::is_parsed_language(&record.language) {
            continue;
        }
        files += 1;
        let Some(path) = absolute(&root, &record.path) else {
            continue;
        };
        let started = Instant::now();
        let Ok(modified) =
            crate::bounded_io::metadata(&path).and_then(|metadata| metadata.modified())
        else {
            continue;
        };
        time.stat += started.elapsed();

        let started = Instant::now();
        let proven = aft_store::stat::observe(&path)
            .ok()
            .flatten()
            .is_some_and(|now| record.stat.proves_unchanged(&now));
        time.evidence += started.elapsed();
        stat_proven += u64::from(proven);

        let started = Instant::now();
        let stored = view
            .outline_symbols(&record.path)
            .map_err(|error| error.to_string())?;
        time.record += started.elapsed();
        let Some(stored) = stored else {
            continue;
        };

        let started = Instant::now();
        let Ok(source) = crate::parser::read_source(&path) else {
            continue;
        };
        time.read += started.elapsed();
        bytes += source.len() as u64;

        let started = Instant::now();
        let same = ContentKey::of(source.as_bytes()) == stored.content;
        time.key += started.elapsed();
        if !same {
            continue;
        }

        let started = Instant::now();
        let Some(converted) = stored
            .symbols
            .iter()
            .map(super::content::symbol_from_outline)
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        time.convert += started.elapsed();
        symbols += converted.len() as u64;

        let started = Instant::now();
        let hash = crate::parser::content_hash_for_source(&source);
        time.hash += started.elapsed();

        let started = Instant::now();
        cache.insert(path, modified, source.len() as u64, hash, converted);
        time.insert += started.elapsed();
        loaded += 1;
    }
    Ok(json!({
        "files": files,
        "loaded": loaded,
        "source_bytes": bytes,
        "symbols": symbols,
        "stat_proven": stat_proven,
        "ms": {
            "stat": millis(time.stat),
            "stat_evidence": millis(time.evidence),
            "record": millis(time.record),
            "file_read": millis(time.read),
            "content_key": millis(time.key),
            "convert": millis(time.convert),
            "cache_hash": millis(time.hash),
            "insert": millis(time.insert),
        },
        "cache_estimated_bytes": cache.estimated_memory().estimated_bytes,
    }))
}

#[derive(Default)]
struct Phases {
    stat: Duration,
    evidence: Duration,
    record: Duration,
    read: Duration,
    key: Duration,
    convert: Duration,
    hash: Duration,
    insert: Duration,
}

fn millis(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10_000.0).round() / 10.0
}

fn absolute(root: &Path, relative: &[u8]) -> Option<PathBuf> {
    let relative = std::str::from_utf8(relative).ok()?;
    Some(
        relative
            .split('/')
            .fold(root.to_path_buf(), |path, part| path.join(part)),
    )
}
