use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::{normalize_path, path_error_response, resolve_with_existing_ancestors};
use super::{AppContext, PathRestrictionContext, ReadPathPurpose};
use crate::protocol::Response;

/// Check each hop's target against the root, even if the full chain is broken.
pub(super) fn iterative_follow_chain(
    req_id: &str,
    original_path: &Path,
    start: &Path,
    resolved_root: &Path,
    resolve_missing: impl Fn(&Path) -> PathBuf,
) -> Result<(), Response> {
    let mut link = start.to_path_buf();
    let mut depth = 0usize;

    loop {
        if depth > 40 {
            return Err(path_error_response(req_id, original_path, resolved_root));
        }

        let target = fs::read_link(&link)
            .map_err(|_| path_error_response(req_id, original_path, resolved_root))?;
        let resolved_target = if target.is_absolute() {
            normalize_path(&target)
        } else {
            let parent = link.parent().unwrap_or_else(|| Path::new(""));
            normalize_path(&parent.join(&target))
        };
        let canonical_target = fs::canonicalize(&resolved_target)
            .unwrap_or_else(|_| resolve_missing(&resolved_target));

        if !canonical_target.starts_with(resolved_root)
            && !resolved_target.starts_with(resolved_root)
        {
            return Err(path_error_response(req_id, original_path, resolved_root));
        }

        match fs::symlink_metadata(&resolved_target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                link = resolved_target;
                depth += 1;
            }
            _ => break,
        }
    }

    Ok(())
}

impl AppContext {
    /// Recognize an in-root dangling link when a write was denied because the
    /// path used another spelling, such as `/var` or a Windows short name.
    /// This check only selects a refusal; it never returns a writable path.
    #[doc(hidden)]
    pub fn is_restricted_dangling_write_target(&self, req_id: &str, path: &Path) -> bool {
        let _io = crate::bounded_io::enter_mutation();
        let Ok(Some(PathRestrictionContext {
            resolved_root,
            path_for_resolution,
            ..
        })) = self.path_restriction_context(req_id, path)
        else {
            return false;
        };
        let id = req_id.to_owned();
        let Ok(Some(link)) = crate::bounded_io::run(&path_for_resolution, None, move |path| {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Ok(None);
            };
            // Resolve the directory (including Windows short names), not the
            // link. A missing target cannot supply the link's location.
            let Ok(parent) = fs::canonicalize(parent) else {
                return Ok(None);
            };
            if !parent.starts_with(&resolved_root) {
                return Ok(None);
            }
            let link = parent.join(name);
            if !fs::metadata(&link).is_err_and(|error| error.kind() == ErrorKind::NotFound)
                || !link.is_symlink()
            {
                return Ok(None);
            }
            // A missing target can also use the uncanonicalized spelling of
            // the root. Resolve its existing ancestors before comparing it.
            Ok(iterative_follow_chain(
                &id,
                &path,
                &link,
                &resolved_root,
                resolve_with_existing_ancestors,
            )
            .ok()
            .map(|()| link))
        }) else {
            return false;
        };

        self.check_path_access(
            req_id,
            path,
            &link,
            None,
            ReadPathPurpose::File,
            true,
            false,
        )
        .is_ok()
    }
}
