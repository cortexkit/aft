//! Offline production-artifact probes. Inputs are copied before any mutation.
//! Run with AFT_DISK_HUNT_INPUT pointing at aft.db, semantic.bin, derived.sqlite,
//! callgraph.sqlite and manifest.json captured together outside the daemon.

use std::time::Instant;

pub fn measure<T>(name: &str, operation: impl FnOnce() -> T) -> T {
    let before = usage();
    let started = Instant::now();
    let value = operation();
    let after = usage();
    eprintln!(
        "hunt {name}: elapsed_ms={} physical_bytes={} logical_bytes={}",
        started.elapsed().as_millis(),
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1)
    );
    value
}

fn usage() -> (u64, u64) {
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut buffer = [0_u8; 512];
    // Darwin RUSAGE_INFO_V4 places disk and logical write counters after the
    // UUID and respectively 17 and 27 eight-byte fields.
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            4,
            buffer.as_mut_ptr().cast(),
        )
    };
    assert_eq!(
        result, 0,
        "Darwin write accounting is required for this probe"
    );
    let counter =
        |n: usize| u64::from_ne_bytes(buffer[16 + n * 8..16 + (n + 1) * 8].try_into().unwrap());
    (counter(17), counter(27))
}
