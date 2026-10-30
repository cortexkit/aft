//! Single-writer, bounded-read diagnostics for the current frame-loop step.

use std::sync::atomic::{fence, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

// Entry points borrow static storage. Only decoded command names need copying;
// oversized names are explicitly marked as clipped when formatting a snapshot.
const COMMAND_BYTES: usize = 128;

pub(super) struct WorkStep {
    generation: AtomicU64,
    entry: AtomicPtr<u8>,
    entry_len: AtomicUsize,
    command: [AtomicU8; COMMAND_BYTES],
    command_len: AtomicUsize,
    request_id: AtomicU64,
    since_ms: AtomicU64,
}

impl Default for WorkStep {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            entry: AtomicPtr::new(std::ptr::null_mut()),
            entry_len: AtomicUsize::new(0),
            command: std::array::from_fn(|_| AtomicU8::new(0)),
            command_len: AtomicUsize::new(0),
            request_id: AtomicU64::new(0),
            since_ms: AtomicU64::new(0),
        }
    }
}

impl WorkStep {
    /// Only the frame loop publishes. The release fence before the relaxed
    /// payload stores pairs with the reader's acquire fence if it observes an
    /// in-progress write, so its final generation check cannot accept torn data.
    #[inline]
    pub(super) fn enter(&self, entry: &'static str, command: &str, request_id: u64, now_ms: u64) {
        let generation = self.generation.load(Ordering::Relaxed);
        self.generation
            .store(generation.wrapping_add(1), Ordering::Release);
        fence(Ordering::Release);
        self.entry
            .store(entry.as_ptr().cast_mut(), Ordering::Relaxed);
        self.entry_len.store(entry.len(), Ordering::Relaxed);
        let used = command.len().min(COMMAND_BYTES - 1);
        for (byte, value) in self.command[..used].iter().zip(command.as_bytes()) {
            byte.store(*value, Ordering::Relaxed);
        }
        self.command[used].store(0, Ordering::Relaxed);
        self.command_len.store(command.len(), Ordering::Relaxed);
        self.request_id.store(request_id, Ordering::Relaxed);
        self.since_ms.store(now_ms, Ordering::Relaxed);
        self.generation
            .store(generation.wrapping_add(2), Ordering::Release);
    }

    pub(super) fn clear(&self) {
        self.enter("", "", 0, 0);
    }

    pub(super) fn snapshot(&self, now_ms: u64) -> String {
        self.snapshot_with_probe(now_ms, || {})
    }

