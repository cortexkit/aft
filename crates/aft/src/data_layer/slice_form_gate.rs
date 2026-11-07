//! Checks on a real checkout that compact slices ([`super::slice`]) answer
//! every call-graph lookup exactly as the string-row slices they replaced
//! ([`super::legacy_slice`]) do, and that each compact slice reads back
//! from its stored record unchanged. Runs from `examples/slice_form_gate.rs`.
//!
//! The checkout is scanned, extracted and resolved once. The old reader
//! holds string-row slices built in memory; the new one reads compact
//! slices from a segment written the way a publication writes it, so the
//! comparison covers the stored record as well as the queries over it.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use aft_store::cache::BlockCache;
use aft_store::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};
use aft_store::segment::{Segment, SegmentMeta};

use super::gate::{compare_every_lookup, Tally};
use super::legacy_query::{LegacyMemorySlices, LegacySliceReader};
use super::legacy_slice::LegacyFileSlice;
use super::query::{SliceReader, SliceSource};
use super::slice::FileSlice;

/// Runs the gate on the checkout at `root`, writing its report to `out`.
/// The composed commands run on every `public_step`-th symbol. Returns how
/// many lookups answered differently plus how many slices did not read
/// back from their records unchanged.
///
/// # Errors
///
/// When the checkout cannot be scanned, extracted or resolved, or the
/// segment cannot be written or read.
pub fn run(
    root: &Path,
    threads: usize,
    public_step: usize,
    out: &mut dyn Write,
) -> Result<usize, String> {
    let io = |error: std::io::Error| error.to_string();
    let started = Instant::now();
    let built = super::trace_measure::build(root, threads)?;
    writeln!(
        out,
        "built {} parsed files in {:.0} ms",
        built.inputs.parses.len(),
        started.elapsed().as_secs_f64() * 1000.0
    )
    .map_err(io)?;

    let mut legacy = LegacyMemorySlices::default();
    let mut compact = std::collections::BTreeMap::new();
    for (path, resolution) in &built.resolved {
        let (key, parse) = &built.inputs.parses[path];
        legacy.insert(LegacyFileSlice::build(path, key, parse, resolution));
        compact.insert(path.clone(), FileSlice::build(path, key, parse, resolution));
    }
    let mut text = std::collections::BTreeMap::<&str, usize>::new();
    for slice in compact.values() {
        for (part, bytes) in slice.probe_strings() {
            *text.entry(part).or_default() += bytes;
        }
    }
    writeln!(out, "string table by part: {text:?}").map_err(io)?;

    let records = built
        .records
        .iter()
        .map(|(key, record)| (key.digest().to_string(), (*key, Arc::clone(record))))
        .collect::<std::collections::HashMap<_, _>>();
    let directory =
        std::env::temp_dir().join(format!("aft-slice-form-gate-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(io)?;
    let segment_path = directory.join("1.seg");
    let family = FamilyId::from_digest(Digest128::of(b"slice form gate"));
    let fragments = super::graph_store::write_segment(
        &segment_path,
        SegmentMeta {
            family,
            segment: SegmentId::new(1),
            incarnation: WriterIncarnation::new(1),
            created_unix_ms: 0,
        },
        &built.inputs,
        &built.resolved,
        &records,
    )?;
    let segment = Arc::new(
        Segment::open(
            &segment_path,
            family,
            &super::graph_store::known_sections(),
            Arc::new(BlockCache::new(256 << 20)),
        )
        .map_err(|error| error.to_string())?,
    );
    let stored = super::graph_store::StoredSlices::open(
        vec![Arc::clone(&segment)],
        &built.inputs,
        &fragments,
    )?;

    // Every stored record decodes to the slice that was stored.
    let mut unequal = 0usize;
    for (path, slice) in &compact {
        match stored.slice(path) {
            Some(read) if *read == *slice => {}
            other => {
                unequal += 1;
                if unequal <= 3 {
                    writeln!(
                        out,
                        "{path}: the stored slice {}",
                        if other.is_some() {
                            "differs from the built one"
                        } else {
                            "did not read back"
                        }
                    )
                    .map_err(io)?;
                }
            }
        }
    }
    let failures = stored.failures();
    writeln!(
        out,
        "stored slices: {} files, {unequal} differ from the built ones, {} read failures",
        compact.len(),
        failures.len()
    )
    .map_err(io)?;
    drop(compact);

    let ours = LegacySliceReader::new(legacy, root.to_path_buf(), "gate".to_string());
    let theirs = SliceReader::new(stored, root.to_path_buf(), "gate".to_string());
    let mut tally = Tally::new();
    let mut examples = Vec::new();
    let started = Instant::now();
    compare_every_lookup(
        &ours,
        &theirs,
        &built.inputs,
        public_step,
        &mut tally,
        &mut examples,
    );
    writeln!(
        out,
        "compared lookups in {:.0} ms (string rows vs compact stored slices)",
        started.elapsed().as_secs_f64() * 1000.0
    )
    .map_err(io)?;
    let mut differences = 0usize;
    for (name, (calls, differ, ours_ms, theirs_ms)) in &tally {
        differences += differ;
        writeln!(
            out,
            "  {name}: {calls} calls, {differ} differ; string rows {ours_ms:.0} ms, compact {theirs_ms:.0} ms"
        )
        .map_err(io)?;
    }
    for example in &examples {
        writeln!(out, "{example}").map_err(io)?;
    }
    let read_failures = theirs.source().failures().len();
    drop(theirs);
    drop(segment);
    let _ = std::fs::remove_dir_all(&directory);
    Ok(differences + unequal + read_failures)
}
