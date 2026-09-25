//! Demotion of structured data files below source code.
//!
//! Large JSON documents (schemas, benchmark results, captured inventories,
//! `*.generated.json` tables) repeat identifiers and prose from the code they
//! describe. They therefore match many query tokens, and often match a quoted
//! phrase more times than the one source line that defines it, so they used to
//! outrank the file an agent was actually looking for. This module decides
//! which candidates count as data for a given query and applies a bounded
//! demotion: data files stay in the result list, just below comparable source.
//!
//! A file counts as data only when its extension is a JSON document format and
//! its name or shape says it is not a hand-edited configuration file:
//! - a generated name (`*.generated.json`, a `generated/` directory, ...), or
//! - at least [`DATA_FILE_MIN_BYTES`] bytes, or
//! - a line of at least [`DATA_FILE_LONG_LINE_CHARS`] characters.
//!
//! Well-known hand-edited manifests (`package.json`, `tsconfig*.json`, dotfile
//! configs such as `.eslintrc.json`) never count as data, whatever their shape.
//!
//! The demotion is switched off for a file when the query asks for it: when
//! the query contains the file's name or stem, or mentions JSON itself. Those
//! queries rank data files exactly as before.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::blocks::LaneCandidate;

/// Files at least this large are treated as data documents rather than
/// hand-written configuration. Hand-edited JSON configs in typical
/// repositories stay well below this size; captured results and schemas do not.
pub const DATA_FILE_MIN_BYTES: u64 = 8 * 1024;

/// A line this long in a JSON file almost always holds embedded content (a
/// captured file body, a serialized list) rather than a hand-edited setting.
pub const DATA_FILE_LONG_LINE_CHARS: usize = 256;

/// How many positions a data candidate moves down inside each scored lane.
///
/// Fusion is reciprocal-rank based, so moving a candidate down by a fixed
/// number of lane positions demotes it by roughly that many results among
/// candidates from the same lane, without dropping it from the list.
pub const DATA_FILE_LANE_DEMOTION: usize = 20;

const DATA_EXTENSIONS: &[&str] = &["json", "jsonl", "ndjson", "geojson"];

/// Query words that ask for JSON data in general rather than for code.
const DATA_QUERY_WORDS: &[&str] = &["json", "jsonl", "ndjson"];

/// Hand-edited manifests that share the JSON extension but describe a project
/// rather than hold data about it.
fn is_hand_edited_manifest(file_name: &str) -> bool {
    file_name.starts_with('.')
        || matches!(
            file_name,
            "package.json" | "composer.json" | "deno.json" | "app.json" | "manifest.json"
        )
        || ((file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
            && file_name.ends_with(".json"))
}

fn has_data_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DATA_EXTENSIONS
                .iter()
                .any(|data| extension.eq_ignore_ascii_case(data))
        })
}

/// Return true when `path` names a JSON-family file whose name or content
/// shape marks it as data rather than source or configuration.
pub fn is_data_file(path: &Path) -> bool {
    if !has_data_extension(path) {
        return false;
    }
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    if is_hand_edited_manifest(&file_name) {
        return false;
    }
    if crate::inspect::path_has_generated_shape(path) {
        return true;
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() >= DATA_FILE_MIN_BYTES {
        return true;
    }
    // Below the size threshold the whole file is small, so reading it to
    // measure its longest line is cheap.
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return false;
    }
    bytes
        .split(|byte| *byte == b'\n')
        .any(|line| line.len() >= DATA_FILE_LONG_LINE_CHARS)
}

/// Return true when the query asks for this particular file or for JSON data
/// in general, so demoting it would work against the query.
pub fn query_asks_for_file(query: &str, path: &Path) -> bool {
    let query = query.to_ascii_lowercase();
    let asks_for_json = query
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| DATA_QUERY_WORDS.contains(&word));
    if asks_for_json {
        return true;
    }
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    if query.contains(&file_name) {
        return true;
    }
    // The stem ("aft.schema" for "aft.schema.json") is enough to name the
    // file. Very short stems ("data", "a") would match unrelated queries.
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|stem| stem.len() >= 5 && query.contains(&stem))
}

/// Per-query classifier with a cache, so a file seen in several lanes is
/// examined on disk once.
pub struct DataFileClassifier<'q> {
    query: &'q str,
    cache: HashMap<PathBuf, bool>,
}

impl<'q> DataFileClassifier<'q> {
    pub fn new(query: &'q str) -> Self {
        Self {
            query,
            cache: HashMap::new(),
        }
    }

