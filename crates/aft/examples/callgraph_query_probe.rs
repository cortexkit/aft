//! Measures what call-graph commands cost on a checkout whose call graph is
//! already published: how long a command takes when nothing of the graph is
//! decoded yet (the first command after a bind) and when it runs again, and
//! how much decoded call-graph data the checkout holds after each.
//!
//! The checkout is built once into a throwaway store. Each query then binds a
//! fresh checkout of that store, so its first run starts with nothing
//! decoded; the bind's own time is reported apart. A last pass decodes every
//! file, as a whole-graph command does, and reports per-file decode times and
//! the memory the fully decoded graph holds.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example callgraph_query_probe -- \
//!     <root> [threads] -- callers:crates/aft/src/context.rs:config ...
//! ```
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::sync::Arc;
use std::time::Instant;

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::heap::Seen;
use aft_store::ids::{Digest128, FamilyId};

fn callgraph_mb(checkout: &Checkout) -> f64 {
    let memory = checkout.memory(&mut Seen::new());
    let live = memory.live.map_or(0, |live| live.callgraph_bytes);
    live as f64 / 1e6
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let split = args.iter().position(|arg| arg == "--").unwrap_or(args.len());
    let root = std::fs::canonicalize(&args[0]).unwrap();
    let threads = args.get(1).filter(|_| split > 1).map_or(8, |t| t.parse().unwrap());
    let queries = args.get(split + 1..).unwrap_or_default().to_vec();
    let store = std::env::temp_dir().join(format!("aft-query-probe-{}", std::process::id()));
    std::fs::create_dir_all(&store).unwrap();
    let family = FamilyId::from_digest(Digest128::of(b"call-graph query probe"));
    let bind = || {
        Checkout::bind(
            Arc::new(DataLayer::new(256 << 20, threads)),
            &store,
            family,
            &root,
        )
        .unwrap()
    };

    let started = Instant::now();
    let built = bind();
    built.build().unwrap();
    println!("cold build: {:.0} ms", ms(started));
    drop(built);

    for query in &queries {
        let mut parts = query.splitn(3, ':');
        let (Some(command), Some(file), Some(symbol)) = (parts.next(), parts.next(), parts.next())
        else {
            eprintln!("skipping malformed query {query}");
            continue;
        };
        let started = Instant::now();
        let checkout = bind();
        let view = checkout.view().unwrap();
        let bind_ms = ms(started);
        let before = callgraph_mb(&checkout);
        let started = Instant::now();
        let first = view.probe_query(command, file, symbol);
        let first_ms = ms(started);
        let after_first = callgraph_mb(&checkout);
        let started = Instant::now();
        let _ = view.probe_query(command, file, symbol);
        let again_ms = ms(started);
        println!(
            "{command} {file}:{symbol}: bind {bind_ms:.0} ms; first {first_ms:.1} ms, again {again_ms:.1} ms; \
             decoded call graph {before:.1} -> {after_first:.1} MB; result {first:?}"
        );
    }

    let checkout = bind();
    let view = checkout.view().unwrap();
    let before = callgraph_mb(&checkout);
    let started = Instant::now();
    let mut decodes = view.probe_decode_all();
    let all_ms = ms(started);
    decodes.sort_unstable();
    let at = |q: f64| decodes[((decodes.len() as f64 - 1.0) * q) as usize];
    println!(
        "decode every file: {} files in {all_ms:.0} ms (one thread); per file p50 {} us, p90 {} us, p99 {} us, max {} us; \
         decoded call graph {before:.1} -> {:.1} MB",
        decodes.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        decodes.last().copied().unwrap_or(0),
        callgraph_mb(&checkout),
    );
    drop(view);
    drop(checkout);
    let _ = std::fs::remove_dir_all(&store);
}
