//! Resolves a real checkout with the data layer's parallel per-file resolver
//! and with today's join, and reports whether they agree.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example resolve_gate -- <checkout root> [threads]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout)]

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let threads = std::env::args()
        .nth(2)
        .map_or(8, |threads| threads.parse().unwrap());
    let report = aft::data_layer::gate::run(std::path::Path::new(&root), threads).unwrap();
    println!("{report:#?}");
    assert_eq!(
        (
            report.rows_only_new,
            report.rows_only_join,
            report.binding_mismatches,
            report.dispatch_mismatches
        ),
        (0, 0, 0, 0),
        "the data layer's resolution differs from the join's"
    );
}
