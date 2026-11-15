//! Process boundary for the test-hooks-only call-graph exactness driver.
fn main() {
    if let Err(error) =
        aft::data_layer::checkout::exactness::run(std::env::args().skip(1).collect())
    {
        eprintln!("EXACTNESS FAILED: {error}");
        std::process::exit(1);
    }
}
