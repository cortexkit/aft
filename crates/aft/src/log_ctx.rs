//! Thread-local session context for log lines.
//!
//! AFT runs a single-threaded request loop. Each incoming request carries a
//! `session_id` that identifies the OpenCode/Pi session. By storing it in a
//! thread-local we can automatically prepend `[ses_xxx]` to every `slog_*`
//! log macro call without threading the session id through every function
//! signature.
//!
//! Background threads spawned during request handling (search-index pre-warm,
//! semantic-index build) **must** capture the session id before spawning and
//! re-install it on the new thread via [`set_session`] or [`with_session`].

use std::cell::RefCell;

thread_local! {
    /// Current session id for log tagging. `None` means "no session context".
    static CURRENT_SESSION: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Most-recent non-None session id observed on this thread. Used as a
    /// fallback for between-request log sites that have no inherent session
    /// (filesystem watcher invalidation, gitignore rebuilds triggered by
    /// the OS, etc.) so their lines still carry a session tag.
    ///
    /// This is best-effort: in a multi-session setup the tag may attribute
    /// project-wide events to whichever session was active most recently.
    /// That's acceptable because the vast majority of AFT users have one
    /// active session per project and the alternative is an untagged line.
    static LAST_SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set the current thread-local session id.
///
/// Call this at the start of a background thread that captured the session id
/// from the parent request loop.
pub fn set_session(session: Option<String>) {
    if let Some(sid) = session.as_deref() {
        LAST_SESSION.with(|s| {
            *s.borrow_mut() = Some(sid.to_string());
        });
    }
    CURRENT_SESSION.with(|s| {
        *s.borrow_mut() = session;
    });
}

struct SessionGuard(Option<String>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        // Restore CURRENT_SESSION directly (don't go through set_session,
        // which would also overwrite LAST_SESSION — the guard's job is to
        // restore the previous CURRENT_SESSION value, not to alter the
        // last-session fallback).
        CURRENT_SESSION.with(|s| {
            *s.borrow_mut() = self.0.take();
        });
    }
}

/// Run `f` with the given session id set on the current thread, restoring the
/// previous value afterwards (RAII-style and panic-safe).
///
/// This is the primary entry point for the main request loop: wrap the
/// dispatch call in `with_session(req.session_id.clone(), || { ... })`.
pub fn with_session<T>(session: Option<String>, f: impl FnOnce() -> T) -> T {
    let prev = current_session();
    set_session(session);
    let _guard = SessionGuard(prev);
    f()
}

/// Return the current session id (e.g. `"abcd1234"`), or `None` if no session is set.
pub fn current_session() -> Option<String> {
    CURRENT_SESSION.with(|s| s.borrow().clone())
}

/// Return the current session id prefix string, e.g. `"[ses_abcd1234] "`,
/// or an empty string if no session id has ever been observed on this thread.
///
/// Resolution order:
///   1. Current request's session (set via `with_session`).
///   2. Most-recent session observed on this thread (fallback for
///      between-request log sites like watcher invalidation that have no
///      inherent session context).
///
/// The stored session id may already carry the `ses_` prefix (OpenCode's
/// real session IDs do); detect that and avoid double-prefixing.
pub fn session_prefix() -> String {
    let sid_opt = CURRENT_SESSION
        .with(|s| s.borrow().clone())
        .or_else(|| LAST_SESSION.with(|s| s.borrow().clone()));
    match sid_opt.as_deref() {
        Some(sid) if sid.starts_with("ses_") => format!("[{}] ", sid),
        Some(sid) => format!("[ses_{}] ", sid),
        None => String::new(),
    }
}

/// Log at INFO level with the optional `[ses_xxx]` session tag.
///
/// Use this instead of `log::info!(...)` in per-request code paths.
/// The macro automatically reads the thread-local session id and formats:
///
/// ```text
/// With session:    [aft] [ses_abcd1234] semantic index: rebuilding from scratch
/// Without session: [aft] semantic index: rebuilding from scratch
/// ```
///
/// The `[aft]` / `[aft-lsp]` outer prefix is added by env_logger based on the
/// log target — do NOT inline it into the macro body, that produces a doubled
/// `[aft-lsp] [aft]` prefix when LSP modules log.
#[macro_export]
macro_rules! slog_info {
    ($($arg:tt)*) => {
        log::info!("{}{}", $crate::log_ctx::session_prefix(), format!($($arg)*))
    };
}

/// Log at WARN level with the optional `[ses_xxx]` session tag.
///
/// See [`slog_info!`] for format details.
#[macro_export]
macro_rules! slog_warn {
    ($($arg:tt)*) => {
        log::warn!("{}{}", $crate::log_ctx::session_prefix(), format!($($arg)*))
    };
}

/// Log at ERROR level with the optional `[ses_xxx]` session tag.
///
/// See [`slog_info!`] for format details.
#[macro_export]
macro_rules! slog_error {
    ($($arg:tt)*) => {
        log::error!("{}{}", $crate::log_ctx::session_prefix(), format!($($arg)*))
    };
}

