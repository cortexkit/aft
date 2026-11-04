//! Resolves a real checkout with the data layer's parallel per-file resolver
//! and with today's join, and reports whether they agree. Then assembles the
//! call graph's rows from the data layer's resolution and compares them,
//! table by table, with the views database today's materialization writes
//! from the same inputs.
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
    assert_eq!(
        report.fragment_mismatches, 0,
        "a fragment does not round-trip"
    );
    assert!(
        report
            .tables
            .values()
            .all(|(_, _, differing)| *differing == 0),
        "the assembled call graph differs from today's views database"
    );
    assert!(
        report
            .queries
            .values()
            .all(|(_, differing, _, _)| *differing == 0),
        "a query over slices answers differently from today's database"
    );
}
