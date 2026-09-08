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
/// Non-Windows absolute paths use the same RFC 3986 path-segment encoder so
/// reserved delimiters cannot make an otherwise valid LSP URI unparsable.
pub fn path_to_uri(path: &Path) -> Result<Url, LspError> {
    let raw = path.to_string_lossy();
    let normalized = normalize_windows_path_for_uri(&raw);

    if let Some((server, path)) = split_unc_path(&normalized) {
        let uri = format!(
            "file://{}/{}",
            encode_uri_component(server),
            encode_windows_uri_path(path)
        );
        return Url::parse(&uri).map_err(|_| {
            LspError::NotFound(format!(
                "failed to convert '{}' to file URI",
                path_display(path)
            ))
        });
    }

    if is_windows_drive_path(&normalized) {
        let uri = format!("file:///{}", encode_windows_uri_path(&normalized));
        return Url::parse(&uri).map_err(|_| {
            LspError::NotFound(format!(
                "failed to convert '{}' to file URI",
                path.display()
            ))
        });
    }

    if normalized.starts_with('/') {
        // RFC 3986 brackets are authority delimiters, not path characters.
        // Building the URI ourselves keeps every LSP payload parseable while
        // preserving legal path punctuation such as parentheses.
        let uri = format!("file://{}", encode_uri_path(&normalized));
        return Url::parse(&uri).map_err(|_| {
            LspError::NotFound(format!(
                "failed to convert '{}' to file URI",
                path.display()
            ))
        });
    }

    Err(LspError::NotFound(format!(
        "failed to convert '{}' to file URI",
        path.display()
    )))
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
            path.push_str(&decode_uri_segment(segment)?);
        }
        return Ok(normalize_lookup_path(&PathBuf::from(path)));
    }

    let path = url.path();
    if path.len() >= 4 && path.as_bytes()[0] == b'/' && is_ascii_drive_prefix(&path[1..]) {
        // Decode each '/'-delimited segment separately and rejoin with '\\'.
        // Splitting before decoding (instead of decoding the whole path) keeps
        // an encoded `%2F` inside one segment from becoming a real separator.
        let decoded = path[1..]
            .split('/')
            .map(decode_uri_segment)
            .collect::<Result<Vec<_>, _>>()?
            .join("\\");
        return Ok(normalize_lookup_path(&PathBuf::from(decoded)));
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
    // Normalized like every other LSP-subsystem path (see canonicalize_for_lsp):
    // store keys and lookups must share one canonical form or Windows verbatim
    // spellings silently miss.
    std::fs::canonicalize(path)
        .map(|canonical| crate::inspect::job::normalize_path(&canonical))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Convert an LSP URI to a PathBuf.
pub fn uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    let url = url::Url::parse(uri.as_str()).ok()?;
    url_to_path(&url).ok()
}

fn normalize_windows_path_for_uri(path: &str) -> String {
    crate::windows_path::non_verbatim_path_text(path).unwrap_or_else(|| path.to_string())
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
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
            )
        {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
        encoded
    })
}

fn encode_uri_path(path: &str) -> String {
    path.split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_windows_uri_path(path: &str) -> String {
    encode_uri_path(&path.replace('\\', "/"))
}

/// Percent-decode a single URI path segment into one filesystem path component.
///
/// Decoding is per segment, never over a whole path, so an encoded separator
/// (`%2F` for '/' or `%5C` for '\\') inside a segment is decoded and then
/// rejected rather than silently splitting the path. Whole-path decoding would
/// let a server that echoes our URIs smuggle extra directory levels (or a NUL)
/// into the path we cache diagnostics under. A malformed `%` escape that is not
/// followed by two hex digits is copied through literally, matching how the
/// `url` crate serializes such input.
fn decode_uri_segment(segment: &str) -> Result<String, LspError> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).and_then(hex_value);
            let lo = bytes.get(i + 2).and_then(hex_value);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                decoded.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }

    // Refuse invalid UTF-8 outright: a wrong path key silently eats push
    // diagnostics, so a refused path is safer than a lossily-replaced one.
    let decoded = String::from_utf8(decoded).map_err(|_| {
        LspError::NotFound(format!(
            "URI segment '{segment}' is not valid UTF-8 after percent-decoding"
        ))
    })?;

    if decoded.contains(['/', '\\', '\0']) {
        return Err(LspError::NotFound(format!(
            "URI segment '{segment}' decodes to a path separator or NUL"
        )));
    }

    Ok(decoded)
}

