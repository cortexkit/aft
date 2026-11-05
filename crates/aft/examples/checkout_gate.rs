//! Replays an edit pack through two data-layer checkouts of one repository
//! on real files, one driven through its handle and one through the
//! runtime, checking every answer and recorded input against cold builds
//! of the same files (see `aft::data_layer::checkout_gate`).
//!
//! ```text
//! cargo run --release -p agent-file-tools --example checkout_gate -- <pack.jsonl.gz> [threads]
//! ```
//! Writes one JSON line per comparison to stdout. `CHECKOUT_GATE_ONLY`
//! (comma-separated path prefixes) keeps only the pack's paths under them,
//! for a shorter run; `CHECKOUT_GATE_PUBLISH_EDITS` sets how many edits pass
//! between publications of a checkout's changes within a commit (8 by
//! default). Exits nonzero when any comparison differs, when nothing was
//! compared, or when checkout A never read from enough segments for a
//! build to fold them.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let pack = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let summary =
        aft::data_layer::checkout_gate::run(std::path::Path::new(&pack), threads, &mut out)
            .unwrap();
    eprintln!(
        "compared checkouts with cold builds {} times; {} differed; {} publications of changes, \
         {} builds that folded A's segments (at most {} segments)",
        summary.comparisons,
        summary.failed,
        summary.publications,
        summary.compactions,
        summary.most_segments
    );
    assert!(summary.comparisons > 0, "nothing was compared");
    assert_eq!(summary.failed, 0, "a checkout differs from a cold build");
    assert!(
        summary.compactions > 0,
        "checkout A never read from more than eight segments, so no build folded them: \
         replay more edits or lower CHECKOUT_GATE_PUBLISH_EDITS"
    );
}
