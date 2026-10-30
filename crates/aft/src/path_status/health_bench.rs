//! Offline writer and health-tick costs on a large ledger, using production paths.
use super::*;

fn cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

#[test]
#[ignore = "offline 50,000-status / 50,000-skip writer and tick measurement"]
fn bench_large_ledger_writer_and_tick() {
    let dir = tempfile::tempdir().unwrap();
    let health = Health::for_view(dir.path());
    let mut writer = PathStatusStore::open(dir.path()).unwrap();
    // Seed outside the timers; every measured mutation uses the normal
    // transaction and post-commit health-notification path.
    let transaction = writer.connection.transaction().unwrap();
    transaction
        .execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<50000)
         INSERT INTO path_status(rel_path, state, reason, since_generation)
         SELECT CAST(printf('status/%05d.rs',x) AS BLOB), 'pending', 'bench', 1 FROM n;
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<50000)
         INSERT INTO path_skips(rel_path, reason, since_generation)
         SELECT CAST(printf('skips/%05d.rs',x) AS BLOB),
                CASE x%2 WHEN 0 THEN 'unsupported_git_mode' ELSE 'non_utf8_source' END, 1 FROM n;",
        )
        .unwrap();
    transaction.commit().unwrap();
    let initial = health.snapshot().1.unwrap();
    assert_eq!(initial.pending_count, 50_000);
    assert_eq!(initial.skipped_by_reason.values().sum::<usize>(), 50_000);
    const TICKS: usize = 20;
    for batches_per_tick in [1, 20] {
        let mut writer_cpu = 0;
        let mut tick_cpu = 0;
        let mut writer_elapsed = Duration::ZERO;
        let mut tick_elapsed = Duration::ZERO;
        let mut writer_scans = 0;
        let mut tick_scans = 0;
        let reads_before = super::super::HEALTH_READS.with(std::cell::Cell::get);
        let opens_before = super::super::STORE_OPENS.with(std::cell::Cell::get);
        for tick in 0..TICKS {
            for batch in 0..batches_per_tick {
                let scans = super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get);
                let cpu = cpu_ns();
                let elapsed = Instant::now();
                if (tick + batch) % 2 == 0 {
                    writer
                        .mark_failed(b"status/00001.rs", "bench failure", 2)
                        .unwrap();
                } else {
                    writer
                        .mark_pending_many([(&b"status/00001.rs"[..], "bench")], 2)
                        .unwrap();
                }
                writer_elapsed += elapsed.elapsed();
                writer_cpu += cpu_ns() - cpu;
                writer_scans +=
                    super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get) - scans;
            }
            let scans = super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get);
            let cpu = cpu_ns();
            let elapsed = Instant::now();
            let summary = health.snapshot().1.unwrap();
            tick_elapsed += elapsed.elapsed();
            tick_cpu += cpu_ns() - cpu;
            tick_scans += super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get) - scans;
            let failed = usize::from((tick + batches_per_tick - 1) % 2 == 0);
            assert_eq!(summary.failed_count, failed);
            assert_eq!(summary.pending_count, 50_000 - failed);
            assert_eq!(
                summary.skipped_by_reason.get("unsupported_git_mode"),
                Some(&25_000)
            );
            assert_eq!(
                summary.skipped_by_reason.get("non_utf8_source"),
                Some(&25_000)
            );
        }
        let batches = TICKS * batches_per_tick;
        println!(
            "large ledger: status_rows=50000 skip_rows=50000 ticks={TICKS} batches_per_tick={batches_per_tick} writer_cpu_us/batch={:.2} writer_elapsed_us/batch={:.2} tick_cpu_us/tick={:.2} tick_elapsed_us/tick={:.2} total_cpu_us/tick={:.2} writer_summary_scans={writer_scans} tick_summary_scans={tick_scans} tick_recounts={} tick_sqlite_opens={}",
            writer_cpu as f64 / batches as f64 / 1000.,
            writer_elapsed.as_secs_f64() * 1_000_000. / batches as f64,
            tick_cpu as f64 / TICKS as f64 / 1000.,
            tick_elapsed.as_secs_f64() * 1_000_000. / TICKS as f64,
            (writer_cpu + tick_cpu) as f64 / TICKS as f64 / 1000.,
            super::super::HEALTH_READS.with(std::cell::Cell::get) - reads_before,
            super::super::STORE_OPENS.with(std::cell::Cell::get) - opens_before,
        );
    }
}
