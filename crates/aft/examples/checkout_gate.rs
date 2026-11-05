//! Replays an edit pack through a data-layer checkout on real files,
//! checking every commit end against a cold build of the same files.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example checkout_gate -- <pack.jsonl.gz> [threads]
//! ```
//! Writes one JSON line per commit to stdout.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let pack = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (checked, failed) =
        aft::data_layer::checkout_gate::run(std::path::Path::new(&pack), threads, &mut out)
            .unwrap();
    eprintln!("checked {checked} commit ends against cold builds; {failed} differed");
    assert_eq!(failed, 0, "a checkout's answers differ from a cold build");
}
