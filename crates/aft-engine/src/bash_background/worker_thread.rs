//! Fallible creation of the helper threads that background bash tasks rely on.
//!
//! `std::thread::spawn` panics when the OS refuses a new thread. These helpers
//! use `std::thread::Builder` instead, so the caller gets an error it can turn
//! into a managed reply while it still owns (and can clean up) the child.

use std::io;

/// Which helper thread is being started. Tests use it to refuse one kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Worker {
    PtyReader,
    PtyWaiter,
    Watch,
}

pub(crate) fn spawn(worker: Worker, run: impl FnOnce() + Send + 'static) -> io::Result<()> {
    #[cfg(test)]
    if REFUSED.with(|refused| refused.get() == Some(worker)) {
        return Err(io::Error::other("injected worker admission failure"));
    }
    let _ = worker;
    std::thread::Builder::new().spawn(run).map(|_| ())
}

#[cfg(test)]
thread_local! {
    static REFUSED: std::cell::Cell<Option<Worker>> = const { std::cell::Cell::new(None) };
}

/// Runs `run` with creation of `worker` threads refused on this test thread.
#[cfg(test)]
pub(crate) fn with_refusal<T>(worker: Worker, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<Worker>);
    impl Drop for Reset {
        fn drop(&mut self) {
            REFUSED.with(|refused| refused.set(self.0));
        }
    }
    let _reset = Reset(REFUSED.with(|refused| refused.replace(Some(worker))));
    run()
}
