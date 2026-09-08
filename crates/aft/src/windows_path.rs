//! Windows extended-length path normalization.
//!
//! `std::fs::canonicalize` returns paths in the extended-length namespace on
//! Windows. Only DOS-drive and UNC names have a safe non-verbatim spelling;
//! other namespaces (for example `\\?\Volume{GUID}\`) must retain their prefix.

use std::path::{Path, PathBuf};

/// Normalize a Windows path for comparisons and Win32 APIs.
///
/// Valid DOS and UNC extended-length paths lose their verbatim prefix. Other
/// namespaces remain untouched because they cannot be represented safely
/// without that prefix. Separators and drive letter casing are also normalized.
pub fn normalize_windows_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy().replace('/', "\\");
    let mut normalized = non_verbatim_path_text(&raw).unwrap_or(raw);
    if normalized.as_bytes().get(1) == Some(&b':') {
        let drive = normalized.as_bytes()[0];
        if drive.is_ascii_lowercase() {
            normalized.replace_range(0..1, &(drive as char).to_ascii_uppercase().to_string());
        }
    }
    PathBuf::from(normalized)
}

/// String form of the strict verbatim-prefix conversion, for APIs that require a command line
/// or URI rather than a [`Path`].
pub fn non_verbatim_path_text(path: &str) -> Option<String> {
    for prefix in [r"\\?\UNC\", r"\\??\UNC\", r"\??\UNC\"] {
        if let Some(tail) = strip_ascii_prefix(path, prefix) {
            if is_safe_unc_tail(tail) {
                return Some(format!(r"\\{tail}"));
            }
            return None;
        }
    }

    for prefix in [r"\\?\", r"\\??\", r"\??\"] {
        if let Some(tail) = strip_ascii_prefix(path, prefix) {
            let bytes = tail.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/')
                && !has_dot_component(tail)
            {
                return Some(tail.to_string());
            }
            return None;
        }
    }

    None
}

fn is_safe_unc_tail(tail: &str) -> bool {
    let mut components = tail.split(['\\', '/']);
    components.next().is_some_and(|server| !server.is_empty())
        && components.next().is_some_and(|share| !share.is_empty())
        && !has_dot_component(tail)
}

fn has_dot_component(path: &str) -> bool {
    path.split(['\\', '/'])
        .any(|component| matches!(component, "." | ".."))
}

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        value.get(prefix.len()..)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{non_verbatim_path_text, normalize_windows_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn converts_only_valid_dos_and_unc_verbatim_paths() {
        assert_eq!(
            non_verbatim_path_text(r"\\?\C:\cache\server.cmd"),
            Some(r"C:\cache\server.cmd".to_string())
        );
        assert_eq!(
            non_verbatim_path_text(r"\\?\unc\host\share\server.cmd"),
            Some(r"\\host\share\server.cmd".to_string())
        );
        assert_eq!(
            normalize_windows_path(Path::new(r"\\??\d:\repo")),
            PathBuf::from(r"D:\repo")
        );
    }

    #[test]
    fn preserves_unsupported_or_malformed_verbatim_namespaces() {
        for path in [
            r"\\?\Volume{1234}\server.cmd",
            r"\\?\UNC\host",
            r"\\?\UNC\\host\share",
            r"\\??\UNC\\host\share",
            r"\\?\UNC\host\share\..\file",
            r"\\?\C:\repo\..\other",
            r"\\?\C:relative",
            r"\\?\relative",
            r"C:\ordinary\path",
        ] {
            assert_eq!(non_verbatim_path_text(path), None, "{path}");
        }
    }
}
