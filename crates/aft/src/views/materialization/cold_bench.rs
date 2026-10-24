//! Paired, isolated measurements of private derived builds on the dense
//! 20k/60k TypeScript fixtures also used by the callgraph-store I/O benchmark.
//! Run this ignored test alone with `--nocapture --test-threads=1` on Linux.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn written() -> (u64, u64) {
    let counters = std::fs::read_to_string("/proc/self/io").unwrap();
    let counter = |name: &str| {
        counters
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
    };
    (counter("write_bytes:"), counter("wchar:"))
}

fn disk_bytes(path: &Path) -> u64 {
    ["", "-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| {
            std::fs::metadata(format!("{}{suffix}", path.display())).map_or(0, |m| m.len())
        })
        .sum()
}

struct StopSampler<'a>(&'a AtomicBool);

impl Drop for StopSampler<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn sample_disk_during<T>(path: &Path, peak: &AtomicU64, build: impl FnOnce() -> T) -> T {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Unwinding must stop sampling before the scope waits for its thread.
        let _stop_on_drop = StopSampler(&stop);
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(disk_bytes(path), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        build()
    })
}

#[test]
fn disk_sampler_panic_probe_child() {
    let Some(root) = std::env::var_os("AFT_TEST_DISK_SAMPLER_PANIC") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let peak = AtomicU64::new(0);
    let path = root.join("unpublished.sqlite");
    let result = std::panic::catch_unwind(|| {
        sample_disk_during(&path, &peak, || {
            std::fs::write(root.join("materialization-started"), b"started").unwrap();
            std::result::Result::<(), &str>::Err("injected materialization error").unwrap();
        });
    });
    assert!(result.is_err());
}

#[test]
fn disk_sampler_stops_when_materialization_panics() {
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "views::materialization::tests::cold_bench::disk_sampler_panic_probe_child",
            "--nocapture",
        ])
        .env("AFT_TEST_DISK_SAMPLER_PANIC", root.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(
                root.path().join("materialization-started").exists(),
                "probe never reached materialization"
            );
            panic!("disk sampler did not stop after the materialization panic");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(root.path().join("materialization-started").exists());
}

#[test]
#[ignore = "offline 20k/60k cold-build I/O measurement, several GB of scratch storage"]
fn measure_private_cold_build_io() {
    let sizes = std::env::var("AFT_COLD_BENCH_SIZES").unwrap_or_else(|_| "20000,60000".into());
    for files in sizes.split(',').map(|s| s.parse::<usize>().unwrap()) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let paths = crate::callgraph_store::perf_tests::write_scaling_fixture(&source, files);
        let sources = paths
            .iter()
            .map(|path| {
                (
                    path.strip_prefix(&source)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    std::fs::read_to_string(path).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let borrowed = sources
            .iter()
            .map(|(p, s)| (p.as_str(), s.as_str()))
            .collect::<Vec<_>>();
        let blobs = dir.path().join("blobs.sqlite");
        let connection = Connection::open(&blobs).unwrap();
        connection.execute_batch("CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL, payload_digest BLOB NOT NULL, payload_schema INTEGER NOT NULL)").unwrap();
        let manifest = manifest(&connection, &borrowed);
        drop(connection);
        drop(borrowed);
        drop(sources);
        let mut legacy = None;
        for (label, mode) in [
            ("legacy", (false, false)),
            ("indexes_only", (false, true)),
            ("durability_only", (true, false)),
            ("deferred_and_relaxed", (true, true)),
        ] {
            let path = dir.path().join(format!("{label}.sqlite"));
            let peak = AtomicU64::new(0);
            let before = written();
            let started = Instant::now();
            let mut materialize_ms = 0;
            sample_disk_during(&path, &peak, || {
                // WAL generations retain a schema-attached keeper through
                // publication. Measure that latency separately from the
                // eventual close/checkpoint.
                let keeper = (!mode.0).then(|| {
                    let keeper = crate::db::file_identity::IdentityConnection::open(
                        &path,
                        "cold-bench-keeper",
                    )
                    .unwrap();
                    keeper.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
                    keeper
                        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
                            row.get::<_, i64>(0)
                        })
                        .unwrap();
                    keeper
                });
                with_cold_mode(mode, || {
                    materialize_manifest_view_database(&path, &blobs, &manifest)
                })
                .unwrap();
                materialize_ms = started.elapsed().as_millis();
                drop(keeper);
                peak.fetch_max(disk_bytes(&path), Ordering::Relaxed);
            });
            let elapsed = started.elapsed();
            let after = written();
            println!("cold_io files={files} mode={label} materialize_ms={materialize_ms} total_wall_ms={} physical_write_bytes={} logical_write_bytes={} peak_disk_bytes={} final_bytes={}",
                elapsed.as_millis(), after.0.saturating_sub(before.0), after.1.saturating_sub(before.1), peak.load(Ordering::Relaxed), disk_bytes(&path));
            if label == "legacy" {
                legacy = Some(path);
            } else {
                parity::assert_logical_parity(&path, legacy.as_ref().unwrap());
                println!("cold_io parity files={files} mode={label} every_table_and_index=equal");
                crate::views::generation::remove_unpublished_derived(&path).unwrap();
            }
        }
    }
}
