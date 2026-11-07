//! Checks that compact call-graph slices answer every lookup as the
//! string-row slices they replaced do, on a real checkout (see
//! `aft::data_layer::slice_form_gate`).
//!
//! ```text
//! cargo run --release -p agent-file-tools --example slice_form_gate -- <root> [threads] [public_step]
//! ```
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let root = std::fs::canonicalize(&args[0]).unwrap();
    let threads = args.get(1).map_or(8, |t| t.parse().unwrap());
    let public_step = args.get(2).map_or(400, |t| t.parse().unwrap());
    let mut out = std::io::stdout();
    let differences =
        aft::data_layer::slice_form_gate::run(&root, threads, public_step, &mut out).unwrap();
    if differences == 0 {
        println!("GATE PASSED");
    } else {
        println!("GATE FAILED: {differences} differences");
        std::process::exit(1);
    }
}
