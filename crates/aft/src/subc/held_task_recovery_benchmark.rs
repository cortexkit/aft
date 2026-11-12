//! Opt-in measurements on an offline snapshot. Commands are selected from the
//! database, never restored into a running registry or attached to processes.

use super::*;
use crate::subc::consent;

// The selection function from the pre-pagination implementation is retained here
// so one binary can compare both lock scopes against the same offline database.
fn baseline_sessions(db: &crate::db::TrackedConnection) -> rusqlite::Result<Vec<Session>> {
    let mut stmt = db.prepare(
        "SELECT t.harness,t.session_id,
                COALESCE(json_extract(t.metadata,'$.project_root'),t.cwd),
                t.task_id,t.metadata,
                EXISTS(SELECT 1 FROM call_ledger c
                       WHERE c.task_id=t.task_id AND c.recovered=1 AND c.state='DispatchStarted'
                         AND json_extract(t.metadata,'$.call_key.requester')=c.carrier
                         AND json_extract(t.metadata,'$.call_key.key')=c.call_key) AS keyed
         FROM bash_tasks t
         WHERE keyed OR t.status NOT IN ('completed','failed','killed','timed_out','fate_unknown')
                     OR t.completion_delivered=0
         ORDER BY 1,2,3,t.task_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    let mut pending = std::collections::BTreeMap::<_, Vec<String>>::new();
    for row in rows {
        let (harness, session, root, task, metadata, keyed) = row?;
        let metadata: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        let runner = [
            metadata["launching_identity"]["principal"].as_str(),
            metadata["call_key"]["requester"].as_str(),
        ]
        .into_iter()
        .any(|carrier| {
            matches!(
                carrier,
                Some("reserved:thalamus-harness" | "reserved:broca")
            )
        }) || [
            Some(harness.as_str()),
            metadata["launching_raw_harness"].as_str(),
            metadata["launching_identity"]["harness"].as_str(),
        ]
        .into_iter()
        .flatten()
        .any(|raw| raw.parse::<crate::harness::Harness>() == Ok(crate::harness::Harness::Runner));
        if keyed || runner {
            pending
                .entry((harness, session, root))
                .or_default()
                .push(task);
        }
    }
    Ok(pending
        .into_iter()
        .map(|((harness, session_id, root), task_ids)| Session {
            harness,
            session_id,
            project_root: root.into(),
            task_ids,
        })
        .collect())
}

#[derive(Clone, Copy, Debug)]
struct Measurement {
    total: Duration,
    max_hold: Duration,
    holds: usize,
    tick_delay: Duration,
}

struct NoCingulate;
impl consent::ConsentTransport for NoCingulate {
    fn call(&self, _: consent::ConsentCall) -> consent::ConsentFuture {
        panic!("the offline benchmark must never call Cingulate")
    }
}

// Only the synchronous SELECT/DELETE sections are modelled for the old cleanup.
// The fixed case below calls the production async cleanup with an unpublished
// card, which exercises both database sections without making a transport call.
fn baseline_cleanup(db: &Arc<StdMutex<crate::db::TrackedConnection>>) {
    let rows = {
        let conn = db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT folder,program,selector,project_root,session,elicitation_id FROM macos_permission_cards").unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    };
    for (folder, program, _, _, _, id) in rows {
        assert!(id.is_none());
        db.lock()
            .unwrap()
            .execute(
                "DELETE FROM macos_permission_cards WHERE folder=?1 AND program=?2",
                rusqlite::params![folder, program],
            )
            .unwrap();
    }
}

fn measure(
    db: &Arc<StdMutex<crate::db::TrackedConnection>>,
    fixed: bool,
) -> (Vec<Session>, Measurement) {
    let mut max_hold = Duration::ZERO;
    let mut holds = 0;
    let mut observe = |_: usize, held: Duration| {
        holds += 1;
        max_hold = max_hold.max(held);
    };
    // The shared wrapper measures from acquisition until immediately before
    // unlock, including query iteration; time waiting to acquire is excluded.
    let begin = Instant::now();
    RecoveryReader {
        db,
        on_chunk: &mut observe,
    }
    .read(|conn| {
        crate::db::call_ledger::recover(conn)?;
        Ok(Vec::<()>::new())
    })
    .unwrap();
    db.lock().unwrap().execute("INSERT INTO macos_permission_cards(folder,program,call_key,selector,project_root,session,elicitation_id) VALUES ('offline-benchmark','program','','unused','unused','unused',NULL)", []).unwrap();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let runtime_db = Arc::clone(db);
    let runtime = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                start_rx.await.unwrap();
                let mut next = Instant::now() + Duration::from_millis(10);
                let cleanup = tokio::spawn(async move {
                    if fixed {
                        consent::withdraw_macos_permission_cards(runtime_db)
                            .await
                            .unwrap();
                    } else {
                        baseline_cleanup(&runtime_db);
                    }
                });
                let mut finish = finish_rx;
                let mut worst = Duration::ZERO;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(next.into()) => {
                            let now = Instant::now();
                            worst = worst.max(now.saturating_duration_since(next));
                            next = now + Duration::from_millis(10);
                        }
                        _ = &mut finish => {
                            worst = worst.max(Instant::now().saturating_duration_since(next));
                            cleanup.await.unwrap();
                            return worst.max(Instant::now().saturating_duration_since(next));
                        }
                    }
                }
            })
    });
    FIRST_RECOVERY_LOCK.with(|probe| {
        *probe.borrow_mut() = Some(Box::new(move || {
            let _ = start_tx.send(());
        }))
    });
    let selected = if fixed {
        sessions(db, &mut observe).unwrap()
    } else {
        RecoveryReader {
            db,
            on_chunk: &mut observe,
        }
        .read(baseline_sessions)
        .unwrap()
    };
    let total = begin.elapsed();
    finish_tx.send(()).unwrap();
    let tick_delay = runtime.join().unwrap();
    (
        selected,
        Measurement {
            total,
            max_hold,
            holds,
            tick_delay,
        },
    )
}

