//! Resolves one file of a checkout again and again on one thread, so a
//! profiler can see where a large file's resolution spends its time.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example resolve_file_bench -- <checkout> <file> [rounds]
//! ```
//! Prints each round's resolve and method-call-site times to standard error.

#![allow(clippy::unwrap_used, clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap();
    let file = args.next().unwrap();
    let rounds = args.next().map_or(5, |rounds| rounds.parse().unwrap());
    aft::data_layer::edit_gate::bench_file(std::path::Path::new(&root), &file, rounds).unwrap();
}
