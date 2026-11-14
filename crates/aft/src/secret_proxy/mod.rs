pub use aft_base::secret_proxy::*;

#[cfg(test)]
pub(crate) mod test_hardening {
    use std::cell::Cell;

    /// Set by the audit tests' isolated child processes; only there does
    /// `protect_secret_process` change the real process limits.
    pub(crate) const REAL_ENV: &str = "AFT_S4_AUDIT_CHILD";

    thread_local! {
        static HARDENED: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn mark() {
        HARDENED.with(|hardened| hardened.set(true));
    }

    /// Whether `protect_secret_process` ran on this thread. Each test runs on
    /// its own thread, so one test's call never satisfies another's check.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn hardened_on_this_thread() -> bool {
        HARDENED.with(Cell::get)
    }
}

#[cfg(all(test, unix))]
mod tests;
