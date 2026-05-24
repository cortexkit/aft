use lsp_types::{Position, Range, TextDocumentIdentifier, TextDocumentPositionParams};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use url::Url;

use super::LspError;

/// Convert AFT 1-based (or compatibility 0-based) line/column values to an LSP 0-based position.
pub fn to_lsp_position(line: u32, column: u32) -> Position {
    Position::new(line.saturating_sub(1), column.saturating_sub(1))
}

/// Convert an LSP 0-based position to AFT 1-based line/column values.
pub fn from_lsp_position(position: &Position) -> (u32, u32) {
    (position.line + 1, position.character + 1)
}

/// Convert an LSP Range to a 1-based AFT range as a JSON-friendly tuple.
pub fn lsp_range_to_aft(range: &Range) -> (u32, u32, u32, u32) {
    let (start_line, start_column) = from_lsp_position(&range.start);
    let (end_line, end_column) = from_lsp_position(&range.end);
    (start_line, start_column, end_line, end_column)
}

/// Build a TextDocumentPositionParams from a file path and 1-based line/column.
pub fn text_document_position(
    file_path: &Path,
    line: u32,
    column: u32,
) -> Result<TextDocumentPositionParams, LspError> {
    let uri = uri_for_path(file_path)?;
    Ok(TextDocumentPositionParams {
        text_document: TextDocumentIdentifier::new(uri),
        position: to_lsp_position(line, column),
    })
}

/// Backwards-compatible alias for existing internal call sites.
pub fn build_text_document_position(
    file_path: &Path,
    line: u32,
    column: u32,
) -> Result<TextDocumentPositionParams, LspError> {
    text_document_position(file_path, line, column)
}

/// Convert a filesystem path to a `file://` URL suitable for LSP payloads.
///
/// This is intentionally Windows-aware even when tests run on Unix. Windows
/// extended-length paths from `std::fs::canonicalize` are normalized before URI
/// conversion:
/// - `C:\dir\file.rs` -> `file:///C:/dir/file.rs`
/// - `\\?\C:\dir\file.rs` -> `file:///C:/dir/file.rs`
/// - `\\?\UNC\server\share\file.rs` -> `file://server/share/file.rs`
///
/// Non-Windows paths use `Url::from_file_path` unchanged.
pub fn path_to_uri(path: &Path) -> Result<Url, LspError> {
    let raw = path.to_string_lossy();
    let normalized = normalize_windows_path_for_uri(&raw);

    if let Some((server, path)) = split_unc_path(&normalized) {
        let uri = format!(
            "file://{}/{}",
            encode_uri_component(server),
            encode_uri_path(path)
        );
        return Url::parse(&uri).map_err(|_| {
            LspError::NotFound(format!(
                "failed to convert '{}' to file URI",
                path_display(path)
            ))
        });
    }

    if is_windows_drive_path(&normalized) {
        let uri = format!(
            "file:///{}",
            encode_uri_path(&normalized.replace('\\', "/"))
        );
        return Url::parse(&uri).map_err(|_| {
            LspError::NotFound(format!(
                "failed to convert '{}' to file URI",
                path.display()
            ))
        });
    }

    Url::from_file_path(path).map_err(|_| {
        LspError::NotFound(format!(
            "failed to convert '{}' to file URI",
            path.display()
        ))
    })
}

