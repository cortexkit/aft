use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection};

use super::*;

const HOUR_MS: i64 = 60 * 60 * 1000;
/// A fixed wall-clock "now" far enough from zero for every age below.
const NOW_MS: i64 = 400 * 24 * HOUR_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

fn shared(dir: &Path) -> Arc<Mutex<TrackedConnection>> {
    Arc::new(Mutex::new(crate::db::open(&dir.join("aft.db")).unwrap()))
}

/// Insert an authority row directly. The sweep never decodes `record_json`, so
/// the payload is filler of the requested size.
fn insert(conn: &Connection, incarnation: &str, phase: &str, revoked_at_ms: Option<i64>) {
    insert_sized(conn, incarnation, phase, revoked_at_ms, 1024);
}

fn insert_sized(
    conn: &Connection,
    incarnation: &str,
    phase: &str,
    revoked_at_ms: Option<i64>,
    payload_bytes: usize,
) {
    conn.execute(
        "INSERT INTO gh_speech_authorities(incarnation,minting_run,harness,session_id,task_id,record_json,record_digest,phase)
         VALUES(?1,'run','test','session',?1,?2,?3,?4)",
        params![incarnation, "x".repeat(payload_bytes), [0_u8; 32].as_slice(), phase],
    )
    .unwrap();
    if let Some(at) = revoked_at_ms {
        conn.execute(
            "INSERT INTO gh_speech_task_revocations(incarnation,reason,recorded_at_ms) VALUES(?1,'terminal',?2)",
            params![incarnation, at],
        )
        .unwrap();
    }
}

fn authorities(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT incarnation FROM gh_speech_authorities ORDER BY incarnation")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn revocations(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM gh_speech_task_revocations",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn sweep_deletes_only_revoked_rows_revoked_a_day_ago_and_keeps_every_live_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = shared(dir.path());
    {
        let conn = db.lock().unwrap();
        insert(&conn, "old-1", "revoked", Some(NOW_MS - DAY_MS - HOUR_MS));
        // Exactly a day old counts as old.
        insert(&conn, "old-2", "revoked", Some(NOW_MS - DAY_MS));
        insert(&conn, "old-3", "revoked", Some(NOW_MS - 30 * DAY_MS));
        insert(&conn, "young", "revoked", Some(NOW_MS - DAY_MS + 1));
        insert(&conn, "active", "active", None);
        insert(&conn, "candidate", "candidate", None);
        // Revocation always marks its row revoked in the same transaction, so a
        // live row with an old revocation should not exist. If one does, the
        // phase check keeps it, and the cursor moves past it.
        insert(
            &conn,
            "active-revocation-recorded",
            "active",
            Some(NOW_MS - 2 * DAY_MS),
        );
        insert(
            &conn,
            "candidate-revocation-recorded",
            "candidate",
            Some(NOW_MS - 2 * DAY_MS),
        );
        // A write queued by an older build: the relay's replay check reads it.
        insert(&conn, "legacy-queued", "revoked", Some(NOW_MS - 2 * DAY_MS));
        conn.execute(
            "INSERT INTO gh_speech_legacy_dispatch(request_nonce,incarnation,forwarding_run,state,enqueued_at_ms)
             VALUES('nonce','legacy-queued','run','queued',1)",
            [],
        )
        .unwrap();
    }

    let mut cursor = String::new();
    // Two rows per batch: the first batch holds only the two live rows above,
    // so the sweep reaches the old rows only if the cursor advances.
    let sweep = sweep(&db, &mut cursor, NOW_MS, Duration::from_secs(60), 2).unwrap();

    assert_eq!(
        (
            sweep.batches,
            sweep.examined,
            sweep.deleted,
            sweep.exhausted
        ),
        (3, 5, 3, true),
        "{sweep:?}"
    );
    assert_eq!(sweep.stopped, None);
    assert!(
        cursor.is_empty(),
        "an exhausted sweep starts over next time"
    );
    let conn = db.lock().unwrap();
    assert_eq!(
        authorities(&conn),
        [
            "active",
            "active-revocation-recorded",
            "candidate",
            "candidate-revocation-recorded",
            "legacy-queued",
            "young",
        ]
    );
    // Revocations stay, so a deleted incarnation is still refused as revoked.
    assert_eq!(revocations(&conn), 7);
}

#[test]
fn batch_deletes_at_most_its_limit_and_resumes_after_its_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = shared(dir.path());
    let mut conn = db.lock().unwrap();
    for index in 0..5 {
        insert(
            &conn,
            &format!("r{index}"),
            "revoked",
            Some(NOW_MS - 2 * DAY_MS),
        );
    }
    let cutoff = NOW_MS - DAY_MS;
    let batch = |conn: &mut TrackedConnection, after: &str| {
        delete_revoked_batch(conn, after, cutoff, 2).unwrap()
    };
    let first = batch(&mut conn, "");
    assert_eq!(
        first,
        Batch {
            examined: 2,
            deleted: 2,
            last: Some("r1".into())
        }
    );
    assert_eq!(authorities(&conn), ["r2", "r3", "r4"]);
    let second = batch(&mut conn, "r1");
    assert_eq!((second.examined, second.deleted), (2, 2));
    assert_eq!(second.last.as_deref(), Some("r3"));
    let third = batch(&mut conn, "r3");
    assert_eq!(
        third,
        Batch {
            examined: 1,
            deleted: 1,
            last: Some("r4".into())
        }
    );
    assert_eq!(batch(&mut conn, "r4"), Batch::default());
    assert!(authorities(&conn).is_empty());
}

