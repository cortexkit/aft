//! The "not found, nearest names" answer for identifier queries.
//!
//! An agent that searches for a name it guessed (a helper it expects to exist,
//! a function renamed since it last looked) gets nothing useful from a fuzzy
//! ranking: every result is a file that merely shares some letters with the
//! guess, and nothing says the name itself is absent. When the whole query is
//! one code identifier and the index proves it occurs nowhere, the reply says
//! so and lists the closest names that do exist, each with the file and line
//! where it is declared or first used.
//!
//! The names come from the files the lexical lane ranks highest for the query:
//! a name close to the query shares most of its trigrams, so those files hold
//! the likely candidates. The scan is bounded by file count, file size and the
//! number of identifier occurrences read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::exact_lane;
use super::snippet_bounds;

/// Maximum number of nearest names listed in a not-found answer.
pub const NEAREST_NAME_LIMIT: usize = 5;

/// Maximum number of files, taken in the lexical lane's order, that the
/// nearest-name scan reads.
pub const NEAREST_NAME_FILE_LIMIT: usize = 50;

/// Files larger than this are skipped, so one large file cannot use up the
/// scan's time; the names an agent misremembers are declared in ordinary
/// source files.
const NEAREST_NAME_MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Most identifier occurrences read across all files before the scan stops.
const NEAREST_NAME_MAX_OCCURRENCES: usize = 500_000;

/// Shortest name considered; shorter tokens are keywords and loop variables.
const NEAREST_NAME_MIN_LEN: usize = 3;

/// Minimum similarity, in thousandths, for a name to be listed; see
/// [`similarity`] for how it is measured.
const NEAREST_NAME_MIN_SIMILARITY: u32 = 500;

static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z_$][A-Za-z0-9_$]*").expect("name pattern compiles"));

/// One name close to the query, with where it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NearestName {
    pub name: String,
    pub path: PathBuf,
    /// 0-based line of the declaration, or of the first occurrence when the
    /// scanned files hold no declaration of the name.
    pub line: u32,
    /// That line, windowed around the name for display.
    pub line_text: String,
}

/// A reply for an identifier that occurs nowhere in the searched project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotFoundAnswer {
    /// The identifier as the user typed it.
    pub query: String,
    pub names: Vec<NearestName>,
}