#[test]
#[ignore = "requires AFT_RECOVERY_BENCH_DB naming an offline snapshot copy under target/scratch"]
fn startup_recovery_snapshot_benchmark() {
    let Some(path) = std::env::var_os("AFT_RECOVERY_BENCH_DB") else {
        return;
    };
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .canonicalize()
        .unwrap();
    let path = PathBuf::from(path).canonicalize().unwrap();
    assert!(
        path.starts_with(&scratch),
        "only scratch snapshot copies are allowed"
    );
    assert!(std::env::var_os("AFT_STORAGE_DIR").is_none());
    let db = Arc::new(StdMutex::new(crate::db::open(&path).unwrap()));
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM macos_permission_cards", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        0,
        "snapshot must not contain published cards"
    );
    consent::install_test_transport(Some(Arc::new(NoCingulate)));
    let expected = baseline_sessions(&db.lock().unwrap()).unwrap();
    println!(
        "warmup selected {} sessions / {} tasks",
        expected.len(),
        expected.iter().map(|s| s.task_ids.len()).sum::<usize>()
    );
    for fixed in [false, true] {
        let mut runs = Vec::new();
        for run in 1..=3 {
            let (selected, measured) = measure(&db, fixed);
            assert_eq!(selected, expected);
            println!(
                "{} run={run} total_ms={:.3} max_hold_ms={:.3} holds={} worst_tick_delay_ms={:.3}",
                if fixed { "fixed" } else { "base" },
                measured.total.as_secs_f64() * 1000.0,
                measured.max_hold.as_secs_f64() * 1000.0,
                measured.holds,
                measured.tick_delay.as_secs_f64() * 1000.0
            );
            runs.push(measured);
        }
        let mut totals = runs.iter().map(|r| r.total).collect::<Vec<_>>();
        totals.sort();
        let mut holds = runs.iter().map(|r| r.max_hold).collect::<Vec<_>>();
        holds.sort();
        let mut delays = runs.iter().map(|r| r.tick_delay).collect::<Vec<_>>();
        delays.sort();
        println!("{} median/worst: total_ms={:.3}/{:.3} max_hold_ms={:.3}/{:.3} tick_delay_ms={:.3}/{:.3}",
            if fixed { "fixed" } else { "base" }, totals[1].as_secs_f64()*1000.0, totals[2].as_secs_f64()*1000.0,
            holds[1].as_secs_f64()*1000.0, holds[2].as_secs_f64()*1000.0,
            delays[1].as_secs_f64()*1000.0, delays[2].as_secs_f64()*1000.0);
    }
    consent::install_test_transport(None);
}
