mod tests {
    use std::time::{Duration, Instant};

    use crate::repeat_state::RepeatBreaker;
    use crate::response_finalize::repeat_breaker::RepeatCall;
    use aft_engine::test_bridge::search_misc::KEY_IDLE_EXPIRY;

    #[test]
    fn audit_growth_idle_sessions_are_pruned_without_dropping_the_current_session() {
        let breaker = RepeatBreaker::default();
        let start = Instant::now();
        let cadence_secs = KEY_IDLE_EXPIRY.as_secs() + 1;
        for index in 0..128 {
            let session_id = format!("repeat-growth-{index}");
            breaker.observe_at(
                &session_id,
                &RepeatCall::new(
                    "read",
                    &serde_json::json!({ "path": format!("path-{index}") }),
                ),
                index,
                start + Duration::from_secs(cadence_secs * index),
            );
        }

        let current = format!("repeat-growth-{}", 127);
        let sessions = aft_engine::test_bridge::search_misc::session_call_counts(&breaker);
        assert_eq!(sessions.len(), 1, "expired sessions must not accumulate");
        let count = sessions.get(&current).expect("current session remains");
        assert_eq!(*count, 1, "current call remains available");

        breaker.observe_at(
            &current,
            &RepeatCall::new("read", &serde_json::json!({ "path": "path-127" })),
            128,
            start + Duration::from_secs(cadence_secs * 127),
        );
        assert_eq!(
            aft_engine::test_bridge::search_misc::session_call_counts(&breaker)[&current],
            2
        );
    }
}
