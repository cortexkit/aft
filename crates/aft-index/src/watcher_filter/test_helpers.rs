    /// A watcher thread whose dispatch channel is full, built over `root`.
    /// The channel holds one `earlier.rs` batch that nobody receives.
    fn filter_with_full_dispatch_channel(
        root: &Path,
    ) -> (
        WatcherFilterThread,
        crossbeam_channel::Receiver<WatcherDispatchEvent>,
        Arc<AtomicBool>,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.try_send(WatcherDispatchEvent::Paths(vec![root.join("earlier.rs")]))
            .unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let filter = WatcherFilterThread::new(
            WatcherFilterConfig::new(root.to_path_buf(), None),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(AtomicU64::new(1)),
            tx,
            Arc::clone(&shutdown),
        );
        (filter, rx, shutdown)
    }

    /// Flushing into a full dispatch channel must not park the watcher thread
    /// in the channel's send (whose back-off spins and yields): the batches
    /// are held, merged into one, and handed on once the consumer catches up.
    #[test]
    fn watcher_full_dispatch_channel_holds_and_merges_batches_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join(name), "fn f() {}\n").unwrap();
        }
        let (mut filter, rx, shutdown) = filter_with_full_dispatch_channel(&root);
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config::default(),
        );
        ctx.update_config(|config| config.project_root = Some(root.clone()));
        *ctx.watcher_rx().lock() = Some(rx.clone());
        let batches_root = root.clone();
        let flusher = thread::spawn(move || {
            for batch in [["a.rs", "b.rs"], ["b.rs", "c.rs"]] {
                assert!(filter.push_raw_paths(batch.map(|name| batches_root.join(name)).to_vec()));
                assert!(filter.flush_pending(), "the watcher thread stopped");
            }
            filter
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !flusher.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let finished = flusher.is_finished();
        // Release a blocked flush so a failing run ends instead of hanging.
        shutdown.store(true, Ordering::SeqCst);
        let mut filter = flusher.join().unwrap();
        assert!(
            finished,
            "flushing into a full dispatch channel blocked the watcher thread"
        );
        shutdown.store(false, Ordering::SeqCst);
        let merged = WatcherDispatchEvent::Paths(
            ["a.rs", "b.rs", "c.rs"]
                .map(|name| root.join(name))
                .to_vec(),
        );
        assert_eq!(filter.backlog, VecDeque::from([merged.clone()]));
        assert_eq!(filter.config.counters.dispatch_backlog(), 1);

        // The consumer catches up: the held batch follows the earlier one.
        assert_eq!(
            rx.try_recv().unwrap(),
            WatcherDispatchEvent::Paths(vec![root.join("earlier.rs")])
        );
        assert!(rx.is_empty());
        assert!(
            ctx.watcher_query_has_pending_changes(),
            "held events remain pending even after the channel empties"
        );
        assert!(filter.retry_backlog());
        assert_eq!(filter.config.counters.dispatch_backlog(), 0);
        assert!(
            ctx.watcher_query_has_pending_changes(),
            "dispatched events remain pending after the backlog clears"
        );
        assert_eq!(rx.try_recv().unwrap(), merged);
        assert!(filter.backlog.is_empty());
        assert_eq!(filter.config.counters.dispatch_backlog(), 0);
        assert!(!ctx.watcher_query_has_pending_changes());
    }