impl NotFoundAnswer {
    /// The reply text, with paths shown relative to `display_root`.
    pub fn render(&self, display_root: &Path) -> String {
        let display_root = super::result_display_root(display_root);
        if self.names.is_empty() {
            return format!(
                "`{}` not found in this project, and no similar names were found.",
                self.query
            );
        }
        let names = self
            .names
            .iter()
            .map(|name| {
                let path = name.path.strip_prefix(display_root).unwrap_or(&name.path);
                format!(
                    "`{}` ({}:{})",
                    name.name,
                    path.display(),
                    name.line.saturating_add(1)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "`{}` not found in this project. Nearest names: {names}",
            self.query
        )
    }
}

/// The identifiers an identifier query looks up, or `None` when the query is
/// not one plain or dotted code name.
///
/// `running_tasks` looks up itself. `ctx.set_harness` looks up the whole
/// dotted name and its last segment, `set_harness`, because the exact lane
/// also searches for the member's declaration; the receiver (`ctx`) is the
/// caller's variable, not something the query asks to find. The last entry is
/// the name nearest names are measured against. Names shorter than three
/// characters are not checked, because the trigram index cannot tell whether
/// they occur.
pub fn looked_up_identifiers(exact_input: &str) -> Option<Vec<String>> {
    let phrase = exact_lane::exact_phrase(exact_input);
    let segments = phrase.split('.').collect::<Vec<_>>();
    let is_name = |segment: &str| {
        segment
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '$')
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
    };
    if !segments.iter().all(|segment| is_name(segment)) {
        return None;
    }
    let last = segments[segments.len() - 1];
    if last.len() < NEAREST_NAME_MIN_LEN {
        return None;
    }
    let mut identifiers = vec![phrase.to_string()];
    if segments.len() > 1 {
        identifiers.push(last.to_string());
    }
    Some(identifiers)
}

/// Up to [`NEAREST_NAME_LIMIT`] names from `files` closest to `target`,
/// most similar first. `files` is read in order and the scan stops at
/// [`NEAREST_NAME_FILE_LIMIT`] files or [`NEAREST_NAME_MAX_OCCURRENCES`]
/// identifier occurrences, whichever comes first.
pub fn nearest_names<'a>(
    target: &str,
    files: impl IntoIterator<Item = &'a Path>,
) -> Vec<NearestName> {
    let target_lower = target.to_ascii_lowercase();
    let target_words = name_words(target);
    let mut locations: HashMap<String, (bool, NearestName)> = HashMap::new();
    let mut occurrences = 0usize;

    let request_files = super::request_files::current();
    for path in files.into_iter().take(NEAREST_NAME_FILE_LIMIT) {
        if occurrences >= NEAREST_NAME_MAX_OCCURRENCES {
            break;
        }
        // A file this search request already read is scanned from those
        // bytes rather than read again.
        let file = match request_files.as_ref().and_then(|files| files.kept(path)) {
            Some(file) if file.bytes().len() as u64 > NEAREST_NAME_MAX_FILE_BYTES => continue,
            Some(file) => file,
            None => {
                let Ok(metadata) = std::fs::metadata(path) else {
                    continue;
                };
                if metadata.len() > NEAREST_NAME_MAX_FILE_BYTES {
                    continue;
                }
                let Ok(fetched) =
                    super::request_files::read_through(request_files.as_deref(), path, |path| {
                        crate::large_read::counted(
                            crate::large_read::Site::NearestNames,
                            super::request_files::read_whole_file(path),
                        )
                    })
                else {
                    continue;
                };
                fetched.file
            }
        };
        let Some(text) = file.utf8() else {
            continue;
        };

        // Declarations first, so a name's listed location is where it is
        // defined when the scanned files define it.
        if exact_lane::is_definition_source(path) {
            for (name, range) in exact_lane::scan_symbols_in_text(&text) {
                if name.len() < NEAREST_NAME_MIN_LEN {
                    continue;
                }
                let replace = locations.get(&name).is_none_or(|(declared, _)| !*declared);
                if replace {
                    let (line, line_text) = line_of_offset(&text, range.start, &name);
                    locations.insert(
                        name.clone(),
                        (
                            true,
                            NearestName {
                                name,
                                path: path.to_path_buf(),
                                line,
                                line_text,
                            },
                        ),
                    );
                }
            }
        }

        for (line_index, line) in text.lines().enumerate() {
            for found in NAME_RE.find_iter(line) {
                occurrences += 1;
                let name = found.as_str();
                if name.len() < NEAREST_NAME_MIN_LEN || locations.contains_key(name) {
                    continue;
                }
                locations.insert(
                    name.to_string(),
                    (
                        false,
                        NearestName {
                            name: name.to_string(),
                            path: path.to_path_buf(),
                            line: u32::try_from(line_index).unwrap_or(u32::MAX),
                            line_text: snippet_bounds::window_snippet_line(line, found.start()),
                        },
                    ),
                );
            }
            if occurrences >= NEAREST_NAME_MAX_OCCURRENCES {
                break;
            }
        }
    }

    let mut scored = locations
        .into_values()
        .filter(|(_, name)| !name.name.eq_ignore_ascii_case(&target_lower))
        .map(|(declared, name)| {
            let score = similarity(&target_lower, &target_words, &name.name);
            (score, declared, name)
        })
        .filter(|(score, _, _)| *score >= NEAREST_NAME_MIN_SIMILARITY)
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.name.cmp(&right.2.name))
            .then_with(|| left.2.path.cmp(&right.2.path))
            .then_with(|| left.2.line.cmp(&right.2.line))
    });
    scored
        .into_iter()
        .take(NEAREST_NAME_LIMIT)
        .map(|(_, _, name)| name)
        .collect()
}

/// How close `candidate` is to the query name, in per mille: the larger of
/// its edit-distance similarity (1 - distance / longer length, compared
/// case-insensitively) and its word overlap (shared words over all words,
/// splitting snake_case and camelCase into words).
fn similarity(target_lower: &str, target_words: &[String], candidate: &str) -> u32 {
    let candidate_lower = candidate.to_ascii_lowercase();
    let longest = target_lower.len().max(candidate_lower.len()).max(1);
    let distance = levenshtein(target_lower.as_bytes(), candidate_lower.as_bytes());
    let edit = 1000 - (distance.min(longest) * 1000 / longest) as u32;

    let candidate_words = name_words(candidate);
    let shared = target_words
        .iter()
        .filter(|word| candidate_words.contains(word))
        .count();
    let union = target_words.len() + candidate_words.len() - shared;
    let words = if union == 0 {
        0
    } else {
        (shared * 1000 / union) as u32
    };
    edit.max(words)
}

