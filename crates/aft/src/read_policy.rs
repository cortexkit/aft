//! Runtime-supplied path authorization for lower-layer readers.

use crate::protocol::Response;
use std::path::{Path, PathBuf};

/// Storage-path comparison is not read authorization: retained storage roots
/// need alias-aware identity and containment even without a request or session.
pub(crate) trait StoragePathPolicy {
    fn identity(&self, path: &Path) -> PathBuf;
    fn contains(&self, path: &Path, prefix: &Path) -> bool;
}

/// A reader asks for the same path decision as the owning request's runtime.
/// Search-specific approvals are selected by the runtime adapter, not the index.
pub trait ReadPolicy {
    fn validate_read_path(
        &self,
        request: &str,
        session: &str,
        path: &Path,
    ) -> Result<PathBuf, Response>;
    fn root_too_broad(&self) -> bool;
    fn is_home_root(&self) -> bool;
}

impl<T: ReadPolicy + ?Sized> ReadPolicy for std::sync::Arc<T> {
    fn validate_read_path(
        &self,
        request: &str,
        session: &str,
        path: &Path,
    ) -> Result<PathBuf, Response> {
        (**self).validate_read_path(request, session, path)
    }
    fn root_too_broad(&self) -> bool {
        (**self).root_too_broad()
    }
    fn is_home_root(&self) -> bool {
        (**self).is_home_root()
    }
}