/// Log at DEBUG level with the optional `[ses_xxx]` session tag.
///
/// Use for verbose-but-useful diagnostics that should be silent by default and
/// only surface when `RUST_LOG=aft=debug` (or similar) is set.
///
/// See [`slog_info!`] for format details.
#[macro_export]
macro_rules! slog_debug {
    ($($arg:tt)*) => {
        log::debug!("{}{}", $crate::log_ctx::session_prefix(), format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset both thread-locals so each test starts from a clean slate.
    /// Rust runs tests on a thread pool — without this helper a previous
    /// test in the same thread could leak its LAST_SESSION value into
    /// the next one.
    fn reset_session_state() {
        CURRENT_SESSION.with(|s| {
            *s.borrow_mut() = None;
        });
        LAST_SESSION.with(|s| {
            *s.borrow_mut() = None;
        });
    }

    #[test]
    fn with_session_sets_and_clears() {
        reset_session_state();
        // Initially no current session
        CURRENT_SESSION.with(|s| {
            assert!(s.borrow().is_none());
        });

        // Set inside with_session
        with_session(Some("test123".to_string()), || {
            CURRENT_SESSION.with(|s| {
                assert_eq!(s.borrow().as_deref(), Some("test123"));
            });
        });

        // CURRENT_SESSION cleared after with_session
        CURRENT_SESSION.with(|s| {
            assert!(s.borrow().is_none());
        });
    }

    #[test]
    fn with_session_none_is_noop() {
        reset_session_state();
        with_session(None, || {
            CURRENT_SESSION.with(|s| {
                assert!(s.borrow().is_none());
            });
        });
    }

    #[test]
    fn session_prefix_format() {
        reset_session_state();
        with_session(Some("abcd1234".to_string()), || {
            assert_eq!(session_prefix(), "[ses_abcd1234] ");
        });

        // After exiting with_session, CURRENT is None but LAST_SESSION
        // still carries "abcd1234" — the prefix falls back to it.
        assert_eq!(session_prefix(), "[ses_abcd1234] ");

        // Truly empty starting state: no prefix.
        reset_session_state();
        assert_eq!(session_prefix(), "");
    }

    #[test]
    fn session_prefix_does_not_double_prefix_real_ids() {
        reset_session_state();
        // Real OpenCode session IDs already start with "ses_" — the
        // formatter must not turn that into "ses_ses_xxx".
        with_session(Some("ses_313660571ffeZTsf4koSJwk50Q".to_string()), || {
            assert_eq!(session_prefix(), "[ses_313660571ffeZTsf4koSJwk50Q] ");
        });
    }

    #[test]
    fn set_session_direct() {
        reset_session_state();
        set_session(Some("direct".to_string()));
        CURRENT_SESSION.with(|s| {
            assert_eq!(s.borrow().as_deref(), Some("direct"));
        });
        set_session(None);
        CURRENT_SESSION.with(|s| {
            assert!(s.borrow().is_none());
        });
        // LAST_SESSION still remembers it for the fallback.
        LAST_SESSION.with(|s| {
            assert_eq!(s.borrow().as_deref(), Some("direct"));
        });
    }

    /// Regression: between-request log sites (filesystem watcher
    /// invalidation, gitignore rebuilds) run outside a `with_session`
    /// scope but should still carry a session tag derived from the most
    /// recent active session on this thread. Without the LAST_SESSION
    /// fallback these lines appeared untagged in the plugin log.
    #[test]
    fn session_prefix_falls_back_to_last_session_after_with_session_exits() {
        reset_session_state();

        // Simulate a request handled on this thread.
        with_session(Some("ses_first".to_string()), || {
            assert_eq!(session_prefix(), "[ses_first] ");
        });

        // CURRENT_SESSION is now None, but a between-request log site
        // should still get a tag from LAST_SESSION.
        assert!(current_session().is_none());
        assert_eq!(session_prefix(), "[ses_first] ");
    }

    #[test]
    fn last_session_updates_with_each_with_session_call() {
        reset_session_state();

        with_session(Some("ses_first".to_string()), || {});
        assert_eq!(session_prefix(), "[ses_first] ");

        with_session(Some("ses_second".to_string()), || {});
        // Newer session wins as the fallback.
        assert_eq!(session_prefix(), "[ses_second] ");
    }

    #[test]
    fn with_session_none_does_not_clear_last_session() {
        reset_session_state();

        with_session(Some("ses_real".to_string()), || {});
        assert_eq!(session_prefix(), "[ses_real] ");

        // A subsequent `with_session(None, ...)` call (which can happen
        // for synthetic / system requests with no session_id) must not
        // erase the fallback tag from earlier real traffic.
        with_session(None, || {});
        assert_eq!(session_prefix(), "[ses_real] ");
    }
}
