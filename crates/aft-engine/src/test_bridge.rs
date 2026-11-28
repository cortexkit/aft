//! Engine test fixtures shared with the daemon and tool-handler tests.

#[cfg(feature = "test-support")]
pub mod bash_remote;
#[cfg(feature = "test-support")]
pub mod display_data;
#[cfg(feature = "test-support")]
pub mod search_misc;

pub mod test_env {
    pub use crate::test_env::*;
}

pub mod test_storage {
    pub use crate::test_storage::*;

    /// Opt into the unit-test storage default for the App built by `make`.
    pub fn with_isolated_context<T>(make: impl FnOnce() -> T) -> T {
        crate::test_storage::with_isolated_context(make)
    }
}
