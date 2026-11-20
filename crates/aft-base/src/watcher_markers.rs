//! Filename protocols reserved for watcher acknowledgments, not project content.

use std::path::Path;

pub const MARKER_PREFIX: &str = ".aft-flush-";
pub const GIT_MARKER_PREFIX: &str = "aft-flush-";
const PREFIX: &str = ".aft-cookie-";

pub fn is_cookie_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(PREFIX))
        .and_then(|suffix| suffix.split_once('-'))
        .is_some_and(|(pid, nonce)| {
            !pid.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && !nonce.is_empty()
                && nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

pub fn is_marker_path(path: &Path) -> bool {
    // Project content excludes both acknowledgment protocols. Command-completion
    // intake uses only flush markers so index-delivery cookies still reach their
    // ordered event queue.
    is_cookie_path(path) || is_flush_marker_path(path)
}

pub fn is_flush_marker_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let suffix = name.strip_prefix(MARKER_PREFIX).or_else(|| {
        path.parent()
            .and_then(Path::file_name)
            .filter(|parent| *parent == ".git")
            .and_then(|_| name.strip_prefix(GIT_MARKER_PREFIX))
    });
    suffix
        .and_then(|suffix| suffix.split_once('-'))
        .is_some_and(|(pid, number)| {
            !pid.is_empty()
                && !number.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_marker_names_require_valid_suffixes_and_git_parent() {
        for name in [
            ".aft-flush-12-3",
            ".git/aft-flush-12-3",
            ".aft-cookie-12-a-b",
        ] {
            assert!(is_marker_path(Path::new(name)), "{name}");
        }
        for name in [
            "aft-flush-12-3",
            "other/aft-flush-12-3",
            ".aft-flush--3",
            ".aft-flush-12-x",
            ".aft-cookie-12-",
            ".aft-cookie-x-ok",
        ] {
            assert!(!is_marker_path(Path::new(name)), "{name}");
        }
    }
}
