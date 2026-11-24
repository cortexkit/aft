    #[test]
    fn configured_context_and_filter_thread_share_root_counters() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config::default(),
        );
        ctx.update_config(|config| config.project_root = Some(root.clone()));
        let config = WatcherFilterConfig::new(root, None);

        config.counters.note_raw_event();

        assert_eq!(ctx.watcher_counters().snapshot().raw_events_total, 1);
    }
