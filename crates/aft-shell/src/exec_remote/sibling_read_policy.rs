use crate::protocol::Response;
use std::path::{Path, PathBuf};

/// The bound session's read checks, without triggering a permission ask.
#[doc(hidden)]
pub trait SiblingReadPolicy {
    fn validate_read_path_without_approval(
        &self,
        id: &str,
        session: &str,
        path: &Path,
    ) -> Result<PathBuf, Response>;
}
