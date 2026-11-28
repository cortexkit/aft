#[allow(unused_imports)]
use super::*;

#[test]
#[ignore = "manual cold-checkout timing with installed language servers"]
fn scoped_inspect_cold_checkout_phase_timings() {
    let root = PathBuf::from(std::env::var_os("AFT_INSPECT_REPRO_ROOT").expect("checkout root"));
    let started = Instant::now();
    let ctx = configured_context(&root);
    eprintln!("configure_ms={}", started.elapsed().as_millis());
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let monitor_finished = finished.clone();
    let monitor = thread::spawn(move || {
        let started = Instant::now();
        let mut previous = String::new();
        while !monitor_finished.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(log) = inspect_phase_log_for_request("cold-checkout-timings") {
                let current = format!("{:?}", log.records);
                if current != previous {
                    eprintln!(
                        "elapsed_ms={} phases={current}",
                        started.elapsed().as_millis()
                    );
                    previous = current;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let started = Instant::now();
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "cold-checkout-timings", "command": "inspect", "worker_session": true,
            "sections": "diagnostics",
            "scope": ["crates/aft-tools/src/commands/inspect.rs", "packages/opencode-plugin/src/tools/inspect.ts"]
        }),
    );
    finished.store(true, std::sync::atomic::Ordering::Release);
    monitor.join().unwrap();
    eprintln!(
        "inspect_ms={} response={response:#}",
        started.elapsed().as_millis()
    );
    for phase in inspect_phase_log_for_request("cold-checkout-timings")
        .unwrap()
        .records
    {
        eprintln!(
            "phase={:?} duration_ms={:?} error={:?}",
            phase.entry,
            phase.duration_ms(),
            phase.terminal_error
        );
    }
}