/// Convert a `file://` URL back into a filesystem path.
///
/// Drive-letter and UNC file URIs are decoded explicitly so Windows paths round
/// trip correctly in cross-platform tests. Unix file URIs delegate to
/// `Url::to_file_path` and then receive the same lookup normalization used by
/// the diagnostics store.
pub fn url_to_path(url: &Url) -> Result<PathBuf, LspError> {
    if url.scheme() != "file" {
        return Err(LspError::NotFound(format!(
            "expected file URI, got '{}'",
            url
        )));
    }

    if let Some(host) = url.host_str() {
        let mut path = String::from(r"\\");
        path.push_str(host);
        for segment in url.path_segments().into_iter().flatten() {
            if segment.is_empty() {
                continue;
            }
            path.push('\\');
            path.push_str(segment);
        }
        return Ok(PathBuf::from(path));
    }

    let path = url.path();
    if path.len() >= 4 && path.as_bytes()[0] == b'/' && is_ascii_drive_prefix(&path[1..]) {
        return Ok(PathBuf::from(path[1..].replace('/', "\\")));
    }

    url.to_file_path()
        .map(|path| normalize_lookup_path(&path))
        .map_err(|_| LspError::NotFound(format!("failed to convert '{}' to path", url)))
}

/// Convert a file path to an LSP URI.
pub fn uri_for_path(path: &Path) -> Result<lsp_types::Uri, LspError> {
    let url = path_to_uri(path)?;
    lsp_types::Uri::from_str(url.as_str()).map_err(|_| {
        LspError::NotFound(format!("failed to parse file URI for '{}'", path.display()))
    })
}

fn normalize_lookup_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Convert an LSP URI to a PathBuf.
pub fn uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    let url = url::Url::parse(uri.as_str()).ok()?;
    url_to_path(&url).ok()
}

fn normalize_windows_path_for_uri(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", stripped)
    } else if let Some(stripped) = path.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        path.to_string()
    }
}

fn split_unc_path(path: &str) -> Option<(&str, &str)> {
    let stripped = path.strip_prefix(r"\\")?;
    let (server, rest) = stripped.split_once(['\\', '/'])?;
    if server.is_empty() || rest.is_empty() {
        return None;
    }
    Some((server, rest))
}

fn is_windows_drive_path(path: &str) -> bool {
    is_ascii_drive_prefix(path)
        && path
            .as_bytes()
            .get(2)
            .is_some_and(|separator| *separator == b'\\' || *separator == b'/')
}

fn is_ascii_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn encode_uri_component(value: &str) -> String {
    value.bytes().fold(String::new(), |mut encoded, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
        encoded
    })
}

fn encode_uri_path(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn path_display(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_drive_path_to_uri_round_trips() {
        let path = Path::new(r"C:\repo\src\main.rs");
        let uri = path_to_uri(path).expect("uri");

        assert_eq!(uri.as_str(), "file:///C:/repo/src/main.rs");
        assert_eq!(
            url_to_path(&uri).expect("path"),
            PathBuf::from(r"C:\repo\src\main.rs")
        );
    }

    #[test]
    fn windows_extended_drive_path_to_uri_strips_prefix() {
        let path = Path::new(r"\\?\C:\repo\src\main.rs");
        let uri = path_to_uri(path).expect("uri");

        assert_eq!(uri.as_str(), "file:///C:/repo/src/main.rs");
        assert_eq!(
            url_to_path(&uri).expect("path"),
            PathBuf::from(r"C:\repo\src\main.rs")
        );
    }

    #[test]
    fn windows_extended_unc_path_preserves_unc_host_and_share() {
        let path = Path::new(r"\\?\UNC\server\share\dir\file.rs");
        let uri = path_to_uri(path).expect("uri");

        assert_eq!(uri.as_str(), "file://server/share/dir/file.rs");
        assert_eq!(
            url_to_path(&uri).expect("path"),
            PathBuf::from(r"\\server\share\dir\file.rs")
        );
    }

    // Unix-absolute path syntax; not a valid Windows absolute path so the
    // round-trip can only be verified on Unix-like targets.
    #[cfg(unix)]
    #[test]
    fn unix_path_to_uri_round_trips() {
        let path = Path::new("/tmp/aft-lsp-position.rs");
        let uri = path_to_uri(path).expect("uri");

        assert_eq!(uri.as_str(), "file:///tmp/aft-lsp-position.rs");
        assert_eq!(url_to_path(&uri).expect("path"), PathBuf::from(path));
    }
}
