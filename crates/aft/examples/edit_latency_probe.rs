//! Replays a run of real-sized edits through the undo snapshot and the
//! source write that `edit`, `write` and `apply_patch` use, while other
//! threads write to the same `aft.db` the way a live daemon does, and reports
//! p50/p90/max for every step of both phases (see `aft::backup::latency`).
//!
//! Run from the workspace root with:
//! `cargo run --release -p agent-file-tools --example edit_latency_probe -- [options]`
//!
//! Options:
//! - `--edits N`       edits to replay (default 300)
//! - `--files N`       distinct source files to cycle through (default 40)
//! - `--dir PATH`      work folder; must not be under a system temp folder,
//!                     where snapshots are skipped (default
//!                     `<crate>/target/edit-latency-probe`)
//! - `--no-writers`    no concurrent `aft.db` writers
//! - `--reader`        hold a read transaction open most of the time, so the
//!                     WAL cannot restart and every checkpoint has work to do
//! - `--wal-pages N`   WAL autocheckpoint threshold of the shared connection
//!                     (default SQLite's 1000; smaller makes checkpoints frequent)
//! - `--io-load MB`    a background thread rewrites and syncs a file of this
//!                     size while the replay runs, like a build writing output
//!
//! The corpus is the crate's own `src/` files, copied into the work folder, so
//! snapshot sizes follow real source files. Every edit changes one line.

use aft::backup::latency::{self, Part, PartTimes};
use aft::backup::BackupStore;
use aft::db::TrackedConnection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "../tests/helpers/manifest_dir.rs"]
mod manifest_dir;

struct Args {
    edits: usize,
    files: usize,
    dir: PathBuf,
    writers: bool,
    reader: bool,
    wal_pages: Option<i64>,
    io_load_mb: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        edits: 300,
        files: 40,
        dir: manifest_dir::manifest_dir()
            .join("target")
            .join("edit-latency-probe"),
        writers: true,
        reader: false,
        wal_pages: None,
        io_load_mb: 0,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--edits" => args.edits = value().parse().expect("--edits"),
            "--files" => args.files = value().parse().expect("--files"),
            "--dir" => args.dir = PathBuf::from(value()),
            "--no-writers" => args.writers = false,
            "--reader" => args.reader = true,
            "--wal-pages" => args.wal_pages = Some(value().parse().expect("--wal-pages")),
            "--io-load" => args.io_load_mb = value().parse().expect("--io-load"),
            other => panic!("unknown option {other}"),
        }
    }
    args
}