/// The lowercase words of a name: snake_case and dotted parts, camelCase and
/// acronym boundaries (`HTTPServer` -> `http`, `server`). Each word once.
fn name_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for part in name.split(|character: char| !character.is_ascii_alphanumeric()) {
        let characters = part.chars().collect::<Vec<_>>();
        let mut start = 0;
        for index in 1..characters.len() {
            let previous = characters[index - 1];
            let current = characters[index];
            let next_is_lower = characters
                .get(index + 1)
                .is_some_and(char::is_ascii_lowercase);
            let boundary = (previous.is_ascii_lowercase() && current.is_ascii_uppercase())
                || (previous.is_ascii_uppercase() && current.is_ascii_uppercase() && next_is_lower);
            if boundary {
                words.push(characters[start..index].iter().collect());
                start = index;
            }
        }
        if start < characters.len() {
            words.push(characters[start..].iter().collect());
        }
    }
    let mut unique = Vec::new();
    for word in words.into_iter().map(|word| word.to_ascii_lowercase()) {
        if !word.is_empty() && !unique.contains(&word) {
            unique.push(word);
        }
    }
    unique
}

fn levenshtein(left: &[u8], right: &[u8]) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (row, left_byte) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, right_byte) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_byte != right_byte);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// The 0-based line holding byte `offset` of `text`, and that line windowed
/// around `name`.
fn line_of_offset(text: &str, offset: usize, name: &str) -> (u32, String) {
    let offset = offset.min(text.len());
    let line_index = text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    let line = text.lines().nth(line_index).unwrap_or_default();
    let anchor = line.find(name).unwrap_or(0);
    (
        u32::try_from(line_index).unwrap_or(u32::MAX),
        snippet_bounds::window_snippet_line(line, anchor),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_words_split_snake_camel_and_acronyms() {
        assert_eq!(
            name_words("mark_file_refreshing"),
            ["mark", "file", "refreshing"]
        );
        assert_eq!(
            name_words("PendingSubcInspect"),
            ["pending", "subc", "inspect"]
        );
        assert_eq!(name_words("HTTPServer"), ["http", "server"]);
        assert_eq!(name_words("ctx.set_harness"), ["ctx", "set", "harness"]);
    }

    #[test]
    fn looked_up_identifiers_cover_plain_and_dotted_names_only() {
        assert_eq!(
            looked_up_identifiers("running_tasks"),
            Some(vec!["running_tasks".to_string()])
        );
        assert_eq!(
            looked_up_identifiers("ctx.set_harness"),
            Some(vec![
                "ctx.set_harness".to_string(),
                "set_harness".to_string()
            ])
        );
        assert_eq!(looked_up_identifiers("aft-mem-sampler"), None);
        assert_eq!(looked_up_identifiers("Foo::bar"), None);
        assert_eq!(looked_up_identifiers("ab"), None);
    }

    #[test]
    fn nearest_names_rank_by_similarity_and_prefer_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.rs");
        let second = dir.path().join("b.rs");
        std::fs::write(
            &first,
            "fn caller() {\n    mark_file_refreshed(1);\n    unrelated_helper();\n}\n",
        )
        .unwrap();
        std::fs::write(
            &second,
            "pub fn mark_file_refreshed(id: u32) {}\npub fn mark_file_stale() {}\n",
        )
        .unwrap();

        let names = nearest_names("mark_file_refreshing", [first.as_path(), second.as_path()]);
        let listed = names
            .iter()
            .map(|name| (name.name.as_str(), name.path.as_path(), name.line))
            .collect::<Vec<_>>();
        assert_eq!(
            listed,
            [
                ("mark_file_refreshed", second.as_path(), 0),
                ("mark_file_stale", second.as_path(), 1),
            ],
            "the declaration in b.rs wins over the earlier call site in a.rs, and names \
             below the similarity floor (`caller`, `unrelated_helper`) are left out"
        );
    }

    #[test]
    fn render_lists_each_name_with_its_relative_location() {
        let answer = NotFoundAnswer {
            query: "slow_fetch".to_string(),
            names: vec![NearestName {
                name: "slow_fetcher".to_string(),
                path: PathBuf::from("/root/src/net.rs"),
                line: 9,
                line_text: "fn slow_fetcher() {}".to_string(),
            }],
        };
        assert_eq!(
            answer.render(Path::new("/root")),
            "`slow_fetch` not found in this project. Nearest names: `slow_fetcher` (src/net.rs:10)"
        );
        let empty = NotFoundAnswer {
            query: "zzz_qqq".to_string(),
            names: Vec::new(),
        };
        assert_eq!(
            empty.render(Path::new("/root")),
            "`zzz_qqq` not found in this project, and no similar names were found."
        );
    }
}