#[test]
fn sweep_stops_at_its_budget_or_a_held_connection_and_restarts_a_stale_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = shared(dir.path());
    insert(
        &db.lock().unwrap(),
        "old",
        "revoked",
        Some(NOW_MS - 2 * DAY_MS),
    );
    let mut cursor = String::new();

    let spent = sweep(&db, &mut cursor, NOW_MS, Duration::ZERO, 2).unwrap();
    assert_eq!((spent.batches, spent.stopped), (0, Some("time_budget")));

    let held = db.lock().unwrap();
    let busy = sweep(&db, &mut cursor, NOW_MS, Duration::from_secs(60), 2).unwrap();
    assert_eq!((busy.batches, busy.stopped), (0, Some("connection_busy")));
    drop(held);

    // A cursor past every row (the rows it pointed at are gone) finds nothing,
    // wraps, and the next sweep starts from the beginning.
    cursor = "zzz".into();
    let wrapped = sweep(&db, &mut cursor, NOW_MS, Duration::from_secs(60), 2).unwrap();
    assert_eq!((wrapped.deleted, wrapped.exhausted), (0, true));
    assert!(cursor.is_empty());
    let again = sweep(&db, &mut cursor, NOW_MS, Duration::from_secs(60), 2).unwrap();
    assert_eq!(again.deleted, 1);
}

/// Every table access in both statements is an index search: the authority
/// side is read only through its primary-key index, never through the rows,
/// whose overflow pages hold tens of kilobytes each. The only scan allowed is
/// of the selected candidates themselves.
#[test]
fn candidate_selection_never_scans_authority_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
    for sql in [COUNT_CANDIDATES, DELETE_CANDIDATES] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params!["", 0, 64], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let joined = plan.join("\n");
        assert!(
            plan.iter()
                .all(|step| !step.starts_with("SCAN ") || step.starts_with("SCAN (subquery-")),
            "{joined}"
        );
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH a USING COVERING INDEX ")),
            "{joined}"
        );
    }
}

fn page_counts(conn: &Connection) -> (i64, i64) {
    (
        conn.pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap(),
        conn.pragma_query_value(None, "freelist_count", |row| row.get(0))
            .unwrap(),
    )
}

#[test]
fn freed_pages_are_vacuumed_only_when_the_file_uses_incremental_auto_vacuum() {
    let dir = tempfile::tempdir().unwrap();
    // AFT databases are created with SQLite's default, auto_vacuum NONE.
    let default_db = shared(dir.path());
    let incremental_dir = dir.path().join("incremental");
    std::fs::create_dir(&incremental_dir).unwrap();
    {
        // The mode is fixed when the first table is created.
        let raw = Connection::open(incremental_dir.join("aft.db")).unwrap();
        raw.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; CREATE TABLE first(x);")
            .unwrap();
    }
    let incremental_db = shared(&incremental_dir);

    for (db, mode) in [(&default_db, 0), (&incremental_db, 2)] {
        {
            let conn = db.lock().unwrap();
            let actual: i64 = conn
                .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
                .unwrap();
            assert_eq!(actual, mode);
            for index in 0..8 {
                insert_sized(
                    &conn,
                    &format!("big-{index}"),
                    "revoked",
                    Some(NOW_MS - 2 * DAY_MS),
                    64 * 1024,
                );
            }
        }
        let before = page_counts(&db.lock().unwrap());
        let swept = sweep(db, &mut String::new(), NOW_MS, Duration::from_secs(60), 64).unwrap();
        assert_eq!(swept.deleted, 8);
        let (pages, free) = page_counts(&db.lock().unwrap());
        if mode == 2 {
            let released = swept.vacuum_pages.expect("incremental vacuum ran");
            assert!(released >= 8 * 16, "{released} pages released");
            assert_eq!(free, 0);
            assert!(pages < before.0, "{pages} < {}", before.0);
        } else {
            assert_eq!(swept.vacuum_pages, None);
            // Deleted rows leave free pages for reuse; the file does not shrink.
            assert!(free >= 8 * 16, "{free} free pages");
            assert_eq!(pages, before.0);
        }
    }
}

