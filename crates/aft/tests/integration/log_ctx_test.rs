/// Test that `with_session` correctly sets and clears the thread-local session id.
#[test]
fn with_session_sets_and_clears_thread_local() {
    // Initially no session
    assert!(aft::log_ctx::current_session().is_none());

    // Set inside with_session
    aft::log_ctx::with_session(Some("test_session_1".to_string()), || {
        assert_eq!(
            aft::log_ctx::current_session(),
            Some("test_session_1".to_string())
        );
    });

    // Cleared after with_session
    assert!(aft::log_ctx::current_session().is_none());

    // Nested sessions
    aft::log_ctx::with_session(Some("outer".to_string()), || {
        assert_eq!(aft::log_ctx::current_session(), Some("outer".to_string()));
        aft::log_ctx::with_session(Some("inner".to_string()), || {
            assert_eq!(aft::log_ctx::current_session(), Some("inner".to_string()));
        });
        // After inner with_session drops, the outer session is restored.
        assert_eq!(aft::log_ctx::current_session(), Some("outer".to_string()));
    });

    assert!(aft::log_ctx::current_session().is_none());
}

#[test]
fn with_session_restores_after_panic() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    assert!(aft::log_ctx::current_session().is_none());
    let result = catch_unwind(AssertUnwindSafe(|| {
        aft::log_ctx::with_session(Some("panic_session".to_string()), || {
            assert_eq!(
                aft::log_ctx::current_session(),
                Some("panic_session".to_string())
            );
            panic!("intentional panic for with_session guard test");
        });
    }));

    assert!(result.is_err());
    assert!(aft::log_ctx::current_session().is_none());
}

/// Test that `set_session` directly sets the thread-local.
#[test]
fn set_session_direct_manipulation() {
    assert!(aft::log_ctx::current_session().is_none());

    aft::log_ctx::set_session(Some("direct_session".to_string()));
    assert_eq!(
        aft::log_ctx::current_session(),
        Some("direct_session".to_string())
    );

    aft::log_ctx::set_session(None);
    assert!(aft::log_ctx::current_session().is_none());
}

/// Test that `slog_info!` prepends `[aft] [ses_xxx]` when a session is set
/// and `[aft]` when no session is set.
///
/// Tests the macro output format by calling session_prefix() directly and
/// verifying the format string construction, since the global logger may
/// already be set by other tests.
#[test]
fn slog_macros_prepend_session_tag() {
    // Case 1: With session id — verify session_prefix() output.
    // The slog_* macros expand to `log::info!("{}{}", session_prefix(), ...)`.
    // env_logger prepends `[aft]` or `[aft-lsp]` based on target at output time;
    // we test the macro body directly, not env_logger formatting.
    aft::log_ctx::with_session(Some("abcd1234".to_string()), || {
        let prefix = aft::log_ctx::session_prefix();
        assert_eq!(prefix, "[ses_abcd1234] ");
        let body = format!(
            "{}semantic index: rebuilding from scratch (450 files)",
            prefix
        );
        assert_eq!(
            body,
            "[ses_abcd1234] semantic index: rebuilding from scratch (450 files)"
        );
    });

    // Case 2: Without explicit session id — verify LAST_SESSION fallback.
    // session_prefix() falls back to the most recently set session id so that
    // between-request log lines (filesystem watcher invalidation, gitignore
    // matcher rebuilds) still carry a session tag. This thread had Case 1
    // set "abcd1234", so the fallback should return that.
    aft::log_ctx::with_session(None, || {
        let prefix = aft::log_ctx::session_prefix();
        assert_eq!(prefix, "[ses_abcd1234] ");
    });
}
