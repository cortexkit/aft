//! Replays an edit pack through the data layer's incremental call-graph
//! update, checking results against cold resolutions.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example edit_gate -- <pack.jsonl.gz> [threads] [check every N edits]
//! ```
//! Writes one JSON line per edit to stdout.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let pack = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let check_every = args.next().map_or(1, |every| every.parse().unwrap());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (checked, failures) = aft::data_layer::edit_gate::run(
        std::path::Path::new(&pack),
        threads,
        check_every,
        &mut out,
    )
    .unwrap();
    eprintln!("checked {checked} edits against cold resolutions; {failures} differed");
    if std::env::var_os("EDIT_GATE_TIMING_ONLY").is_some_and(|value| value == "1") {
        eprintln!("timing only: nothing was checked");
        return;
    }
    assert!(checked > 0, "no edit was checked against a cold resolution");
    assert_eq!(
        failures, 0,
        "an incremental update differs from a cold resolution"
    );
}
