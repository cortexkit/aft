//! Opt-in release-profile replay; no wall-clock sleeps or daemon required.

use super::*;
use crate::inspect::tier2_scheduler::{Tier2RefreshScheduler, TIER2_REFRESH_STORM_PATH_THRESHOLD};
use std::time::Duration;

fn thread_cpu() -> Duration {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // The kernel initializes both fields on success; thread CPU excludes the
    // index workers and filesystem waits outside the counting thread.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) },
        0
    );
    let time = unsafe { time.assume_init() };
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

fn timed<T>(run: impl FnOnce() -> T) -> (T, f64, f64) {
    let wall = Instant::now();
    let cpu = thread_cpu();
    let value = run();
    (
        value,
        (thread_cpu() - cpu).as_secs_f64() * 1000.0,
        wall.elapsed().as_secs_f64() * 1000.0,
    )
}

#[test]
#[ignore = "release-profile TODO replay, run explicitly"]
fn todo_count_release_replay() {
    const FILES: usize = 7_400;
    const SECONDS: u64 = 600;
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&root)
        .status()
        .unwrap()
        .success());
    let mut scope = Vec::new();
    for index in 0..FILES {
        let path = root.join(format!("src/group_{:02}/file_{index:05}.rs", index / 100));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut source = format!("// Module {index}\n");
        if index % 20 == 0 {
            source.push_str(&format!("// TODO(owner): work item {index}\n"));
        }
        for item in 0..40 {
            source.push_str(&format!(
                "pub const VALUE_{item}: usize = {};\n",
                index + item
            ));
        }
        std::fs::write(&path, source).unwrap();
        scope.push(path);
    }
    eprintln!("TODO_REPLAY fixture files={FILES}; building stored records");
    let ctx = crate::data_layer::tools::inspect::bind_published_for_test(&root);
    let checkout = ctx.data_checkout().unwrap();
    let job = super::tests::todos_job(&root, scope.clone());
    let relative = scope
        .iter()
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/")
                .into_bytes()
        })
        .collect::<Vec<_>>();
    let detail_paths = relative
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 20 == 0)
        .take(MAX_ITEMS)
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    let limit = crate::inspect::scanners::source_text::MAX_SOURCE_BYTES;
    let cold = timed(|| run_todos_scan(&job).outcome.unwrap().aggregate);
    assert_eq!(cold.0, uncached_aggregate_for_test(&job));
    eprintln!(
        "TODO_REPLAY cold cached_cpu_ms={:.3} cached_wall_ms={:.3}",
        cold.1, cold.2
    );

    for round in 0..3 {
        let start = Instant::now();
        let mut scheduler = Tier2RefreshScheduler::new();
        scheduler.reset_after_configure(start - Duration::from_secs(120));
        scheduler.request_pull(true);
        let mut old_cpu = 0.0;
        let mut new_cpu = 0.0;
        let mut scheduled = 0;
        let mut explicit = 0;
        let mut edits = 0;
        let mut counts = 0;
        let mut old_decodes = 0;
        let mut new_decodes = 0;
        for second in 0..SECONDS {
            let mut changed = 0;
            // Ten saves/minute, then a quiet window; three explicit inspect
            // calls/minute. This intentionally exercises fresh inspect jobs,
            // rather than timing status-bar reads of the cached TODO count.
            if second % 60 < 20 && second % 2 == 0 {
                let index = (round * 101 + edits * 53) % FILES;
                std::fs::write(&scope[index], format!("// FIXME: replay {round} save {edits}\npub const VALUE: usize = {edits};\n")).unwrap();
                checkout
                    .checkout()
                    .record_paths(&[relative[index].clone()])
                    .unwrap();
                // Count record work, not extraction: the real refresh worker
                // also extracts edits independently of the counting thread.
                checkout.checkout().refresh_callgraph().unwrap();
                edits += 1;
                changed = 1;
            }
            if second % 60 == 25 {
                checkout.checkout().publish_changes().unwrap();
            }
            if second == 300 {
                checkout.checkout().rescan().unwrap();
                // A rescan makes the index store temporarily unreadable to
                // TODO queries; becoming readable again requests a refresh.
                scheduler.request_pull(true);
                // The watcher feeds a rescan as more than the storm threshold,
                // rather than counting every discovered path as a new edit.
                changed = TIER2_REFRESH_STORM_PATH_THRESHOLD + 1;
            }
            let automatic = scheduler
                .tick(start + Duration::from_secs(second), changed, true, false)
                .is_some();
            let inspect = matches!(second % 60, 20 | 40 | 55);
            if automatic {
                scheduled += 1;
            }
            if inspect {
                explicit += 1;
            }
            for _ in 0..usize::from(automatic) + usize::from(inspect) {
                let mut old = None;
                let mut new = None;
                for cached in if (round + counts) % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    let before =
                        crate::data_layer::checkout::source_work_for_test(&root).todo_decodes;
                    let result = timed(|| {
                        if cached {
                            std::hint::black_box(run_todos_scan(&job))
                                .outcome
                                .unwrap()
                                .aggregate
                        } else {
                            uncached_aggregate_for_test(&job)
                        }
                    });
                    let decoded = crate::data_layer::checkout::source_work_for_test(&root)
                        .todo_decodes
                        - before;
                    if cached {
                        new_cpu += result.1;
                        new_decodes += decoded;
                        new = Some(result.0);
                    } else {
                        old_cpu += result.1;
                        old_decodes += decoded;
                        old = Some(result.0);
                    }
                }
                assert_eq!(old, new);
                counts += 1;
            }
        }
        let index = 7399;
        std::fs::write(&scope[index], format!("// BUG: one edit round {round}\n")).unwrap();
        checkout
            .checkout()
            .record_paths(&[relative[index].clone()])
            .unwrap();
        checkout.checkout().refresh_callgraph().unwrap();
        let view = checkout.view().unwrap();
        let before = crate::data_layer::checkout::source_work_for_test(&root).todo_decodes;
        let (old_one, new_one) = if round % 2 == 0 {
            let old = timed(|| {
                checkout
                    .checkout()
                    .todo_markers(&view, &relative, limit)
                    .unwrap()
            });
            let new = timed(|| {
                checkout
                    .checkout()
                    .todo_counts(&view, &relative, limit)
                    .unwrap()
            });
            (old, new)
        } else {
            let new = timed(|| {
                checkout
                    .checkout()
                    .todo_counts(&view, &relative, limit)
                    .unwrap()
            });
            let old = timed(|| {
                checkout
                    .checkout()
                    .todo_markers(&view, &relative, limit)
                    .unwrap()
            });
            (old, new)
        };
        let one_decodes =
            crate::data_layer::checkout::source_work_for_test(&root).todo_decodes - before;
        assert_eq!(one_decodes, FILES + 1);
        let detail = timed(|| {
            checkout
                .checkout()
                .todo_markers(&view, &detail_paths, limit)
                .unwrap()
        });
        let (memo_entries, memo_bytes) = checkout.checkout().todo_count_memo_stats_for_test();
        eprintln!("TODO_REPLAY round={} minutes=10 edits={} automatic={} explicit={} full_counts={} old_cpu_ms_per_min={:.3} new_cpu_ms_per_min={:.3} old_decodes={} new_decodes={} old_one_cpu_ms={:.3} old_one_wall_ms={:.3} new_one_cpu_ms={:.3} new_one_wall_ms={:.3} detail_100_cpu_ms={:.3} detail_100_wall_ms={:.3} one_count_decodes=1 detail_decodes=100 memo_entries={} memo_bytes={}", round + 1, edits, scheduled, explicit, counts, old_cpu / 10.0, new_cpu / 10.0, old_decodes, new_decodes, old_one.1, old_one.2, new_one.1, new_one.2, detail.1, detail.2, memo_entries, memo_bytes);
    }
}