    fn snapshot_with_probe(&self, now_ms: u64, after_read: impl FnOnce()) -> String {
        // Never spin on a publisher: it may be suspended halfway through an
        // update. An explicitly incomplete snapshot is better than a watchdog
        // that waits for the very thread it is diagnosing.
        let before = self.generation.load(Ordering::Acquire);
        if before % 2 != 0 {
            return "entry=updating".to_string();
        }
        let entry = self.entry.load(Ordering::Relaxed);
        let entry_len = self.entry_len.load(Ordering::Relaxed);
        let command_len = self.command_len.load(Ordering::Relaxed);
        let used = command_len.min(COMMAND_BYTES - 1);
        let mut bytes = [0; COMMAND_BYTES];
        for (byte, source) in bytes[..used].iter_mut().zip(&self.command) {
            *byte = source.load(Ordering::Relaxed);
        }
        let request_id = self.request_id.load(Ordering::Relaxed);
        let since = self.since_ms.load(Ordering::Relaxed);
        after_read();
        fence(Ordering::Acquire);
        if self.generation.load(Ordering::Relaxed) != before {
            return "entry=updating".to_string();
        }
        if entry_len == 0 {
            return "entry=none".to_string();
        }
        // SAFETY: enter accepts only immutable static strings. The fences and
        // matching even generation above validate the pointer/length pair before
        // dereferencing it; the allocation can never be freed by a publisher.
        let entry =
            unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(entry, entry_len)) };
        format!(
            "entry={entry} command={:?}{} request_id={request_id} step_elapsed_ms={}",
            String::from_utf8_lossy(&bytes[..used]),
            if command_len >= COMMAND_BYTES {
                " [clipped]"
            } else {
                ""
            },
            now_ms.saturating_sub(since),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "optimized micro-benchmark; run explicitly with --ignored --nocapture"]
    fn benchmark_frame_loop_marks() {
        use std::hint::black_box;
        use std::time::Instant;

        const CALLS: u64 = 200_000;
        for (kind, command) in [("entry_only", ""), ("tool", "read")] {
            let step = WorkStep::default();
            let mut samples = [0.0; 7];
            for elapsed in &mut samples {
                let start = Instant::now();
                for request_id in 0..CALLS {
                    // Keep the slot address opaque so publishing stores cannot
                    // be elided just because the benchmark owns the slot.
                    black_box(&step).enter(
                        black_box("handle_tool_call"),
                        black_box(command),
                        black_box(request_id),
                        black_box(10),
                    );
                }
                *elapsed = start.elapsed().as_nanos() as f64 / CALLS as f64;
                black_box(step.snapshot(35));
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "frame_loop_mark kind={kind} calls_per_sample={CALLS} samples={} median_ns={:.2} best_ns={:.2}",
                samples.len(), samples[3], samples[0],
            );
        }
    }

    #[test]
    fn stalled_publisher_never_blocks_snapshot() {
        let step = WorkStep::default();
        step.enter("poll_if_woken", "read", 891, 10);
        assert_eq!(
            step.snapshot(35),
            "entry=poll_if_woken command=\"read\" request_id=891 step_elapsed_ms=25"
        );
        // Simulate a publisher stopped between the first and last atomic write.
        step.generation.fetch_add(1, Ordering::Release);
        assert_eq!(step.snapshot(40), "entry=updating");
    }

    #[test]
    fn stale_generation_returns_updating_without_retrying() {
        let step = WorkStep::default();
        step.enter("old_step", "read", 891, 10);
        assert_eq!(
            step.snapshot_with_probe(40, || step.enter("new_step", "write", 892, 30)),
            "entry=updating"
        );
        assert_eq!(
            step.snapshot(45),
            "entry=new_step command=\"write\" request_id=892 step_elapsed_ms=15"
        );
    }

    #[test]
    fn shorter_commands_and_clear_do_not_leak_old_bytes() {
        let step = WorkStep::default();
        step.enter("tool", "long_command", 1, 10);
        step.enter("tool", "read", 2, 20);
        assert_eq!(
            step.snapshot(25),
            "entry=tool command=\"read\" request_id=2 step_elapsed_ms=5"
        );
        step.enter("maintenance", "", 0, 30);
        assert_eq!(
            step.snapshot(35),
            "entry=maintenance command=\"\" request_id=0 step_elapsed_ms=5"
        );
        step.clear();
        assert_eq!(step.snapshot(40), "entry=none");
    }

    #[test]
    fn concurrent_snapshot_never_combines_entry_and_payload_generations() {
        let step = std::sync::Arc::new(WorkStep::default());
        let writer_step = std::sync::Arc::clone(&step);
        let writer = std::thread::spawn(move || {
            for _ in 0..100_000 {
                writer_step.enter("short", "read", 1, 10);
                writer_step.enter("a_longer_static_entry_point", "write", 2, 20);
            }
        });
        for _ in 0..100_000 {
            let snapshot = step.snapshot(30);
            assert!(
                matches!(snapshot.as_str(), "entry=none" | "entry=updating" |
                    "entry=short command=\"read\" request_id=1 step_elapsed_ms=20" |
                    "entry=a_longer_static_entry_point command=\"write\" request_id=2 step_elapsed_ms=10"),
                "{snapshot}",
            );
        }
        writer.join().unwrap();
    }
}
