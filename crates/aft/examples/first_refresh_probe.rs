//! Measures the first call-graph refreshes after a restart: what a restarted
//! AFT pays to apply its first edits to a published checkout.
//!
//! Builds the checkout at `root` into a temporary store, binds it again as a
//! restarted process would, then appends a comment to each file named after
//! the root in turn, records it and refreshes the call graph, printing the
//! refresh's time and its update statistics. Every file is restored at the
//! end.
//!
//! ```text
//! cargo run --profile stage -p agent-file-tools --example first_refresh_probe -- <root> <file>...
//! ```
//!
//! With `AFT_PROBE_SETTLE_MS` set, the probe waits that long after binding
//! before the first edit.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let files = args.collect::<Vec<_>>();
    let store = std::env::temp_dir().join(format!("aft-refresh-probe-{}", std::process::id()));
    std::fs::create_dir_all(&store).unwrap();
    let family = FamilyId::from_digest(Digest128::of(b"first refresh probe"));
    let layer = || Arc::new(DataLayer::new(256 << 20, 8));

    let started = Instant::now();
    let first = Checkout::bind(layer(), &store, family, &root).unwrap();
    first.build().unwrap();
    println!("cold build: {:.0} ms", ms(started));
    drop(first);

    // A restarted process: a new layer, so nothing is cached in memory.
    let started = Instant::now();
    let checkout = Checkout::bind(layer(), &store, family, &root).unwrap();
    println!("bind: {:.0} ms", ms(started));
    if let Some(settle) = std::env::var("AFT_PROBE_SETTLE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(settle));
    }
    let mut originals = Vec::new();
    for (round, file) in files.iter().enumerate() {
        let path = root.join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        if !originals.iter().any(|(saved, _)| saved == &path) {
            originals.push((path.clone(), text.clone()));
        }
        let (head, rest) = text.split_once('\n').unwrap_or((&text, ""));
        std::fs::write(&path, format!("{head} // probe {round}\n{rest}")).unwrap();
        let started = Instant::now();
        checkout.record_paths(&[file.as_bytes().to_vec()]).unwrap();
        let recorded = ms(started);
        let started = Instant::now();
        let stats = checkout.refresh_callgraph().unwrap();
        let refreshed = ms(started);
        match stats {
            Some(stats) => println!(
                "edit {round} {file}: record {recorded:.0} ms, refresh {refreshed:.0} ms; update {:.0} ms (facts {:.0}, bind {:.0}, resolve {:.0}, dispatch {:.0}, sites {:.0}); rebound {}, resolved {}, replayed {}, dispatch replayed {}, sites {}, touched {}",
                stats.total_ms,
                stats.facts_ms,
                stats.bind_ms,
                stats.resolve_ms,
                stats.dispatch_ms,
                stats.sites_ms,
                stats.rebound,
                stats.resolved,
                stats.replayed,
                stats.dispatch_replayed,
                stats.sites_resolved,
                stats.touched.len(),
            ),
            None => println!("edit {round} {file}: record {recorded:.0} ms, refresh {refreshed:.0} ms; nothing to update"),
        }
    }
    for (path, text) in originals {
        std::fs::write(path, text).unwrap();
    }
    drop(checkout);
    let _ = std::fs::remove_dir_all(&store);
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
