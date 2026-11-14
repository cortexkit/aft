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
//!
//! A line logged outside any session scope carries no session tag. Threads
//! such as the request loop, the subc frame loop and executor workers serve
//! many sessions in turn, so "the last session this thread served" says
//! nothing about who owns a later between-request line; tagging with it sent
//! operators to the wrong session. Code that logs about a specific session
//! from such a thread (a pending bind, a background task) must enter that
//! session's scope with [`with_session`] first.

use std::cell::RefCell;

thread_local! {
    /// Current session id for log tagging. `None` means "no session context".
    static CURRENT_SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set the current thread-local session id.
///
/// Call this at the start of a background thread that captured the session id
/// from the parent request loop.
pub fn set_session(session: Option<String>) {
    CURRENT_SESSION.with(|s| {
        *s.borrow_mut() = session;
    });
}

struct SessionGuard(Option<String>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
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
/// or an empty string when no session scope is active on this thread.
///
/// The stored session id may already carry the `ses_` prefix (OpenCode's
/// real session IDs do); detect that and avoid double-prefixing.
pub fn session_prefix() -> String {
    match current_session().as_deref() {
        Some(sid) if sid.starts_with("ses_") => format!("[{}] ", sid),
        Some(sid) => format!("[ses_{}] ", sid),
        None => String::new(),
    }
}

#[doc(hidden)]
pub fn format_session_message(message: std::fmt::Arguments<'_>) -> String {
    let mut line = String::new();
    write_session_message(&mut line, message).expect("writing to a String cannot fail");
    line
}

fn write_session_message(
    writer: &mut impl std::fmt::Write,
    message: std::fmt::Arguments<'_>,
) -> std::fmt::Result {
    CURRENT_SESSION.with(|current| {
        if let Some(sid) = current.borrow().as_deref() {
            writer.write_str(if sid.starts_with("ses_") {
                "["
            } else {
                "[ses_"
            })?;
            writer.write_str(sid)?;
            writer.write_str("] ")?;
        }
        Ok(())
    })?;
    // Release the session borrow before user-provided Display implementations
    // run; formatting a message may itself change the logging context.
    writer.write_fmt(message)
}

/// Log at INFO level with the optional `[ses_xxx]` session tag.
///
/// Use this instead of `log::info!(...)` in per-request code paths.
/// The macro automatically reads the thread-local session id and formats:
///
/// ```text
/// With session:    <ts> [aft] INFO [ses_abcd1234] semantic index: rebuilding from scratch
/// Without session: <ts> [aft] INFO semantic index: rebuilding from scratch
/// ```
///
/// The timestamp, `[aft]` / `[aft-lsp]` outer prefix and level are added by
/// the env_logger format in `logging.rs` based on the log record — do NOT
/// inline them into the macro body, that produces a doubled
/// `[aft-lsp] [aft]` prefix when LSP modules log.
#[macro_export]
macro_rules! slog_info {
    ($($arg:tt)*) => {
        if $crate::logging::slog_info_enabled(module_path!()) {
            $crate::logging::emit_slog_info(
                module_path!(),
                $crate::log_ctx::format_session_message(format_args!($($arg)*)),
            )
        }
    };
}

/// Log at WARN level with the optional `[ses_xxx]` session tag.
///
/// See [`slog_info!`] for format details.
#[macro_export]
macro_rules! slog_warn {
    ($($arg:tt)*) => {
        if $crate::logging::slog_warn_enabled(module_path!()) {
            $crate::logging::emit_slog_warn(
                module_path!(),
                $crate::log_ctx::format_session_message(format_args!($($arg)*)),
            )
        }
    };
}

/// Log at ERROR level with the optional `[ses_xxx]` session tag.
///
/// See [`slog_info!`] for format details.
#[macro_export]
macro_rules! slog_error {
    ($($arg:tt)*) => {
        if $crate::logging::slog_error_enabled(module_path!()) {
            $crate::logging::emit_slog_error(
                module_path!(),
                $crate::log_ctx::format_session_message(format_args!($($arg)*)),
            )
        }
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
        if $crate::logging::slog_debug_enabled(module_path!()) {
            $crate::logging::emit_slog_debug(
                module_path!(),
                $crate::log_ctx::format_session_message(format_args!($($arg)*)),
            )
        }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_message_streams_borrowed_session_and_message() {
        use super::*;
        struct Writer {
            sid: *const u8,
            saw_sid: bool,
            saw_message: bool,
        }
        impl std::fmt::Write for Writer {
            fn write_str(&mut self, value: &str) -> std::fmt::Result {
                if value == "borrow-check" {
                    assert_eq!(
                        value.as_ptr(),
                        self.sid,
                        "session was cloned before formatting"
                    );
                    self.saw_sid = true;
                }
                if value == "message" {
                    self.saw_message = true;
                }
                assert!(
                    !value.contains("message") || value == "message",
                    "message was preformatted"
                );
                Ok(())
            }
        }
        with_session(Some("borrow-check".to_string()), || {
            let sid = CURRENT_SESSION.with(|s| s.borrow().as_ref().unwrap().as_ptr());
            let mut writer = Writer {
                sid,
                saw_sid: false,
                saw_message: false,
            };
            write_session_message(&mut writer, format_args!("message")).unwrap();
            assert!(writer.saw_sid && writer.saw_message);
            assert_eq!(
                format_session_message(format_args!("message")),
                "[ses_borrow-check] message"
            );
        });
    }

    use super::*;
    use std::cell::Cell;
    use std::fmt;

    struct LogMaxLevelGuard(log::LevelFilter);

    impl LogMaxLevelGuard {
        fn set(level: log::LevelFilter) -> Self {
            let previous = log::max_level();
            log::set_max_level(level);
            Self(previous)
        }
    }

    impl Drop for LogMaxLevelGuard {
        fn drop(&mut self) {
            log::set_max_level(self.0);
        }
    }

    struct CountingDisplay<'a>(&'a Cell<usize>);

    impl fmt::Display for CountingDisplay<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.set(self.0.get() + 1);
            formatter.write_str("counted")
        }
    }

    /// Reset the thread-local so each test starts from a clean slate.
    /// Rust runs tests on a thread pool, and a test that panics inside
    /// `set_session` (rather than `with_session`) could otherwise leak its
    /// session into the next test on the same thread.
    fn reset_session_state() {
        CURRENT_SESSION.with(|s| {
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

        // Once the scope has ended there is no session context, so no prefix.
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
        assert_eq!(session_prefix(), "[ses_direct] ");
        set_session(None);
        CURRENT_SESSION.with(|s| {
            assert!(s.borrow().is_none());
        });
        assert_eq!(session_prefix(), "");
    }

    /// A line logged after a session scope ends, on a thread that served that
    /// session, carries no session tag. Threads like the request loop and the
    /// subc frame loop serve many sessions in turn, so tagging an unscoped
    /// line with the previous session (as an earlier fallback did) attributed
    /// project-wide or other-session events to whichever session ran last.
    #[test]
    fn unscoped_line_after_scope_ends_carries_no_session() {
        reset_session_state();

        with_session(Some("ses_first".to_string()), || {
            assert_eq!(
                format_session_message(format_args!("inside")),
                "[ses_first] inside"
            );
        });

        assert!(current_session().is_none());
        assert_eq!(
            format_session_message(format_args!("invalidated 1 files")),
            "invalidated 1 files"
        );
    }

    /// A thread that served session A and then logs about session B inside
    /// B's scope tags the line with B only.
    #[test]
    fn scoped_line_carries_its_own_session_not_the_previous_one() {
        reset_session_state();

        with_session(Some("ses_A".to_string()), || {});
        let line = with_session(Some("ses_B".to_string()), || {
            format_session_message(format_args!("pending bind"))
        });
        assert_eq!(line, "[ses_B] pending bind");
    }

    #[test]
    fn disabled_debug_log_does_not_format_arguments() {
        let _max_level = LogMaxLevelGuard::set(log::LevelFilter::Warn);
        let formatted = Cell::new(0);

        crate::slog_debug!("debug argument: {}", CountingDisplay(&formatted));

        assert_eq!(
            formatted.get(),
            0,
            "disabled debug arguments must stay lazy"
        );
    }

    /// `with_session(None, ...)` (synthetic or system requests with no
    /// session id) runs its closure with no session tag, even nested inside
    /// a real session's scope, and restores that scope afterwards.
    #[test]
    fn with_session_none_runs_untagged_and_restores_outer_scope() {
        reset_session_state();

        with_session(Some("ses_real".to_string()), || {
            with_session(None, || {
                assert_eq!(session_prefix(), "");
            });
            assert_eq!(session_prefix(), "[ses_real] ");
        });
        assert_eq!(session_prefix(), "");
    }
}
