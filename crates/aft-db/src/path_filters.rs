#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct PathFilters {
    includes: Option<GlobSet>,
    excludes: Option<GlobSet>,
}

#[doc(hidden)]
pub fn build_path_filters(include: &[String], exclude: &[String]) -> Result<PathFilters, String> {
    Ok(PathFilters {
        includes: build_globset(include)?,
        excludes: build_globset(exclude)?,
    })
}

pub(crate) fn relative_to_root(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

impl PathFilters {
    #[doc(hidden)]
    pub fn matches(&self, root: &Path, path: &Path) -> bool {
        if !crate::bash_background::registry::safe_task_entry(path) {
            return false;
        }
        if self.includes.is_none() && self.excludes.is_none() {
            return true;
        }
        #[cfg(any(test, feature = "test-support"))]
        audit_record(|work| work.filter_keys += 1);
        // Compare normalized copies: the search root arrives non-verbatim while
        // indexed entries may carry the Windows `\\?\` form, and a raw
        // strip_prefix between the two forms fails, which read as "no match".
        let relative = to_glob_path(&relative_to_root(
            &normalize_path(root),
            &normalize_path(path),
        ));
        if self
            .includes
            .as_ref()
            .is_some_and(|includes| !includes.is_match(&relative))
        {
            return false;
        }
        if self
            .excludes
            .as_ref()
            .is_some_and(|excludes| excludes.is_match(&relative))
        {
            return false;
        }
        true
    }
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| error.to_string())?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

fn to_glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
