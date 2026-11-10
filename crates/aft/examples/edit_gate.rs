//! Replays an edit pack through the data layer's incremental call-graph
//! update, checking results against cold resolutions.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example edit_gate -- <pack.jsonl.gz> [threads] [check every N edits]
//! ```
//! Writes one JSON line per edit to stdout. With `EDIT_GATE_EDITS=N` it
//! replays only the pack's first N edits. With `EDIT_GATE_TIMING_ONLY=1`
//! it skips every check, for profiling the updates alone; such a run checks
//! nothing, so it says so and exits with status 3, never as a pass.

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
    if std::env::var_os("EDIT_GATE_TIMING_ONLY").is_some_and(|value| value == "1") {
        eprintln!(
            "EDIT_GATE_TIMING_ONLY=1: every check was skipped, so this run checked nothing \
             and is not a pass; unset it to run the gate"
        );
        std::process::exit(3);
    }
    eprintln!("checked {checked} edits against cold resolutions; {failures} differed");
    assert!(checked > 0, "no edit was checked against a cold resolution");
    assert_eq!(
        failures, 0,
        "an incremental update differs from a cold resolution"
    );
}
