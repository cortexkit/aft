use std::path::{Component, Path, PathBuf};

const OPERATION_NOT_PERMITTED: &str = "Operation not permitted";

pub(crate) fn source_read_only_note<L: AsRef<str>>(
    source_read_only: bool,
    failed: bool,
    error_lines: impl Iterator<Item = L>,
    cwd: &Path,
    project_root: &Path,
) -> Option<String> {
    if !source_read_only || !failed {
        return None;
    }

    // Inspect only permission-error lines, without retaining or shortening the command output.
    for line in error_lines
        .filter(|line| line.as_ref().contains(OPERATION_NOT_PERMITTED))
        .take(1_000)
    {
        for candidate in path_candidates(line.as_ref()) {
            if let Some(path) = checkout_relative_path(candidate, cwd, project_root) {
                return Some(format!(
                    "note: the build server keeps tracked files read-only during a job, so the write to {path} was refused (Operation not permitted). Edit files on this machine; the job's copy is discarded."
                ));
            }
        }
    }
    None
}

fn path_candidates(line: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = rest.as_bytes()[start] as char;
        rest = &rest[start + 1..];
        let Some(end) = rest.find(quote) else {
            break;
        };
        candidates.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    if candidates.is_empty() {
        if let Some((prefix, _)) = line.split_once(": Operation not permitted") {
            // BSD diagnostics and shell redirects put the unquoted path after the last prefix.
            candidates.push(prefix.rsplit(": ").next().unwrap_or(prefix).trim());
        }
    }
    candidates
}

fn checkout_relative_path(candidate: &str, cwd: &Path, project_root: &Path) -> Option<String> {
    if candidate.is_empty() {
        return None;
    }
    let path = Path::new(candidate);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let absolute = normalize_path(&absolute);
    let root = normalize_path(project_root);
    let relative = absolute.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runon_read_only_note_scan_stops_at_the_thousandth_matching_line() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().parent().unwrap().join("outside.rs");
        let outside_error = format!("'{}': Operation not permitted", outside.display());
        let errors = std::iter::repeat_n(outside_error.as_str(), 1_000)
            .chain(std::iter::once("'src/x.rs': Operation not permitted"));
        let mut scanned = 0;
        let note = source_read_only_note(
            true,
            true,
            errors.inspect(|_| scanned += 1),
            root.path(),
            root.path(),
        );
        assert_eq!(scanned, 1_000);
        assert!(note.is_none());
    }

    #[test]
    fn runon_read_only_note_skips_nonmatching_lines_and_outside_candidates() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().parent().unwrap().join("outside.rs");
        let error = format!(
            "cp: '{}' to 'src/x.rs': Operation not permitted",
            outside.display()
        );
        let errors =
            std::iter::repeat_n("ordinary output", 1_001).chain(std::iter::once(error.as_str()));
        let note = source_read_only_note(true, true, errors, root.path(), root.path()).unwrap();
        assert!(note.contains("write to src/x.rs was refused"), "{note}");
    }
}
