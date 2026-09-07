//! Cross-module contract tests for watcher collection and plane-worker refresh.

use std::io;

use aft::blob_store::{BlobPlane, CallgraphKey};
use aft::path_status::{PathState, PathStatusStore, VISIBLE_PATH_CAP};
use aft::refresh::{
    apply_path_status, execute_plane_batch, preserve_pending_after_unstable_read, FailureClass,
    FailureTracker, PreparedWork, WorkDisposition, WorkerFailure, TRANSIENT_RETRY_LIMIT,
};
use aft::watcher::{read_stable, FileStamp, StableReadError, MAX_STABLE_READ_ATTEMPTS};

fn stamp(size: u64, modified_ns: u128) -> FileStamp {
    FileStamp {
        size,
        modified_ns: Some(modified_ns),
        #[cfg(unix)]
        inode: 1,
        #[cfg(unix)]
        ctime_ns: 1,
    }
}

#[test]
fn dirty_read_stays_pending_without_running_plane_work_and_requeues_on_next_event() {
    let mut stat_calls = 0;
    let mut plane_runs = 0;
    let read = read_stable(
        || {
            stat_calls += 1;
            Ok(if stat_calls % 2 == 0 {
                stamp(2, stat_calls as u128)
            } else {
                stamp(1, stat_calls as u128)
            })
        },
        || Ok::<_, io::Error>(b"unstable source".to_vec()),
    );
    assert!(matches!(
        read,
        Err(StableReadError::Unstable {
            attempts: MAX_STABLE_READ_ATTEMPTS
        })
    ));

    let view = tempfile::tempdir().expect("create view directory");
    let mut statuses = PathStatusStore::open(view.path()).expect("open path-status store");
    preserve_pending_after_unstable_read(&mut statuses, b"src/dirty.rs", 41)
        .expect("record pending path");
    assert_eq!(
        plane_runs, 0,
        "unstable bytes must never reach a plane worker"
    );
    let pending = statuses
        .status_for(b"src/dirty.rs")
        .expect("read pending row")
        .expect("pending row exists");
    assert_eq!(pending.state, PathState::Pending);
    assert_eq!(pending.since_generation, 41);

    let key = CallgraphKey::for_current(b"stable source", "rust").full_key();
    let mut tracker = FailureTracker::default();
    let outcomes = execute_plane_batch(
        &mut tracker,
        "family-a",
        [PreparedWork {
            rel_path: b"src/dirty.rs".to_vec(),
            full_key: key,
            payload: b"parsed".to_vec(),
        }],
        |_, _| {
            plane_runs += 1;
            Ok(())
        },
    )
    .expect("next watcher event can process stable bytes");
    apply_path_status(&mut statuses, &outcomes, 42).expect("clear status after publish");
    assert_eq!(plane_runs, 1);
    assert!(
        statuses
            .status_for(b"src/dirty.rs")
            .expect("read cleared status")
            .is_none(),
        "only the later successful plane publish clears the pending row"
    );
}

#[test]
fn a_full_key_is_processed_once_and_fanned_out_to_every_path() {
    let key = CallgraphKey::for_current(b"byte-identical", "rust").full_key();
    let mut tracker = FailureTracker::default();
    let mut operations = 0;
    let outcomes = execute_plane_batch(
        &mut tracker,
        "family-a",
        [
            PreparedWork {
                rel_path: b"src/second.rs".to_vec(),
                full_key: key.clone(),
                payload: b"extraction".to_vec(),
            },
            PreparedWork {
                rel_path: b"src/first.rs".to_vec(),
                full_key: key,
                payload: b"extraction".to_vec(),
            },
        ],
        |_, _| {
            operations += 1;
            Ok(())
        },
    )
    .expect("deduplicate plane work");

    assert_eq!(operations, 1, "one extraction/put per full key");
    assert_eq!(outcomes[0].disposition, WorkDisposition::Published);
    assert_eq!(
        outcomes[0].rel_paths,
        vec![b"src/first.rs".to_vec(), b"src/second.rs".to_vec()]
    );
}

#[test]
fn retry_and_quarantine_results_drive_per_view_status_counts_and_path_cap() {
    let view = tempfile::tempdir().expect("create view directory");
    let mut statuses = PathStatusStore::open(view.path()).expect("open path-status store");
    let key = CallgraphKey::for_current(b"transient", "rust").full_key();
    let mut tracker = FailureTracker::default();

    for _ in 0..TRANSIENT_RETRY_LIMIT {
        let outcomes = execute_plane_batch(
            &mut tracker,
            "family-a",
            [PreparedWork {
                rel_path: b"src/retrying.rs".to_vec(),
                full_key: key.clone(),
                payload: Vec::new(),
            }],
            |_, _| Err(WorkerFailure::transient("timeout")),
        )
        .expect("execute retrying batch");
        apply_path_status(&mut statuses, &outcomes, 6).expect("mark retry pending");
    }
    assert_eq!(
        tracker.record_failure("family-a", &key, FailureClass::Transient),
        WorkDisposition::BreakerRecorded { failures: 1 }
    );

    for index in 0..VISIBLE_PATH_CAP + 3 {
        statuses
            .mark_failed(
                format!("src/failure-{index:02}.rs").as_bytes(),
                "quarantined",
                7,
            )
            .expect("mark failed");
    }
    let summary = statuses.summary().expect("summarize path statuses");
    assert_eq!(summary.pending_count, 1);
    assert_eq!(summary.failed_count, VISIBLE_PATH_CAP + 3);
    assert_eq!(summary.paths.len(), VISIBLE_PATH_CAP);
    assert!(summary
        .paths
        .windows(2)
        .all(|paths| paths[0].rel_path <= paths[1].rel_path));
    assert_eq!(
        tracker.breaker_failures("family-a", BlobPlane::Callgraph),
        1
    );
}