    /// Whether `path` should be demoted for this query.
    pub fn demote(&mut self, path: &Path) -> bool {
        if let Some(known) = self.cache.get(path) {
            return *known;
        }
        let demote = is_data_file(path) && !query_asks_for_file(self.query, path);
        self.cache.insert(path.to_path_buf(), demote);
        demote
    }
}

/// Move every demoted candidate [`DATA_FILE_LANE_DEMOTION`] positions down in
/// a lane's canonical order, keeping all other relative order unchanged.
///
/// A demoted candidate lands after the source candidates that were within
/// that many positions below it; nothing is removed.
pub fn demote_data_candidates(
    candidates: Vec<LaneCandidate>,
    mut demote: impl FnMut(&Path) -> bool,
) -> Vec<LaneCandidate> {
    let mut keyed = candidates
        .into_iter()
        .enumerate()
        .map(|(position, candidate)| {
            let demoted = demote(&candidate.path);
            let key = if demoted {
                position + DATA_FILE_LANE_DEMOTION
            } else {
                position
            };
            // On an equal key the source candidate goes first.
            ((key, demoted), candidate)
        })
        .collect::<Vec<_>>();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, candidate)| candidate).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;

    fn write(dir: &Path, relative: &str, content: &str) -> PathBuf {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn large_or_long_line_or_generated_json_is_data() {
        let dir = tempfile::tempdir().unwrap();
        let large = write(
            dir.path(),
            "docs/inventory.json",
            &"{\"k\": 1}\n".repeat(1200),
        );
        let long_line = write(
            dir.path(),
            "fixtures/census.json",
            &format!("{{\n  \"body\": \"{}\"\n}}\n", "x".repeat(500)),
        );
        let generated = write(dir.path(), "src/op-kinds.generated.json", "{\"a\": 1}\n");
        assert!(is_data_file(&large));
        assert!(is_data_file(&long_line));
        assert!(is_data_file(&generated));
    }

    #[test]
    fn source_small_json_and_manifests_are_not_data() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(dir.path(), "src/lib.rs", &"x".repeat(20_000));
        let small = write(
            dir.path(),
            "fixtures/expected.json",
            "{\n  \"ok\": true\n}\n",
        );
        let manifest = write(
            dir.path(),
            "package.json",
            &format!(
                "{{\n  \"scripts\": {{ \"build\": \"{}\" }}\n}}\n",
                "y".repeat(9000)
            ),
        );
        let tsconfig = write(dir.path(), "tsconfig.base.json", &"{}\n".repeat(5000));
        let dotfile = write(dir.path(), ".eslintrc.json", &"{}\n".repeat(5000));
        let jsonc = write(dir.path(), ".cortexkit/aft.jsonc", &"{}\n".repeat(5000));
        for path in [source, small, manifest, tsconfig, dotfile, jsonc] {
            assert!(!is_data_file(&path), "{} must not be data", path.display());
        }
    }

    #[test]
    fn query_naming_the_file_or_json_keeps_it_undemoted() {
        let schema = Path::new("assets/aft.schema.json");
        assert!(query_asks_for_file(
            "aft.schema.json governed manifest",
            schema
        ));
        assert!(query_asks_for_file("where is aft.schema defined", schema));
        assert!(query_asks_for_file(
            "status snapshot json serialization",
            schema
        ));
        assert!(!query_asks_for_file("was not found on PATH", schema));
        assert!(!query_asks_for_file("jsonrpc transport", schema));
    }

    fn candidate(path: &str) -> LaneCandidate {
        LaneCandidate::non_exact(
            path,
            None,
            EvidenceDescriptor::for_non_exact(true, false),
            1.0,
            false,
        )
    }

    #[test]
    fn lane_demotion_moves_data_down_by_a_bounded_amount_and_keeps_it() {
        let mut paths = vec!["data.json".to_string()];
        paths.extend((0..30).map(|index| format!("src/file{index}.rs")));
        let lane = paths.iter().map(|path| candidate(path)).collect::<Vec<_>>();
        let demoted = demote_data_candidates(lane, |path| path == Path::new("data.json"));
        assert_eq!(demoted.len(), 31);
        let position = demoted
            .iter()
            .position(|candidate| candidate.path == Path::new("data.json"))
            .unwrap();
        assert_eq!(position, DATA_FILE_LANE_DEMOTION);
        assert_eq!(demoted[0].path, Path::new("src/file0.rs"));
    }

    #[test]
    fn lane_demotion_without_data_keeps_order() {
        let lane = vec![candidate("b.rs"), candidate("a.rs")];
        let kept = demote_data_candidates(lane.clone(), |_| false);
        assert_eq!(kept, lane);
    }
}