/// The crate's own source files, largest last, as the replay corpus.
fn corpus(limit: usize) -> Vec<PathBuf> {
    let src = manifest_dir::manifest_dir().join("src");
    let mut files = std::fs::read_dir(&src)
        .expect("read src")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter_map(|path| Some((std::fs::metadata(&path).ok()?.len(), path)))
        // Keep to ordinary source sizes; the largest modules are outliers.
        .filter(|(len, _)| *len <= 256 * 1024)
        .collect::<Vec<_>>();
    files.sort();
    let step = (files.len() / limit.max(1)).max(1);
    files
        .into_iter()
        .step_by(step)
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

fn open_pool(db_path: &Path, wal_pages: Option<i64>) -> Arc<Mutex<TrackedConnection>> {
    let conn = aft::db::open(db_path).expect("open aft.db");
    if let Some(pages) = wal_pages {
        conn.set_wal_autocheckpoint(pages).expect("wal pages");
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS probe_ledger (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
         CREATE TABLE IF NOT EXISTS probe_tasks (id INTEGER PRIMARY KEY, state TEXT NOT NULL, output BLOB);",
    )
    .expect("probe tables");
    Arc::new(Mutex::new(conn))
}

/// Writers that share the daemon's connection: a call-ledger-like insert of a
/// 2 KiB row every 2 ms and a bash-task-like row update every 10 ms.
fn spawn_shared_writers(
    pool: &Arc<Mutex<TrackedConnection>>,
    stop: &Arc<AtomicBool>,
) -> Vec<std::thread::JoinHandle<()>> {
    let ledger = {
        let pool = Arc::clone(pool);
        let stop = Arc::clone(stop);
        std::thread::spawn(move || {
            let payload = vec![7_u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                if let Ok(conn) = pool.lock() {
                    let _ =
                        conn.execute("INSERT INTO probe_ledger (payload) VALUES (?1)", [&payload]);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let tasks = {
        let pool = Arc::clone(pool);
        let stop = Arc::clone(stop);
        std::thread::spawn(move || {
            let output = vec![b'x'; 8192];
            let mut n = 0_i64;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(conn) = pool.lock() {
                    let _ = conn.execute(
                        "INSERT INTO probe_tasks (id, state, output) VALUES (?1, 'running', ?2)
                         ON CONFLICT(id) DO UPDATE SET state = excluded.state, output = excluded.output",
                        rusqlite::params![n % 64, &output],
                    );
                }
                n += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    vec![ledger, tasks]
}

/// Another process's writer, on its own connection: a 5 ms write transaction
/// every 40 ms, so the shared connection sometimes meets a held write lock.
fn spawn_foreign_writer(db_path: &Path, stop: &Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let db_path = db_path.to_path_buf();
    let stop = Arc::clone(stop);
    std::thread::spawn(move || {
        let conn = aft::db::open(&db_path).expect("open foreign connection");
        let payload = vec![3_u8; 4096];
        while !stop.load(Ordering::Relaxed) {
            if conn.execute_batch("BEGIN IMMEDIATE").is_ok() {
                let _ = conn.execute("INSERT INTO probe_ledger (payload) VALUES (?1)", [&payload]);
                std::thread::sleep(Duration::from_millis(5));
                let _ = conn.execute_batch("COMMIT");
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    })
}

/// A reader that keeps a read transaction open for 500 ms at a time.
fn spawn_reader(db_path: &Path, stop: &Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let db_path = db_path.to_path_buf();
    let stop = Arc::clone(stop);
    std::thread::spawn(move || {
        let conn = aft::db::open(&db_path).expect("open reader connection");
        while !stop.load(Ordering::Relaxed) {
            let _ = conn.execute_batch("BEGIN");
            let _: rusqlite::Result<i64> =
                conn.query_row("SELECT COUNT(*) FROM probe_ledger", [], |row| row.get(0));
            std::thread::sleep(Duration::from_millis(500));
            let _ = conn.execute_batch("COMMIT");
            std::thread::sleep(Duration::from_millis(5));
        }
    })
}

/// Rewrites a file of `megabytes` in 1 MiB writes and syncs it, again and
/// again, so device flushes compete with other writes as during a build.
fn spawn_io_load(
    dir: &Path,
    megabytes: usize,
    stop: &Arc<AtomicBool>,
    written: &Arc<AtomicU64>,
) -> std::thread::JoinHandle<()> {
    let path = dir.join("io-load.bin");
    let stop = Arc::clone(stop);
    let written = Arc::clone(written);
    std::thread::spawn(move || {
        use std::io::Write;
        let chunk = vec![0x5a_u8; 1 << 20];
        while !stop.load(Ordering::Relaxed) {
            let Ok(mut file) = std::fs::File::create(&path) else {
                return;
            };
            for _ in 0..megabytes {
                if stop.load(Ordering::Relaxed) || file.write_all(&chunk).is_err() {
                    break;
                }
                written.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            let _ = file.sync_data();
        }
        let _ = std::fs::remove_file(&path);
    })
}

#[derive(Default)]
struct Samples {
    backup: Vec<Duration>,
    write: Vec<Duration>,
    post_record: Vec<Duration>,
    parts: Vec<PartTimes>,
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn row(name: &str, values: &mut [Duration]) -> String {
    values.sort();
    let ms = |value: Duration| value.as_secs_f64() * 1_000.0;
    format!(
        "| {name} | {:.2} | {:.2} | {:.2} |",
        ms(percentile(values, 0.5)),
        ms(percentile(values, 0.9)),
        ms(values.last().copied().unwrap_or_default())
    )
}

fn main() {
    let args = parse_args();
    let run_dir = args.dir.join(format!("run-{}", std::process::id()));
    let project = run_dir.join("project");
    let storage = run_dir.join("storage");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::create_dir_all(&storage).expect("storage dir");

    let sources = corpus(args.files);
    assert!(!sources.is_empty(), "no corpus files");
    let mut files = Vec::new();
    for source in &sources {
        let target = project.join(source.file_name().expect("file name"));
        std::fs::copy(source, &target).expect("copy corpus file");
        files.push(target);
    }

    let db_path = storage.join("aft.db");
    let pool = open_pool(&db_path, args.wal_pages);
    let store = Arc::new(Mutex::new(BackupStore::new()));
    {
        let mut store = store.lock().unwrap();
        store.set_storage_dir_for_harness(storage.clone(), aft::harness::Harness::Opencode, 72);
        store.set_db_pool(Arc::clone(&pool));
        store.set_db_harness(aft::harness::Harness::Opencode);
        store.set_db_project_key("edit-latency-probe".to_string());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let io_written = Arc::new(AtomicU64::new(0));
    let mut background = Vec::new();
    if args.writers {
        background.extend(spawn_shared_writers(&pool, &stop));
        background.push(spawn_foreign_writer(&db_path, &stop));
    }
    if args.reader {
        background.push(spawn_reader(&db_path, &stop));
    }
    if args.io_load_mb > 0 {
        background.push(spawn_io_load(&run_dir, args.io_load_mb, &stop, &io_written));
    }

    let session = "probe-session";
    let mut samples = Samples::default();
    let replay_started = Instant::now();
    for index in 0..args.edits {
        let path = &files[index % files.len()];
        let before = std::fs::read_to_string(path).expect("read file");
        let mut lines = before.lines().map(str::to_owned).collect::<Vec<_>>();
        let line = (index * 7919) % lines.len().max(1);
        if let Some(text) = lines.get_mut(line) {
            text.push_str(&format!(" // probe {index}"));
        }
        let after = lines.join("\n") + "\n";
        let op_id = format!("op-{index}");

        latency::take();
        let backup_started = Instant::now();
        let wait = Instant::now();
        let mut guard = store.lock().unwrap();
        // The live edit path times this wait as part of the backup phase.
        let store_wait = wait.elapsed();
        let snapshot = guard.snapshot_with_op(session, path, "probe edit", Some(&op_id));
        drop(guard);
        let backup_elapsed = backup_started.elapsed();
        let snapshot_id = snapshot.expect("snapshot");
        assert!(
            snapshot_id.is_some(),
            "snapshot skipped: is {} under a system temp folder? pass --dir",
            path.display()
        );
        let mut parts = latency::take();
        parts.set(Part::StoreLock, store_wait);

        let write_started = Instant::now();
        aft::edit::write_source_for_probe(path, after.as_bytes()).expect("write source");
        let write_elapsed = write_started.elapsed();
        let write_parts = latency::take();
        for part in Part::WRITE {
            parts.set(part, write_parts.get(part));
        }

        // The request finishes by recording what each mutation left, before
        // its response is sent.
        let post_started = Instant::now();
        store.lock().unwrap().record_post_mutation_states();
        samples.post_record.push(post_started.elapsed());
        latency::take();

        samples.backup.push(backup_elapsed);
        samples.write.push(write_elapsed);
        samples.parts.push(parts);
    }
    let replay_elapsed = replay_started.elapsed();
    stop.store(true, Ordering::Relaxed);
    for handle in background {
        let _ = handle.join();
    }

    let ledger_rows: i64 = pool
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM probe_ledger", [], |row| row.get(0))
        .unwrap_or(0);
    let backup_rows: i64 = pool
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM backups", [], |row| row.get(0))
        .unwrap_or(0);

    println!(
        "edits={} files={} writers={} reader={} wal_pages={} io_load_mb={} replay_s={:.1} ledger_rows={} backup_rows={} io_written_mb={}",
        args.edits,
        files.len(),
        args.writers,
        args.reader,
        args.wal_pages.unwrap_or(1000),
        args.io_load_mb,
        replay_elapsed.as_secs_f64(),
        ledger_rows,
        backup_rows,
        io_written.load(Ordering::Relaxed) >> 20,
    );
    println!("| step | p50 ms | p90 ms | max ms |");
    println!("|---|---|---|---|");
    println!("{}", row("backup phase", &mut samples.backup));
    for part in Part::BACKUP {
        let mut values = samples
            .parts
            .iter()
            .map(|parts| parts.get(part))
            .collect::<Vec<_>>();
        println!("{}", row(&format!("  {}", part.name()), &mut values));
    }
    println!("{}", row("write phase", &mut samples.write));
    for part in Part::WRITE {
        let mut values = samples
            .parts
            .iter()
            .map(|parts| parts.get(part))
            .collect::<Vec<_>>();
        println!("{}", row(&format!("  {}", part.name()), &mut values));
    }
    println!("{}", row("post-mutation record", &mut samples.post_record));

    let _ = std::fs::remove_dir_all(&run_dir);
}
