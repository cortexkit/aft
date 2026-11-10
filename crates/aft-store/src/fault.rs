//! Crash injection for tests.
//!
//! Every step that changes the store's files calls [`point`] first. A test
//! arms a countdown with [`fail_after`]; the step it reaches fails with an
//! injected error and every later step on that thread fails too, as if the
//! process had died there. The test then reopens the store from disk and
//! checks what survived. Outside tests, [`point`] does nothing.

use std::io;

#[cfg(test)]
thread_local! {
    static COUNTDOWN: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One step that changes a file. Fails when an armed countdown reaches it.
#[cfg_attr(not(test), inline(always))]
pub(crate) fn point(step: &'static str) -> io::Result<()> {
    #[cfg(test)]
    {
        STEPS.set(STEPS.get() + 1);
        match COUNTDOWN.get() {
            Some(0) => {
                FIRED.set(true);
                return Err(io::Error::other(format!("injected crash at {step}")));
            }
            Some(left) => COUNTDOWN.set(Some(left - 1)),
            None => {}
        }
    }
    #[cfg(not(test))]
    let _ = step;
    Ok(())
}

/// Lets `steps` more steps succeed on this thread, then fails every one.
#[cfg(test)]
pub(crate) fn fail_after(steps: u64) {
    COUNTDOWN.set(Some(steps));
    FIRED.set(false);
}

/// Whether an injected failure happened since the countdown was armed.
#[cfg(test)]
pub(crate) fn fired() -> bool {
    FIRED.get()
}

/// Stops injecting failures on this thread.
#[cfg(test)]
pub(crate) fn disarm() {
    COUNTDOWN.set(None);
}

/// How many steps this thread has passed since the last reset.
#[cfg(test)]
pub(crate) fn steps() -> u64 {
    STEPS.get()
}

#[cfg(test)]
pub(crate) fn reset_steps() {
    STEPS.set(0);
}
