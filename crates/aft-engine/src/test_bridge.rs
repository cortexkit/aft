//! Engine test fixtures shared with the daemon and tool-handler tests.

pub mod test_env {
    pub use crate::test_env::*;
}

pub mod test_storage {
    pub use crate::test_storage::*;
}

pub fn with_test_reader<T>(reader: Option<&str>, run: impl FnOnce() -> T) -> T {
    crate::late_result_bus::with_test_reader(reader, run)
}
