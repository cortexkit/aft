#[cfg(test)]
mod health_warming_honesty_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_config(config: Config) -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), config)
    }

    fn health_tier2_status(ctx: &AppContext) -> &'static str {
        let root = std::path::Path::new("/tmp/health-warming-honesty-test");
        ctx.try_health_snapshot(root)
            .tier2
            .expect("tier2 component present")
            .status
    }

    #[test]
    fn health_tier2_and_inspect_builder_state_read_the_same_registry() {
        // Health must not report ready while the inspect builder registry
        // still has a live registration for this root.
        let ctx = ctx_with_config(Config::default());
        ctx.inspect_manager()
            .set_tier2_in_flight_for_test(crate::inspect_category::InspectCategory::DeadCode, true);

        assert_eq!(health_tier2_status(&ctx), "building");
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(crate::inspect_category::InspectCategory::DeadCode),
            crate::inspect::InspectBuilderState::Building
        );

        ctx.inspect_manager().set_tier2_in_flight_for_test(
            crate::inspect_category::InspectCategory::DeadCode,
            false,
        );

        assert_eq!(health_tier2_status(&ctx), "ready");
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(crate::inspect_category::InspectCategory::DeadCode),
            crate::inspect::InspectBuilderState::Absent
        );

        ctx.inspect_manager().record_tier2_attempt_outcome_for_test(
            crate::inspect_category::InspectCategory::DeadCode,
            crate::inspect::JobOutcome::Fresh {
                payload: crate::inspect::compatibility_test::callgraph_unavailable_aggregate(0),
            },
        );
        assert_eq!(
            health_tier2_status(&ctx),
            "ready",
            "a finished callgraph_unavailable attempt must not keep health.tier2=building"
        );
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(crate::inspect_category::InspectCategory::DeadCode),
            crate::inspect::InspectBuilderState::Absent
        );
        assert!(
            ctx.inspect_manager()
                .tier2_builder_state_detail(crate::inspect_category::InspectCategory::DeadCode)
                .starts_with("last attempt failed: callgraph_unavailable (attempt 1, first at "),
            "inspect refusals must carry the failed-attempt history the health surface no longer treats as busy"
        );
    }

    /// Nothing refreshes the whole-repository analyses in the background, so
    /// edits never leave the plane stale with a refresh deadline: it reports
    /// ready, with only its status field.
    #[test]
    fn edits_leave_the_tier2_plane_ready_without_refresh_fields() {
        let ctx = ctx_with_config(Config::default());
        let now = Instant::now();
        ctx.note_tier2_refresh_started_at(now);
        assert_eq!(ctx.tick_tier2_refresh_scheduler_at(now, 1), None);
        ctx.add_pending_tier2_paths([PathBuf::from("src/lib.rs")]);

        let tier2 = serde_json::to_value(
            ctx.try_health_snapshot(Path::new("/tmp/health-warming-honesty-test"))
                .tier2
                .expect("tier2 component present"),
        )
        .expect("tier2 health serializes");

        assert_eq!(tier2, serde_json::json!({ "status": "ready" }));
        assert!(!ctx.inspect_manager().tier2_any_in_flight());
    }

    #[test]
    fn failed_tier2_pass_returns_the_plane_to_ready() {
        let ctx = ctx_with_config(Config::default());
        ctx.inspect_manager().set_tier2_in_flight_for_test(
            crate::inspect_category::InspectCategory::Duplicates,
            true,
        );
        assert_eq!(health_tier2_status(&ctx), "building");

        ctx.inspect_manager().record_tier2_attempt_outcome_for_test(
            crate::inspect_category::InspectCategory::Duplicates,
            crate::inspect::JobOutcome::Failed {
                message: "scan failed".to_string(),
            },
        );
        assert_eq!(health_tier2_status(&ctx), "ready");
    }
}
