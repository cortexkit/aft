    #[test]
    fn worker_only_edits_with_tier2_enabled_do_not_refresh_the_callgraph() {
        use crate::inspect::tier2_scheduler::{
            TIER2_REFRESH_COLD_CACHE_DELAY, TIER2_REFRESH_DEBOUNCE, TIER2_REFRESH_MIN_INTERVAL,
        };
        use crate::inspect::Tier2TriggerReason;

        let quiet = Duration::from_millis(100);
        let fixture = PublicationFixture::new(quiet, quiet);
        let ctx = fixture.context();
        ctx.set_worker_only_routes(true);
        assert!(ctx.config().inspect.enabled);
        let configured_at = Instant::now()
            .checked_sub(TIER2_REFRESH_COLD_CACHE_DELAY + Duration::from_secs(1))
            .unwrap();
        ctx.reset_tier2_refresh_scheduler_at(configured_at);
        let now = Instant::now();
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(now, 0),
            Some(Tier2TriggerReason::ConfigureWarm)
        );
        wait_until(|| {
            crate::runtime_drain::drain_inspect_events(&ctx);
            ctx.status_bar_count_values().todos == Some(0)
        });
        let before = fixture.refreshes();
        let path = fixture.handle.root().join("src/lib.rs");
        std::fs::write(
            &path,
            "// TODO: keep the count current\npub fn original() {}\n",
        )
        .unwrap();
        fixture.handle.note_paths(std::slice::from_ref(&path));
        ctx.add_pending_tier2_paths([path]);
        let edited_at = now + TIER2_REFRESH_MIN_INTERVAL;
        assert_eq!(ctx.tick_tier2_refresh_scheduler_at(edited_at, 1), None);
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(edited_at + TIER2_REFRESH_DEBOUNCE, 0),
            Some(Tier2TriggerReason::Debounce)
        );
        wait_until(|| {
            crate::runtime_drain::drain_inspect_events(&ctx);
            ctx.status_bar_count_values().todos == Some(1)
        });
        std::thread::sleep(quiet * 3);
        assert_eq!(fixture.refreshes(), before);
        assert!(pending(fixture.handle.checkout()));
    }
