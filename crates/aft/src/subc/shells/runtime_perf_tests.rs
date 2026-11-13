use super::*;
use crate::bash_background::buffer::ARTIFACT_OPENS;
use crate::bash_background::persistence::{read_task_at, TaskPaths};
use std::io::Write;

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

fn fixture() -> (tempfile::TempDir, Arc<Executor>, Arc<App>, Vec<TaskPaths>) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let db = Arc::new(std::sync::Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    let mut active = Vec::new();
    for root in 0..40 {
        let path = dir.path().join(format!("root-{root}"));
        std::fs::create_dir(&path).unwrap();
        let ctx = Arc::new(crate::context::AppContext::with_app_and_provider(
            app.clone(),
            Box::new(crate::language::StubProvider),
            crate::config::Config {
                project_root: Some(path.clone()),
                ..Default::default()
            },
        ));
        ctx.set_db(db.clone());
        ctx.set_harness(crate::harness::Harness::Opencode);
        assert!(executor.register_actor(
            crate::path_identity::ProjectRootId::from_path(&path).unwrap(),
            ctx.clone()
        ));
        for index in 0..if root < 20 { 8 } else { 7 } {
            let running = root < 5 && index == 0;
            let (_, _, task) = captured(&path, b"initial output\n", false, !running);
            let mut metadata = read_task_at(&task).unwrap();
            // Remote fixtures stay running without claiming a local child PID.
            if running {
                metadata.remote = Some(serde_json::from_value(json!({"harness":"test","session":"plain-session","job_id":format!("0b816da2-78cb-4fef-8abc-{root:012x}"),"stdout_len":15,"stderr_len":0,"cancel_requested":false})).unwrap());
                crate::bash_background::persistence::write_task_at(&task, &metadata).unwrap();
                active.push(task.paths.clone());
            }
            crate::db::bash_tasks::upsert_bash_task(
                &db.lock().unwrap(),
                &metadata.to_bash_task_row("test", &task.paths).unwrap(),
            )
            .unwrap();
            ctx.bash_background()
                .insert_shell_fixture(metadata, task.paths)
                .unwrap();
        }
    }
    (dir, executor, app, active)
}

fn append(paths: &TaskPaths) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(&paths.stdout)
        .unwrap()
        .write_all(b"progress\n")
        .unwrap();
    crate::bash_background::shell_metadata::changed();
}

#[test]
#[ignore = "40-root collection measurement; run optimized and in isolation"]
fn shells_watch_collection_fixture() {
    let (_dir, executor, app, active) = fixture();
    let db = app.db().unwrap();
    db.lock().unwrap().trace(Some(trace_facts_sql));
    for legacy in [true, false] {
        let collect = if legacy {
            super::legacy::collect_legacy
        } else {
            collect_at_revision
        };
        let mut facts = collect(&executor, Some(&db), true, DbFacts::default(), 0).db;
        let mut cpu = 0;
        let mut wall = Duration::ZERO;
        let mut opens = 0;
        let mut file_opens = 0;
        let mut queries = 0;
        for revision in 1..=40 {
            for paths in &active {
                append(paths);
            }
            ARTIFACT_OPENS.with(|count| count.set(0));
            crate::bash_background::persistence::work_counts::reset();
            FACTS_SQL_TRACE.with(|trace| trace.borrow_mut().clear());
            let start = std::time::Instant::now();
            let cpu_start = cpu_ns();
            let collected = collect(&executor, Some(&db), true, facts, revision);
            cpu += cpu_ns() - cpu_start;
            wall += start.elapsed();
            assert_eq!(collected.rows.len(), 300);
            facts = collected.db;
            opens += ARTIFACT_OPENS.with(|count| count.get());
            file_opens += crate::bash_background::persistence::work_counts::get().opens;
            queries += FACTS_SQL_TRACE.with(|trace| trace.borrow().len());
        }
        println!("{}: 40 roots / 300 retained / 5 output producers / 40 revisions: CPU {:.3} ms/revision, wall {:.3} ms/revision, artifact opens {:.1}/revision, persistence file/directory opens {:.1}/revision, SQL statements {:.1}/revision", if legacy { "legacy" } else { "cached" }, cpu as f64 / 40e6, wall.as_secs_f64() * 25.0, opens as f64 / 40.0, file_opens as f64 / 40.0, queries as f64 / 40.0);
    }
    db.lock().unwrap().trace(None);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let service = Service::new(app, executor);
        service.refresh(true).await;
        service.enabled.store(true, Ordering::Release);
        let before = COLLECT_CALLS.load(Ordering::Relaxed);
        let start = std::time::Instant::now();
        let until = start + Duration::from_secs(5);
        let mut notifications = 0;
        while std::time::Instant::now() < until {
            for paths in &active { append(paths); notifications += 1; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        service.stop();
        let calls = COLLECT_CALLS.load(Ordering::Relaxed) - before;
        println!("busy watch: {calls} collections / {:.3} s = {:.2} revisions/s, {} output notifications", start.elapsed().as_secs_f64(), calls as f64 / start.elapsed().as_secs_f64(), notifications);
        service.refresh(true).await;
    });
}
