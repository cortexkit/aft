//! In-band views delivery acknowledgments, distinct from command-report flushes.
use std::path::Path;

const PREFIX: &str = ".aft-cookie-";

pub(crate) fn is_cookie_path(path: &Path) -> bool {
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
