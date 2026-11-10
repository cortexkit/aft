//! Offline write measurements. AFT_WRITE_PROBE_DIR must contain SQLite-backup
//! snapshots named aft-snapshot.sqlite and callgraph.sqlite, never live stores.
//! Run this ignored test alone; process counters include concurrent threads.
//! The production-refresh probe edits its disposable prefrontal-root clone;
//! AFT_WRITE_PROBE_DELETE=1 deletes the two selected source files in that clone.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::Instant;

fn measure<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let before = aft::process_io::Bytes::capture().unwrap();
    let started = Instant::now();
    let result = f();
    let delta = aft::process_io::Bytes::capture()
        .unwrap()
        .delta(before)
        .unwrap();
    println!(
        "{label}: seconds={:.3} physical={} logical={}",
        started.elapsed().as_secs_f64(),
        delta.written,
        delta.logical
    );
    result
}

#[test]
#[ignore = "requires offline database snapshots; measures physical writes"]
fn offline_database_read_writes() {
    let dir = PathBuf::from(std::env::var_os("AFT_WRITE_PROBE_DIR").unwrap());
    let db = rusqlite::Connection::open_with_flags(
        dir.join("aft-snapshot.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let session: (String, String) = db.query_row("SELECT harness,session_id FROM bash_tasks GROUP BY harness,session_id ORDER BY count(*) DESC LIMIT 1", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    for _ in 0..2 {
        measure("bash-session", || {
            aft::db::bash_tasks::list_bash_tasks_for_session(&db, &session.0, &session.1).unwrap()
        });
    }
}
