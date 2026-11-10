//! Offline production-artifact probes. Inputs are copied before any mutation.
//! Run with AFT_DISK_HUNT_INPUT pointing at aft.db, semantic.bin, derived.sqlite,
//! callgraph.sqlite and manifest.json captured together outside the daemon.

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

fn measure<T>(name: &str, operation: impl FnOnce() -> T) -> T {
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

#[test]
#[ignore = "offline query-plan inventory on production artifact copies"]
fn bench_disk_write_query_plans() {
    let input =
        PathBuf::from(std::env::var_os("AFT_DISK_HUNT_INPUT").expect("set input directory"));
    for file in [
        "aft.db",
        "inspect.sqlite",
        "derived.sqlite",
        "callgraph.sqlite",
    ] {
        let conn = Connection::open_with_flags(input.join(file), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        eprintln!("hunt {file} SQLite {}", rusqlite::version());
        for pragma in [
            "journal_mode",
            "synchronous",
            "wal_autocheckpoint",
            "page_size",
            "cache_size",
            "temp_store",
            "mmap_size",
        ] {
            let value = conn
                .query_row(&format!("PRAGMA {pragma}"), [], |r| {
                    Ok(format!("{:?}", r.get_ref(0)?))
                })
                .unwrap();
            eprintln!("hunt pragma {pragma}={value}");
        }
    }
    let conn = Connection::open_with_flags(input.join("aft.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let (harness, session, project): (String, String, String) = conn.query_row(
        "SELECT harness, session_id, project_key FROM bash_tasks GROUP BY harness, session_id ORDER BY count(*) DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    let rows = measure("session-tasks-production", || {
        crate::db::bash_tasks::list_bash_tasks_for_session(&conn, &harness, &session).unwrap()
    });
    eprintln!("hunt production session rows={}", rows.len());
    drop(rows);
    for (name, sql, parameters) in [
        ("session-tasks", "SELECT * FROM bash_tasks WHERE harness = ?1 AND session_id = ?2 ORDER BY started_at, task_id", vec![harness.clone(), session.clone()]),
        ("project-replay", "SELECT * FROM bash_tasks WHERE harness = ?1 AND project_key = ?2 AND (status NOT IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown') OR completion_delivered = 0) ORDER BY started_at, task_id", vec![harness.clone(), project]),
        ("task-id", "SELECT * FROM bash_tasks WHERE harness = ?1 AND task_id = ?2 ORDER BY started_at DESC", vec![harness.clone(), "missing".into()]),
        ("session-backups", "SELECT * FROM backups WHERE harness = ?1 AND session_id = ?2 ORDER BY file_path, order_blob", vec![harness, session]),
        ("backup-sessions", "SELECT harness, session_id FROM backups GROUP BY harness, session_id", vec![]),
    ] {
        let plans = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap().query_map(rusqlite::params_from_iter(parameters.iter()), |r| r.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let count = measure(name, || {
            let mut stmt = conn.prepare(sql).unwrap();
            let mut rows = stmt.query(rusqlite::params_from_iter(parameters.iter())).unwrap();
            let mut count = 0;
            while rows.next().unwrap().is_some() { count += 1; }
            count
        });
        eprintln!("hunt plan {name}: rows={count} {plans:?}");
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    measure("removal-health", || {
        crate::db::removal::removal_health_from_connection(&conn, now).unwrap()
    });
}
