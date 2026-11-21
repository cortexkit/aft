use super::*;
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Work {
    kind_probes: usize,
    uri_resolutions: usize,
}

enum Signal {
    Filesystem,
    Finished(Work),
}

struct Probe {
    work: Work,
    gate: Option<(Sender<Signal>, Receiver<()>)>,
}

thread_local! {
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

fn record(update: impl FnOnce(&mut Work)) {
    PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        if let Some(probe) = probe.as_mut() {
            update(&mut probe.work);
            if let Some((entered, release)) = probe.gate.take() {
                entered.send(Signal::Filesystem).unwrap();
                release.recv().unwrap();
            }
        }
    });
}

pub(super) fn record_kind_probe() {
    record(|work| work.kind_probes += 1);
}

pub(super) fn record_uri_resolution() {
    record(|work| work.uri_resolutions += 1);
}

fn backlog(root: &Path) -> WatcherForwardBacklog {
    let paths = (0..WATCHER_FORWARD_BACKLOG_CAP)
        .map(|index| root.join(format!("file-{index}.rs")))
        .collect::<Vec<_>>();
    let mut backlog = WatcherForwardBacklog::default();
    backlog.merge(&paths, false, vec!["custom.config".into()]);
    backlog
}

fn gated_forward(root: &Path, direct: bool) -> (Work, bool) {
    let backlog = backlog(root);
    let manager = Arc::new(parking_lot::Mutex::new(LspManager::new()));
    let worker_manager = Arc::clone(&manager);
    let (signal_tx, signal_rx) = crossbeam_channel::unbounded();
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let worker = std::thread::spawn(move || {
        PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Probe {
                work: Work::default(),
                gate: Some((signal_tx.clone(), release_rx)),
            });
        });
        let reloads = {
            let mut manager = worker_manager.lock();
            if direct {
                let events = backlog
                    .paths
                    .into_iter()
                    .map(|path| (path, FileChangeType::CHANGED))
                    .collect::<Vec<_>>();
                manager.forward_watcher_file_events(&events, &backlog.extra_config_markers)
            } else {
                backlog.apply(&mut manager)
            }
        };
        assert!(reloads.is_empty());
        let work = PROBE.with(|probe| probe.borrow_mut().take().unwrap().work);
        signal_tx.send(Signal::Finished(work)).unwrap();
    });
    let (work, manager_progress) = match signal_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
        Signal::Finished(work) => (work, manager.try_lock().is_some()),
        Signal::Filesystem => {
            let progress = manager.try_lock_for(Duration::from_millis(50)).is_some();
            release_tx.send(()).unwrap();
            let Signal::Finished(work) = signal_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            else {
                panic!("the filesystem gate must be entered only once");
            };
            (work, progress)
        }
    };
    worker.join().unwrap();
    (work, manager_progress)
}

#[test]
fn watcher_without_clients_does_no_filesystem_work() {
    let tmp = tempfile::tempdir().unwrap();
    let (queued, queued_progress) = gated_forward(tmp.path(), false);
    let (direct, direct_progress) = gated_forward(tmp.path(), true);
    eprintln!("queued={queued:?}, direct={direct:?}, manager_progress={queued_progress}/{direct_progress}");
    assert_eq!(
        queued,
        Work::default(),
        "a backlog with no clients needs no disk state"
    );
    assert_eq!(
        direct,
        Work::default(),
        "direct forwarding with no clients needs no disk state"
    );
    assert!(
        queued_progress && direct_progress,
        "filesystem work must not block the manager when no client can receive the batch"
    );
}

#[test]
fn watcher_without_clients_has_no_overflow_reload_target() {
    let mut manager = LspManager::new();
    let backlog = WatcherForwardBacklog {
        overflowed: true,
        reload_all_rust: true,
        ..WatcherForwardBacklog::default()
    };
    assert!(backlog.apply(&mut manager).is_empty());
    assert!(manager.watched_file_skip_logged.is_empty());
}

#[test]
#[ignore = "release-mode watcher lock-hold measurement"]
fn measure_watcher_without_clients_lock_hold() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..WATCHER_FORWARD_BACKLOG_CAP {
        std::fs::write(
            tmp.path().join(format!("file-{index}.rs")),
            "fn example() {}\n",
        )
        .unwrap();
    }
    let manager = parking_lot::Mutex::new(LspManager::new());
    let mut elapsed = Vec::new();
    for _ in 0..101 {
        let backlog = backlog(tmp.path());
        let mut manager = manager.lock();
        let started = Instant::now();
        assert!(backlog.apply(&mut manager).is_empty());
        elapsed.push(started.elapsed());
    }
    elapsed.sort();
    eprintln!(
        "2048-path no-client backlog, 101 samples: locked apply p50={:?}, p95={:?}",
        elapsed[50], elapsed[95]
    );
}
