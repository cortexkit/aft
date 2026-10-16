//! Reading a scope file as source text for the Tier-2 tree-sitter scanners
//! (complexity, duplicates), and reporting the files that could not be read
//! that way.
//!
//! A single file whose bytes are not valid UTF-8 (a GBK- or Latin-1-encoded
//! data file, for example) used to abort the whole category build. Such a file
//! is now a per-file skip: it contributes nothing, its contribution records the
//! reason, and the category reports it as a named gap
//! (`complete: false`, `skipped_files: [{file, reason}]`). The skip is stored
//! as the file's cached contribution, so it is not re-read until the file
//! changes, and a changed file that is now valid UTF-8 is analyzed normally.

use std::path::Path;
#[cfg(debug_assertions)]
use std::path::PathBuf;

use serde_json::{json, Value};

/// Reason recorded for a file whose bytes are not valid UTF-8.
pub(crate) const NOT_VALID_UTF8: &str = "not valid UTF-8";

/// Most skipped files named in a project-wide aggregate. The total is always
/// reported in `skipped_files_count`, so a capped list is never mistaken for
/// the whole set.
pub(crate) const SKIPPED_FILES_LIMIT: usize = 20;

/// Outcome of reading one scope file as source text.
#[derive(Debug)]
pub(crate) enum SourceText {
    /// The file decoded as UTF-8.
    Text(String),
    /// The file was read but is not valid UTF-8; the scanner skips it.
    NotUtf8,
    /// The file disappeared or is excluded from project source (for example,
    /// internal storage). No contribution, exactly as the next walk will see it.
    Vanished,
}

/// Read `path` as UTF-8 source text. Errors other than "not found" (for
/// example permission denied) are returned unchanged so each scanner keeps
/// its existing contract for them.
pub(crate) fn read_source_text(path: &Path) -> std::io::Result<SourceText> {
    if crate::project_content::is_excluded(path) {
        return Ok(SourceText::Vanished);
    }
    #[cfg(debug_assertions)]
    bump_source_read_count(path);
    match crate::project_content::read(path) {
        Ok(bytes) => Ok(match String::from_utf8(bytes) {
            Ok(text) => SourceText::Text(text),
            Err(_) => SourceText::NotUtf8,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SourceText::Vanished),
        Err(error) => Err(error),
    }
}

/// Record the named per-file gap on a category aggregate. Does nothing when
/// no file was skipped, so aggregates without skips are unchanged.
///
/// `skipped` holds `(file, reason)` pairs. The list is sorted by file; with a
/// `drill_down_limit` (the project-wide rollup) at most
/// [`SKIPPED_FILES_LIMIT`] entries are named, and `skipped_files_count` always
/// carries the full total. Scoped rollups pass no limit so scope filtering
/// sees every skipped file.
pub(crate) fn write_skipped_files(
    aggregate: &mut Value,
    mut skipped: Vec<(String, String)>,
    drill_down_limit: Option<usize>,
) {
    if skipped.is_empty() {
        return;
    }
    skipped.sort();
    let total = skipped.len();
    let named = if drill_down_limit.is_some() {
        SKIPPED_FILES_LIMIT
    } else {
        usize::MAX
    };
    let rows = skipped
        .into_iter()
        .take(named)
        .map(|(file, reason)| json!({ "file": file, "reason": reason }))
        .collect::<Vec<_>>();
    aggregate["complete"] = Value::Bool(false);
    aggregate["skipped_files"] = Value::Array(rows);
    aggregate["skipped_files_count"] = json!(total);
}

#[cfg(debug_assertions)]
static SOURCE_READS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<PathBuf, usize>>,
> = std::sync::OnceLock::new();

#[cfg(debug_assertions)]
fn bump_source_read_count(path: &Path) {
    let reads = SOURCE_READS.get_or_init(Default::default);
    if let Ok(mut reads) = reads.lock() {
        *reads.entry(path.to_path_buf()).or_default() += 1;
    }
}

/// How many times the Tier-2 scanners read `path` as source text in this
/// process. Debug builds only; lets tests prove a file was or was not opened.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn source_read_count_for_debug(path: &Path) -> usize {
    SOURCE_READS
        .get()
        .and_then(|reads| reads.lock().ok().map(|reads| reads.get(path).copied()))
        .flatten()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_is_a_skip_and_a_missing_file_is_vanished() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gbk = dir.path().join("gbk.json");
        std::fs::write(&gbk, b"{\"k\":\"\xb9\xe6\xce\"}").expect("write gbk");
        let utf8 = dir.path().join("ok.ts");
        std::fs::write(&utf8, "export const k = 1;\n").expect("write utf8");

        assert!(matches!(
            read_source_text(&gbk).expect("read gbk"),
            SourceText::NotUtf8
        ));
        assert!(matches!(
            read_source_text(&utf8).expect("read utf8"),
            SourceText::Text(text) if text == "export const k = 1;\n"
        ));
        assert!(matches!(
            read_source_text(&dir.path().join("gone.ts")).expect("read missing"),
            SourceText::Vanished
        ));
    }

    #[test]
    fn skipped_files_are_sorted_capped_and_counted() {
        let skipped = (0..SKIPPED_FILES_LIMIT + 5)
            .rev()
            .map(|index| (format!("f{index:03}.ts"), NOT_VALID_UTF8.to_string()))
            .collect::<Vec<_>>();
        let mut capped = json!({ "count": 0 });
        write_skipped_files(&mut capped, skipped.clone(), Some(100));
        assert_eq!(capped["complete"], json!(false));
        assert_eq!(
            capped["skipped_files_count"],
            json!(SKIPPED_FILES_LIMIT + 5)
        );
        let rows = capped["skipped_files"].as_array().expect("rows");
        assert_eq!(rows.len(), SKIPPED_FILES_LIMIT);
        assert_eq!(
            rows[0],
            json!({ "file": "f000.ts", "reason": NOT_VALID_UTF8 })
        );

        let mut uncapped = json!({ "count": 0 });
        write_skipped_files(&mut uncapped, skipped, None);
        assert_eq!(
            uncapped["skipped_files"].as_array().map(Vec::len),
            Some(SKIPPED_FILES_LIMIT + 5)
        );

        let mut untouched = json!({ "count": 0 });
        write_skipped_files(&mut untouched, Vec::new(), Some(100));
        assert_eq!(untouched, json!({ "count": 0 }));
    }
}
