//! Release-only wall/thread-CPU probe for the real Rust input fingerprint.
//! Run with `cargo run --release -p agent-file-tools --example rust_fingerprint_probe --features test-hooks -- <corpus>`.

#[cfg(all(feature = "test-hooks", target_os = "linux"))]
fn main() {
    use std::time::{Duration, Instant};

    use aft::lsp::FingerprintProbe;

    fn cpu() -> Duration {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // The pointer refers to writable storage with the layout clock_gettime expects.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
            0
        );
        Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
    }

    let root =
        std::fs::canonicalize(std::env::args_os().nth(1).expect("corpus path required")).unwrap();
    let start_cpu = cpu();
    let start = Instant::now();
    let cold = FingerprintProbe::capture(&root, None);
    let cold_wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let cold_cpu_ms = (cpu() - start_cpu).as_secs_f64() * 1000.0;
    let Some(cold) = cold else {
        println!(
            "{}",
            serde_json::json!({"complete": false, "cold_wall_ms": cold_wall_ms, "cold_cpu_ms": cold_cpu_ms})
        );
        return;
    };
    let start_cpu = cpu();
    let start = Instant::now();
    let warm =
        FingerprintProbe::capture(&root, Some(&cold)).expect("warm fingerprint must complete");
    let warm_wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let warm_cpu_ms = (cpu() - start_cpu).as_secs_f64() * 1000.0;
    assert!(cold.same_digest(&warm));
    let (examined, inputs, rust_files, rust_bytes, cold_hashed) = cold.stats();
    let (_, _, _, _, warm_hashed) = warm.stats();
    println!(
        "{}",
        serde_json::json!({"complete": true, "cold_wall_ms": cold_wall_ms, "cold_cpu_ms": cold_cpu_ms, "warm_wall_ms": warm_wall_ms, "warm_cpu_ms": warm_cpu_ms, "examined": examined, "inputs": inputs, "rust_files": rust_files, "rust_bytes": rust_bytes, "cold_hashed": cold_hashed, "warm_hashed": warm_hashed})
    );
}

#[cfg(not(all(feature = "test-hooks", target_os = "linux")))]
fn main() {
    eprintln!("rust_fingerprint_probe requires Linux and --features test-hooks");
    std::process::exit(1);
}