/// Measures deletion on a large synthetic table. Not part of the suite: it
/// writes `rows` × 84 KiB (8.4 GB for the default 100,000 rows).
///
/// `AFT_SPEECH_RETENTION_BENCH_DIR` names the directory for the database
/// (required); `AFT_SPEECH_RETENTION_BENCH_ROWS` the revoked rows (default
/// 100000); `AFT_SPEECH_RETENTION_BENCH_LIVE` extra active rows kept (default 0);
/// `AFT_SPEECH_RETENTION_BENCH_VACUUM=1` times a full VACUUM afterwards.
#[test]
#[ignore = "benchmark that writes gigabytes; see its doc comment"]
fn bench_revoked_retention_on_large_rows() {
    let env = |name: &str| std::env::var(name).ok();
    let dir = std::path::PathBuf::from(env("AFT_SPEECH_RETENTION_BENCH_DIR").expect("bench dir"));
    let rows: usize =
        env("AFT_SPEECH_RETENTION_BENCH_ROWS").map_or(100_000, |v| v.parse().unwrap());
    let live: usize = env("AFT_SPEECH_RETENTION_BENCH_LIVE").map_or(0, |v| v.parse().unwrap());
    let path = dir.join("aft.db");
    let payload = "7,".repeat(84 * 1024 / 2);
    let build = std::time::Instant::now();
    {
        let mut conn = crate::db::open(&path).unwrap();
        let mut written = 0;
        while written < rows + live {
            let tx = conn.transaction().unwrap();
            for index in written..(written + 1000).min(rows + live) {
                let incarnation = format!("{:064x}", index as u128 * 0x9E37_79B9_7F4A_7C15);
                let (phase, revoked) = if index < rows {
                    ("revoked", Some(NOW_MS - 2 * DAY_MS))
                } else {
                    ("active", None)
                };
                tx.execute(
                    "INSERT INTO gh_speech_authorities(incarnation,minting_run,harness,session_id,task_id,record_json,record_digest,phase)
                     VALUES(?1,'run','test','session',?1,?2,?3,?4)",
                    params![incarnation, payload, [0_u8; 32].as_slice(), phase],
                )
                .unwrap();
                if let Some(at) = revoked {
                    tx.execute(
                        "INSERT INTO gh_speech_task_revocations(incarnation,reason,recorded_at_ms) VALUES(?1,'terminal',?2)",
                        params![incarnation, at],
                    )
                    .unwrap();
                }
            }
            tx.commit().unwrap();
            written = (written + 1000).min(rows + live);
        }
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    }
    let file_bytes = || std::fs::metadata(&path).unwrap().len();
    let wal_bytes = || std::fs::metadata(dir.join("aft.db-wal")).map_or(0, |m| m.len());
    eprintln!(
        "built rows={rows} live={live} record_bytes={} file_bytes={} in {:?}",
        payload.len(),
        file_bytes(),
        build.elapsed()
    );
    let db = shared(&dir);
    {
        let conn = db.lock().unwrap();
        let pragma = |name: &str| -> i64 {
            conn.pragma_query_value(None, name, |row| row.get(0))
                .unwrap()
        };
        eprintln!(
            "page_size={} auto_vacuum={} secure_delete={} page_count={} freelist_count={}",
            pragma("page_size"),
            pragma("auto_vacuum"),
            pragma("secure_delete"),
            pragma("page_count"),
            pragma("freelist_count"),
        );
    }
    let mut cursor = String::new();
    let drain = std::time::Instant::now();
    let (mut ticks, mut deleted, mut worst_tick, mut busiest_tick) = (0, 0, Duration::ZERO, 0);
    let mut worst_batch_average = Duration::ZERO;
    loop {
        let tick = sweep(&db, &mut cursor, NOW_MS, TICK_BUDGET, BATCH_ROWS).unwrap();
        ticks += 1;
        deleted += tick.deleted;
        worst_tick = worst_tick.max(tick.elapsed);
        busiest_tick = busiest_tick.max(tick.deleted);
        if tick.batches > 0 {
            worst_batch_average = worst_batch_average.max(tick.elapsed / tick.batches as u32);
        }
        if tick.exhausted && tick.deleted == 0 {
            break;
        }
    }
    let elapsed = drain.elapsed();
    eprintln!(
        "drained deleted={deleted} ticks={ticks} elapsed={elapsed:?} rows_per_s={:.0} rows_per_tick_max={busiest_tick} rows_per_tick_mean={:.0} worst_tick={worst_tick:?} worst_batch_mean={worst_batch_average:?} file_bytes={} wal_bytes={}",
        deleted as f64 / elapsed.as_secs_f64(),
        deleted as f64 / ticks as f64,
        file_bytes(),
        wal_bytes()
    );
    assert_eq!(deleted, rows);
    {
        let conn = db.lock().unwrap();
        let (pages, free) = page_counts(&conn);
        eprintln!("after drain page_count={pages} freelist_count={free}");
        if env("AFT_SPEECH_RETENTION_BENCH_VACUUM").as_deref() == Some("1") {
            let vacuum = std::time::Instant::now();
            conn.execute_batch("VACUUM").unwrap();
            let vacuumed = vacuum.elapsed();
            let checkpoint = std::time::Instant::now();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            eprintln!(
                "vacuum elapsed={vacuumed:?} checkpoint elapsed={:?} file_bytes={}",
                checkpoint.elapsed(),
                file_bytes()
            );
        }
    }
}
