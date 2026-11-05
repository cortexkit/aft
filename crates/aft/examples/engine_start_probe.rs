//! Measures starting the incremental call-graph engine on a published
//! checkout: what a restarted AFT pays before it can apply its first edit.
//!
//! Builds the checkout at `root` into a temporary store, binds it again as a
//! restarted process would, and times each step of engine start, three times
//! (the first with the store's blocks not yet in the shared cache).
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example engine_start_probe -- <root> [threads]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::sync::Arc;
use std::time::Instant;

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let store = std::env::temp_dir().join(format!("aft-engine-probe-{}", std::process::id()));
    std::fs::create_dir_all(&store).unwrap();
    let family = FamilyId::from_digest(Digest128::of(b"engine start probe"));

    let started = Instant::now();
    let first = Checkout::bind(
        Arc::new(DataLayer::new(256 << 20, threads)),
        &store,
        family,
        &root,
    )
    .unwrap();
    let report = first.build().unwrap();
    println!(
        "cold build: {:.0} ms ({report:?})",
        started.elapsed().as_secs_f64() * 1000.0
    );
    drop(first);

    // A restarted process: a new layer, so nothing is cached in memory.
    let started = Instant::now();
    let checkout = Checkout::bind(
        Arc::new(DataLayer::new(256 << 20, threads)),
        &store,
        family,
        &root,
    )
    .unwrap();
    println!("bind: {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
    for run in 1..=3 {
        let timings = checkout.measure_engine_start().unwrap();
        println!(
            "engine start, run {run}: {}",
            timings
                .iter()
                .map(|(step, ms)| format!("{step} {ms:.0} ms"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // AFT_PROBE_REFS="caller file|needle|target file:symbol,..." prints a
    // caller's references and the callers reported for the targets.
    if let Ok(spec) = std::env::var("AFT_PROBE_REFS") {
        for probe in spec.split(';') {
            let mut parts = probe.split('|');
            let (Some(file), Some(needle), Some(targets)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let targets = targets
                .split(',')
                .filter_map(|target| target.split_once(':'))
                .map(|(file, symbol)| (file.to_string(), symbol.to_string()))
                .collect::<Vec<_>>();
            for line in checkout.probe_references(file, needle, &targets).unwrap() {
                println!("{line}");
            }
        }
    }
    let started = Instant::now();
    let missing = checkout.check_stored_inputs().unwrap();
    println!(
        "stored input terms: {} missing ({:.0} ms){}",
        missing.len(),
        started.elapsed().as_secs_f64() * 1000.0,
        missing
            .iter()
            .take(10)
            .map(|line| format!("\n  {line}"))
            .collect::<String>()
    );
    drop(checkout);
    let _ = std::fs::remove_dir_all(&store);
    assert!(missing.is_empty(), "the stored input terms miss readers");
}
