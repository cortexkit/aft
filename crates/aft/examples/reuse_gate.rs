//! Resolves every commit-end checkout of an edit pack by reusing other
//! checkouts' stored resolutions, and checks each against a cold resolution.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example reuse_gate -- <pack.jsonl.gz> [threads]
//! ```
//! Writes one JSON line per commit to stdout.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let pack = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (checked, failures) =
        aft::data_layer::edit_gate::run_reuse(std::path::Path::new(&pack), threads, &mut out)
            .unwrap();
    eprintln!("checked {checked} checkouts against cold resolutions; {failures} differed");
    assert!(checked > 0, "no checkout was checked");
    assert_eq!(
        failures, 0,
        "a reused resolution differs from a cold resolution"
    );
}
