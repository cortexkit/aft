//! Runs the dead-code gate (`aft::inspect::dead_code_gate`) on one
//! directory: dead code as `aft_inspect` computes it, checked against an
//! independent computation field by field, before and after a few edits.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example dead_code_gate -- <root> <storage> [compare|new] [edits]
//! ```
//!
//! `storage` is a scratch directory for the checkout's store. `new` runs
//! `aft_inspect`'s computation alone, so the process measures its memory;
//! `compare` (the default) also runs the independent one and fails when
//! their findings or facts differ.
//! `edits` (default 3) is how many files gain a function for the second
//! round; the files are restored afterwards. One JSON line per phase,
//! comparison and count goes to stdout.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use aft::inspect::dead_code_gate::{run, Mode};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("a root directory");
    let storage = args.next().expect("a storage directory");
    let mode = match args.next().as_deref() {
        None | Some("compare") => Mode::Compare,
        Some("new") => Mode::New,
        Some(other) => panic!("unknown mode {other}"),
    };
    let edits = args.next().map_or(3, |edits| edits.parse().unwrap());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let summary = run(
        std::path::Path::new(&root),
        std::path::Path::new(&storage),
        mode,
        edits,
        &mut out,
    )
    .unwrap();
    eprintln!(
        "compared {} times; {} differed; {} rollups patched a kept graph",
        summary.comparisons, summary.differed, summary.patched
    );
    if mode == Mode::Compare {
        assert!(summary.comparisons > 0, "nothing was compared");
        assert_eq!(summary.differed, 0, "the findings or facts differ");
    }
}
