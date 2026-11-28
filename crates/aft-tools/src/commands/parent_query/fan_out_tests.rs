use std::process::Command;
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;

use super::fan_out_children;

#[test]
fn concurrent_parent_searches_do_not_deadlock_during_nested_rayon_join() {
    const CHILD_PROCESS: &str = "AFT_TEST_PARENT_FAN_OUT_CHILD";
    if std::env::var_os(CHILD_PROCESS).is_none() {
        // Isolate the two-worker global pool from all other tests. The child
        // process also owns any threads left deadlocked if the test fails.
        let (_, module) = module_path!().split_once("::").unwrap();
        let test = format!(
            "{module}::concurrent_parent_searches_do_not_deadlock_during_nested_rayon_join"
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD_PROCESS, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "isolated fan-out test failed:\n{stdout}{stderr}"
        );
        return;
    }

    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .unwrap();
    let engines = Arc::new([Mutex::new(()), Mutex::new(())]);
    let (right_started, wait_for_right) = bounded(1);
    let (start_second_search, wait_for_join) = bounded(1);
    let (contending, wait_for_contention) = bounded(1);
    let (other_child_done, wait_for_other_child) = bounded(1);
    let (done, finished) = bounded(2);
    let deadline = Duration::from_secs(5);

    let first_engines = Arc::clone(&engines);
    let first_done = done.clone();
    thread::spawn(move || {
        let rows = fan_out_children(&*first_engines, &[0, 1], |engine, &index| {
            let _guard = engine.lock().unwrap();
            if index == 0 {
                rayon::join_context(
                    |_| wait_for_right.recv().unwrap(),
                    |context| {
                        assert!(context.migrated(), "the other worker must steal the join");
                        right_started.send(()).unwrap();
                        start_second_search.send(()).unwrap();
                        wait_for_other_child.recv().unwrap();
                    },
                );
            }
            index
        });
        first_done.send(rows).unwrap();
    });
    wait_for_join
        .recv_timeout(deadline)
        .expect("the first search did not enter a stolen Rayon join");

    thread::spawn(move || {
        let rows = fan_out_children(&*engines, &[0, 1], |engine, &index| {
            if index == 0 {
                assert!(matches!(engine.try_lock(), Err(TryLockError::WouldBlock)));
                contending.send(()).unwrap();
            }
            let _guard = engine.lock().unwrap();
            if index == 1 {
                wait_for_contention.recv().unwrap();
                other_child_done.send(()).unwrap();
            }
            index
        });
        done.send(rows).unwrap();
    });

    // With Rayon fan-out, the first child's worker steals the second search
    // while waiting in the join and tries to reacquire its own engine lock.
    // The other worker cannot help: it is waiting for the other child.
    for _ in 0..2 {
        let rows = finished
            .recv_timeout(deadline)
            .expect("parent searches deadlocked while waiting for a nested Rayon join");
        assert_eq!(rows, [0, 1]);
    }
}