fn hex_value(byte: &u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

    #[test]
    fn windows_extended_volume_path_is_not_misrepresented_as_a_file_uri() {
        let path = Path::new(r"\\?\Volume{1234}\repo\main.rs");
        assert!(path_to_uri(path).is_err());
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

    // The drive-letter and UNC decode branches below run on every platform
    // whenever a drive/UNC-shaped file URI is handed in, so these tests are
    // deliberately NOT cfg(windows)-gated: a regression must fail CI on Linux
    // too, not only on Windows runners.

    // Regression guard for the common case: an already-unescaped ASCII path
    // must decode to byte-identical output (no behavior change from adding
    // percent-decoding).
    #[test]
    fn plain_ascii_drive_uri_is_byte_identical() {
        let url = Url::parse("file:///C:/repo/src/main.rs").expect("url");
        assert_eq!(
            url_to_path(&url).expect("path"),
            PathBuf::from(r"C:\repo\src\main.rs")
        );
    }

    #[test]
    fn drive_uri_with_space_decodes() {
        let url = Url::parse("file:///C:/repo/space%20dir/main.rs").expect("url");
        assert_eq!(
            url_to_path(&url).expect("path"),
            PathBuf::from(r"C:\repo\space dir\main.rs")
        );
    }

    #[test]
    fn drive_uri_with_utf8_segment_decodes() {
        // %C3%A9 is the UTF-8 encoding of 'é'.
        let url = Url::parse("file:///C:/repo/caf%C3%A9/main.rs").expect("url");
        assert_eq!(
            url_to_path(&url).expect("path"),
            PathBuf::from("C:\\repo\\café\\main.rs")
        );
    }

    #[test]
    fn unc_uri_with_space_decodes() {
        let url = Url::parse("file://server/share/space%20dir/file.rs").expect("url");
        assert_eq!(
            url_to_path(&url).expect("path"),
            PathBuf::from(r"\\server\share\space dir\file.rs")
        );
    }

    #[test]
    fn unc_uri_with_utf8_segment_decodes() {
        let url = Url::parse("file://server/share/caf%C3%A9/file.rs").expect("url");
        assert_eq!(
            url_to_path(&url).expect("path"),
            PathBuf::from(r"\\server\share\café\file.rs")
        );
    }

    #[test]
    fn encoded_separator_in_segment_is_rejected() {
        // An encoded '%2F' must not smuggle a '/' into a single path component.
        let drive = Url::parse("file:///C:/repo/a%2Fb/main.rs").expect("url");
        assert!(url_to_path(&drive).is_err());

        let unc = Url::parse("file://server/share/a%2Fb/file.rs").expect("url");
        assert!(url_to_path(&unc).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_framework_route_paths_round_trip_through_lsp_uris() {
        for path in [
            "/tmp/aft/(app)/page.tsx",
            "/tmp/aft/[locationId]/route.ts",
            "/tmp/aft/[[...sign-in]]/page.tsx",
        ] {
            let path = Path::new(path);
            let uri = uri_for_path(path).expect("LSP URI");
            assert!(!uri.as_str().contains(['[', ']']), "URI: {}", uri.as_str());
            assert_eq!(
                uri_to_path(&uri).as_deref(),
                Some(path),
                "URI: {}",
                uri.as_str()
            );
        }
    }

    #[test]
    fn windows_framework_route_paths_round_trip_through_lsp_uris() {
        for path in [
            r"C:\repo\(app)\page.tsx",
            r"C:\repo\[locationId]\route.ts",
            r"C:\repo\[[...sign-in]]\page.tsx",
        ] {
            let path = Path::new(path);
            let uri = uri_for_path(path).expect("LSP URI");
            assert!(!uri.as_str().contains(['[', ']']), "URI: {}", uri.as_str());
            assert_eq!(
                uri_to_path(&uri).as_deref(),
                Some(path),
                "URI: {}",
                uri.as_str()
            );
        }
    }
}
