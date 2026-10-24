//! Ranking for aft_search's regex and literal route.
//!
//! A regex or literal query used to reach the grep engine with a small match
//! cap: the first matches the parallel scan happened to meet were kept and
//! sorted by modification time. For an identifier alternation such as
//! `validate_read_path|restrict_to_project_root` the first matches were mostly
//! documentation and release notes, and the file that defines the identifier
//! was often not among them at all.
//!
//! This route instead verifies every candidate file the trigram index admits,
//! within a bound on files examined and time spent (never on matches found),
//! and ranks whole files before a page is cut. Each file is one result,
//! represented by its declaration of a matched name or else its first
//! matching line. Files are ordered by class (source code, then other text
//! such as documentation and configuration, then structured data files; see
//! [`super::data_file`]), then by the strongest declaration of a matched name
//! (a keyword declaration, then a field line, then none), then by how many
//! lines match, newest first, and by path. The pattern alone decides which
//! lines match, so ranking never adds or removes a match.

use std::cmp::Reverse;
use std::path::Path;
use std::time::Duration;

use crate::parser::{detect_language, LangId};
use crate::search_index::{
    GrepExaminationLimits, GrepFileCollection, GrepFileMatches, GrepMatch, GrepResult,
};

use super::data_file::DataFileClassifier;

/// Most candidate files one query reads. Past this the ranking covers the
/// files examined so far, and the response says so.
pub(crate) const MAX_EXAMINED_FILES: usize = 20_000;

/// Time after which no further candidate file is started.
pub(crate) const EXAMINATION_BUDGET: Duration = Duration::from_secs(5);

/// Most matching lines kept per file for ranking. Every matching line is
/// still counted, and a later line declaring the matched name is kept too.
pub(crate) const MAX_LINES_PER_FILE: usize = 20;

const LIMITS: GrepExaminationLimits = GrepExaminationLimits {
    max_files: MAX_EXAMINED_FILES,
    budget: EXAMINATION_BUDGET,
    max_lines_per_file: MAX_LINES_PER_FILE,
};

#[cfg(test)]
thread_local! {
    /// Lets a test put the file bound within reach of a small fixture.
    pub(crate) static MAX_EXAMINED_FILES_OVERRIDE: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// The bounds for one query.
pub(crate) fn limits() -> GrepExaminationLimits {
    #[cfg(test)]
    if let Some(max_files) = MAX_EXAMINED_FILES_OVERRIDE.with(std::cell::Cell::get) {
        return GrepExaminationLimits {
            max_files,
            ..LIMITS
        };
    }
    LIMITS
}

/// Coarse kind of a file, in ranking order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FileClass {
    Source,
    Other,
    Data,
}

/// Classify a file from its name alone, without reading it.
fn class_from_name(path: &Path) -> FileClass {
    match detect_language(path) {
        Some(LangId::Json) => FileClass::Data,
        Some(LangId::Markdown | LangId::Yaml | LangId::Toml | LangId::Html) | None => {
            FileClass::Other
        }
        Some(_) => FileClass::Source,
    }
}

/// Order in which candidate files are examined: source first, so that when the
/// file bound is reached the files most likely to rank first were read.
pub(crate) fn examine_priority(path: &Path) -> u8 {
    class_from_name(path) as u8
}

/// Class used for ranking. A JSON file counts as data only when
/// `DataFileClassifier::demote` says so: it must be generated, large or have
/// very long lines, and the query must not name it or ask for JSON. Any other
/// JSON file, such as a small hand-edited config, ranks with other text.
fn file_class(path: &Path, data_files: &mut DataFileClassifier<'_>) -> FileClass {
    if data_files.demote(path) {
        return FileClass::Data;
    }
    match class_from_name(path) {
        FileClass::Data => FileClass::Other,
        class => class,
    }
}

/// Modifiers that may precede a declaration keyword.
const DECLARATION_MODIFIERS: &[&str] = &[
    "pub",
    "export",
    "default",
    "async",
    "unsafe",
    "extern",
    "static",
    "declare",
    "abstract",
    "public",
    "private",
    "protected",
    "internal",
    "override",
    "final",
    "open",
    "inline",
    "readonly",
];

/// Keywords after which the next identifier is the declared name.
const DECLARATION_KEYWORDS: &[&str] = &[
    "fn",
    "function",
    "def",
    "class",
    "struct",
    "enum",
    "union",
    "trait",
    "interface",
    "type",
    "const",
    "let",
    "var",
    "val",
    "mod",
    "module",
    "namespace",
    "object",
    "func",
    "macro_rules!",
];

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn leading_identifier(text: &str) -> Option<&str> {
    let end = text
        .bytes()
        .position(|byte| !is_identifier_byte(byte))
        .unwrap_or(text.len());
    (end > 0).then(|| &text[..end])
}

/// Strip one leading word (and the whitespace after it) when it is `word`.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(word)?;
    if rest
        .bytes()
        .next()
        .is_some_and(|byte| is_identifier_byte(byte) && !word.ends_with('!'))
    {
        return None;
    }
    Some(rest.trim_start())
}

/// How a line introduces a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Declaration {
    /// A declaration keyword names it: `fn name`, `class Name`, `const NAME`.
    Keyword,
    /// It leads the line followed by a colon: a struct field or object
    /// property. A struct literal's `name: value` looks the same, so this form
    /// ranks below a keyword declaration.
    Field,
}

/// The name a line declares, if it declares one, and how: `pub fn name`,
/// `export async function name`, `class Name`, `const name =`,
/// `func (r *T) Name`, or a field or property written `name: ...` at the
/// start of the line.
pub(crate) fn declared_name(line: &str) -> Option<(&str, Declaration)> {
    let mut rest = line.trim_start();
    // Rust visibility with a scope: pub(crate), pub(super), pub(in path).
    loop {
        if let Some(after) = rest.strip_prefix("pub(") {
            let close = after.find(')')?;
            rest = after[close + 1..].trim_start();
            continue;
        }
        match DECLARATION_MODIFIERS
            .iter()
            .find_map(|modifier| strip_word(rest, modifier))
        {
            Some(after) => rest = after,
            None => break,
        }
    }
    for keyword in DECLARATION_KEYWORDS {
        let Some(mut after) = strip_word(rest, keyword) else {
            continue;
        };
        // `const fn name` and similar: the keyword was a modifier.
        if let Some(inner) = DECLARATION_KEYWORDS
            .iter()
            .find_map(|inner| strip_word(after, inner))
        {
            after = inner;
        }
        // Generator functions: `function* name`.
        after = after.trim_start_matches('*').trim_start();
        // Go methods: `func (receiver *Type) Name(`.
        if *keyword == "func" && after.starts_with('(') {
            let close = after.find(')')?;
            after = after[close + 1..].trim_start();
        }
        return leading_identifier(after).map(|name| (name, Declaration::Keyword));
    }
    // A struct field, object property or annotated name: `name: ...`, but not
    // a path such as `name::item`.
    let name = leading_identifier(rest)?;
    let after = rest[name.len()..].trim_start();
    (after.starts_with(':') && !after.starts_with("::")).then_some((name, Declaration::Field))
}

/// True when `name` occurs in `text` as a whole identifier.
fn contains_identifier(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(name).any(|(start, _)| {
        let end = start + name.len();
        let before_ok = start == 0 || !is_identifier_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_identifier_byte(bytes[end]);
        before_ok && after_ok
    })
}

/// How the line of this match declares the matched name, if it does.
pub(crate) fn match_declaration(grep_match: &GrepMatch) -> Option<Declaration> {
    declared_name(&grep_match.line_text)
        .filter(|(name, _)| contains_identifier(&grep_match.match_text, name))
        .map(|(_, declaration)| declaration)
}

/// Keyword declarations first, then fields, then files that declare nothing.
fn declaration_rank(declaration: Option<Declaration>) -> u8 {
    match declaration {
        Some(Declaration::Keyword) => 0,
        Some(Declaration::Field) => 1,
        None => 2,
    }
}

/// A matching line past the per-file listing limit is still listed when it
/// declares the matched name, so the ranking sees it and the reader gets it.
pub(crate) fn keep_past_line_limit(grep_match: &GrepMatch) -> bool {
    match_declaration(grep_match) == Some(Declaration::Keyword)
}

/// How [`rank_collection`] breaks a tie between files of the same class,
/// declaration strength and matched-line count, before falling back to path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecencyTiebreak {
    /// Newest file first: the pattern-only route's order.
    NewestFirst,
    /// No modification-time tie-break, so ties fall to the path. Split-query
    /// search feeds these positions into score fusion, where an order that
    /// moves when a file is touched (without its content changing) would make
    /// the fused ranking depend on the machine and the time of the checkout.
    None,
}

/// Order the examined files, one result per file with its listed lines.
///
/// Files are ordered by class (source, other text, data), then by the
/// strongest declaration of a matched name among their listed lines (keyword,
/// field, none), then by how many lines match, newest first unless `recency`
/// says otherwise, and by path.
/// Each file lists every matching line when it has ten or fewer, else five,
/// its declaration line first (see [`RankedFile`]).
/// `total_matches` counts every matching line in the examined files.
/// `truncated` and `engine_capped` are set when a bound left files unexamined.
pub(crate) fn rank_collection(
    collection: GrepFileCollection,
    project_root: &Path,
    query: &str,
    recency: RecencyTiebreak,
) -> RankedFiles {
    let mut data_files = DataFileClassifier::new(project_root, query);
    let mut keyed: Vec<((FileClass, Option<Declaration>), GrepFileMatches)> = collection
        .files
        .into_iter()
        .map(|file| {
            let class = file_class(&file.path, &mut data_files);
            let declaration = file.matches.iter().filter_map(match_declaration).min();
            ((class, declaration), file)
        })
        .collect();
    keyed.sort_by(|((left_class, left_declaration), left), ((right_class, right_declaration), right)| {
        left_class
            .cmp(right_class)
            // `None` sorts first for Option; a declaration must sort first here.
            .then_with(|| declaration_rank(*left_declaration).cmp(&declaration_rank(*right_declaration)))
            .then_with(|| right.matched_lines.cmp(&left.matched_lines))
            .then_with(|| match recency {
                RecencyTiebreak::NewestFirst => Reverse(left.modified).cmp(&Reverse(right.modified)),
                RecencyTiebreak::None => std::cmp::Ordering::Equal,
            })
            .then_with(|| left.path.cmp(&right.path))
    });

    let files_with_matches = keyed.len();
    let total_matches = keyed.iter().map(|(_, file)| file.matched_lines).sum();
    // One result per file, fixed here before any page is cut, so a file can
    // never be split across a page boundary and show up on two pages.
    let files: Vec<RankedFile> = keyed
        .into_iter()
        .filter_map(|(_, file)| RankedFile::from_matches(file.matches, file.matched_lines))
        .collect();

    RankedFiles {
        summary: GrepResult {
            matches: files.iter().map(|file| file.lines[0].clone()).collect(),
            total_matches,
            files_searched: collection.files_examined,
            files_with_matches,
            index_status: collection.index_status,
            truncated: collection.examination_capped,
            fully_degraded: collection.fully_degraded,
            engine_capped: collection.examination_capped,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: collection.missing_on_disk,
            // The collection's time bound is reported through
            // `examination_capped` above.
            scan_deadline_reached: false,
            files_read_directly: 0,
            walk_bound: None,
        },
        files,
    }
}

/// A file with as many matching lines as it has at most, the same allowance
/// grep's text rendering gives a file (`format_grep_text` in
/// `commands/grep.rs`): every line when the file has this many or fewer.
const ALL_LINES_UP_TO: usize = 10;

/// Lines listed for a file with more matching lines than [`ALL_LINES_UP_TO`].
const LINES_WHEN_MORE: usize = 5;

/// One ranked result: a file and the matching lines listed for it.
#[derive(Clone, Debug)]
pub(crate) struct RankedFile {
    /// The file's strongest declaration of a matched name (or its first
    /// matching line when it declares nothing), then its other listed lines
    /// in file order. Never empty.
    pub lines: Vec<GrepMatch>,
    /// Matching lines in the file that are not listed.
    pub more_in_file: usize,
}

impl RankedFile {
    /// True when the file's leading line declares a matched name with a
    /// declaration keyword (`fn name`, `class Name`, `const NAME`). The
    /// leading line is the file's strongest declaration, so this holds exactly
    /// when the file ranks among keyword-declaring files. A field line
    /// (`name: value`) does not count: it looks the same as a struct literal
    /// or object property that merely uses the name.
    pub(crate) fn declares_match(&self) -> bool {
        match_declaration(&self.lines[0]) == Some(Declaration::Keyword)
    }

    fn from_matches(matches: Vec<GrepMatch>, matched_lines: usize) -> Option<Self> {
        let best = matches
            .iter()
            .enumerate()
            .min_by_key(|(index, line)| (declaration_rank(match_declaration(line)), *index))
            .map(|(index, _)| index)?;
        let shown = if matched_lines <= ALL_LINES_UP_TO {
            matched_lines
        } else {
            LINES_WHEN_MORE
        }
        .min(matches.len())
        .max(1);
        let mut rest = matches;
        let first = rest.remove(best);
        let mut lines = Vec::with_capacity(shown);
        lines.push(first);
        lines.extend(rest.into_iter().take(shown - 1));
        Some(Self {
            more_in_file: matched_lines.saturating_sub(lines.len()),
            lines,
        })
    }
}

/// The ranked files and the totals that describe the whole examination.
#[derive(Clone, Debug)]
pub(crate) struct RankedFiles {
    /// Totals, flags and one match per file (its first listed line), in rank
    /// order.
    pub summary: GrepResult,
    /// The same files, in the same order, with their listed lines.
    pub files: Vec<RankedFile>,
}

/// The count line of a ranked page. A ranked page lists one result per
/// file, so the line counts files first, then the matching lines in them.
pub(crate) const RANKED_COUNT_LINE: &str =
    "{files} files match ({lines} matching lines); one result per file, definitions first.";

/// Render one page of ranked files the way grep renders matches: each file's
/// path, then its listed lines, then how many more lines it has. The count
/// line under them covers every match in the examined files.
pub(crate) fn format_ranked_page(
    page: &[RankedFile],
    summary: &GrepResult,
    project_root: &Path,
) -> String {
    let mut sections = Vec::with_capacity(page.len());
    for file in page {
        let path = &file.lines[0].file;
        let mut section = path
            .strip_prefix(project_root)
            .unwrap_or(path)
            .display()
            .to_string();
        for line in &file.lines {
            section.push_str(&format!(
                "\n{}: {}",
                line.line,
                crate::commands::grep::truncate_line_text(&line.line_text)
            ));
        }
        if file.more_in_file > 0 {
            section.push_str(&format!("\n+{} more in this file", file.more_in_file));
        }
        sections.push(section);
    }
    // The count line ends with the same cap and index tags grep's footer
    // carries, so a floor or a non-ready index is still disclosed.
    let footer = format!(
        "{}{}",
        RANKED_COUNT_LINE
            .replace("{files}", &summary.files_with_matches.to_string())
            .replace("{lines}", &summary.total_matches.to_string()),
        crate::commands::grep::grep_footer_tags(summary)
    );
    if sections.is_empty() {
        footer
    } else {
        format!("{}\n\n{}", sections.join("\n\n"), footer)
    }
}

/// One line saying how much of the candidate set the ranking covered, for a
/// response whose examination stopped at a bound.
pub(crate) fn examination_disclosure(files_examined: usize, candidates: usize) -> String {
    format!(
        "[examined {files_examined} of {candidates} candidate files before the file or time budget ran out; files not examined are not ranked]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use crate::search_index::IndexStatus;

    fn grep_match(file: &str, line_text: &str, match_text: &str) -> GrepMatch {
        GrepMatch {
            file: PathBuf::from(file),
            line: 1,
            column: 1,
            line_text: line_text.to_string(),
            match_text: match_text.to_string(),
        }
    }

    #[test]
    fn declared_name_reads_common_declaration_forms() {
        use Declaration::{Field, Keyword};
        let cases = [
            (
                "    pub fn validate_read_path(",
                Some(("validate_read_path", Keyword)),
            ),
            ("pub(crate) async fn load(", Some(("load", Keyword))),
            (
                "export async function addToCart(item) {",
                Some(("addToCart", Keyword)),
            ),
            ("export const useCart = () => {", Some(("useCart", Keyword))),
            (
                "  const addToCart = useCallback(",
                Some(("addToCart", Keyword)),
            ),
            ("function* walk(tree) {", Some(("walk", Keyword))),
            (
                "func (s *Server) Serve(conn net.Conn) {",
                Some(("Serve", Keyword)),
            ),
            ("pub const fn limit() -> usize {", Some(("limit", Keyword))),
            ("class CartStore {", Some(("CartStore", Keyword))),
            (
                "def restrict_to_project_root(self):",
                Some(("restrict_to_project_root", Keyword)),
            ),
            ("macro_rules! bail {", Some(("bail", Keyword))),
            (
                "    pub restrict_to_project_root: bool,",
                Some(("restrict_to_project_root", Field)),
            ),
            (
                "  addToCart: (item) => set(item),",
                Some(("addToCart", Field)),
            ),
            ("    validate_read_path(&path)?;", None),
            ("Use `validate_read_path` to check reads.", None),
            ("crate::context::validate_read_path(path)", None),
            ("- addToCart: adds an item", None),
            ("  const { addToCart } = useCart();", None),
        ];
        for (line, expected) in cases {
            assert_eq!(declared_name(line), expected, "line: {line}");
        }
    }

    #[test]
    fn declaration_requires_the_declared_name_inside_the_match() {
        assert_eq!(
            match_declaration(&grep_match(
                "a.rs",
                "pub fn validate_read_path(",
                "validate_read_path"
            )),
            Some(Declaration::Keyword)
        );
        // The line declares a different name than the one matched.
        assert_eq!(
            match_declaration(&grep_match(
                "a.rs",
                "fn check() { validate_read_path(p) }",
                "validate_read_path"
            )),
            None
        );
        // A match on part of the declared name is not the declaration.
        assert_eq!(
            match_declaration(&grep_match(
                "a.rs",
                "fn validate_read_path_strict(",
                "validate_read_path"
            )),
            None
        );
    }

    fn file(
        path: &str,
        age_secs: u64,
        lines: Vec<GrepMatch>,
        matched_lines: usize,
    ) -> GrepFileMatches {
        GrepFileMatches {
            path: PathBuf::from(path),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
            matches: lines,
            matched_lines,
        }
    }

    fn collection(files: Vec<GrepFileMatches>, capped: bool) -> GrepFileCollection {
        GrepFileCollection {
            candidate_files: files.len(),
            files_examined: files.len(),
            files,
            examination_capped: capped,
            fully_degraded: false,
            index_status: IndexStatus::Ready,
            missing_on_disk: 0,
        }
    }

    fn ranked_paths(result: &RankedFiles) -> Vec<String> {
        let paths: Vec<String> = result
            .summary
            .matches
            .iter()
            .map(|grep_match| grep_match.file.display().to_string())
            .collect();
        let mut unique = paths.clone();
        unique.dedup();
        assert_eq!(unique, paths, "each file must be exactly one result");
        paths
    }

    #[test]
    fn definitions_rank_first_then_source_then_documentation() {
        let result = rank_collection(
            collection(
                vec![
                    file(
                        "/nonexistent/docs/guide.md",
                        0,
                        vec![grep_match(
                            "/nonexistent/docs/guide.md",
                            "Call addToCart.",
                            "addToCart",
                        )],
                        1,
                    ),
                    file(
                        "/nonexistent/src/components/Button.jsx",
                        1,
                        vec![grep_match(
                            "/nonexistent/src/components/Button.jsx",
                            "onClick={() => addToCart(item)}",
                            "addToCart",
                        )],
                        1,
                    ),
                    file(
                        "/nonexistent/src/hooks/useCart.js",
                        2,
                        vec![grep_match(
                            "/nonexistent/src/hooks/useCart.js",
                            "  const addToCart = (item) => {",
                            "addToCart",
                        )],
                        1,
                    ),
                ],
                false,
            ),
            Path::new("/nonexistent"),
            "addToCart",
            RecencyTiebreak::NewestFirst,
        );
        assert_eq!(
            ranked_paths(&result),
            [
                "/nonexistent/src/hooks/useCart.js",
                "/nonexistent/src/components/Button.jsx",
                "/nonexistent/docs/guide.md",
            ]
        );
        assert_eq!(result.summary.total_matches, 3);
        assert_eq!(result.summary.files_with_matches, 3);
        assert!(!result.summary.engine_capped);
        assert!(!result.summary.truncated);
    }

    #[test]
    fn keyword_declaration_outranks_field_lines_and_more_mentions() {
        let config = "/nonexistent/src/config.rs";
        let setup = "/nonexistent/src/setup.rs";
        let context = "/nonexistent/src/context.rs";
        let result = rank_collection(
            collection(
                vec![
                    file(
                        setup,
                        0,
                        vec![
                            grep_match(
                                setup,
                                "        restrict_to_project_root: true,",
                                "restrict_to_project_root",
                            ),
                            grep_match(
                                setup,
                                "    if cfg.restrict_to_project_root {",
                                "restrict_to_project_root",
                            ),
                            grep_match(setup, "    validate_read_path(&p)?;", "validate_read_path"),
                        ],
                        3,
                    ),
                    file(
                        config,
                        0,
                        vec![grep_match(
                            config,
                            "    pub restrict_to_project_root: bool,",
                            "restrict_to_project_root",
                        )],
                        1,
                    ),
                    file(
                        context,
                        0,
                        vec![grep_match(
                            context,
                            "    pub fn validate_read_path(",
                            "validate_read_path",
                        )],
                        1,
                    ),
                ],
                false,
            ),
            Path::new("/nonexistent"),
            "validate_read_path|restrict_to_project_root",
            RecencyTiebreak::NewestFirst,
        );
        // Both field-form files tie on declaration strength; the one with more
        // matching lines goes first.
        assert_eq!(ranked_paths(&result), [context, setup, config]);
    }

    #[test]
    fn only_keyword_declarations_are_kept_past_the_line_limit() {
        assert!(keep_past_line_limit(&grep_match(
            "a.rs",
            "pub fn load(",
            "load"
        )));
        assert!(!keep_past_line_limit(&grep_match(
            "a.rs",
            "    load: true,",
            "load"
        )));
        assert!(!keep_past_line_limit(&grep_match(
            "a.rs",
            "    load();",
            "load"
        )));
    }

    fn numbered(path: &str, line: u32, text: &str, matched: &str) -> GrepMatch {
        GrepMatch {
            line,
            ..grep_match(path, text, matched)
        }
    }

    #[test]
    fn files_list_all_lines_up_to_ten_else_five_with_the_declaration_first() {
        let path = "/nonexistent/src/cart.js";
        let lines = |count: u32| -> Vec<GrepMatch> {
            (1..=count)
                .map(|line| {
                    if line == 7 {
                        numbered(path, line, "export function addToCart(item) {", "addToCart")
                    } else {
                        numbered(path, line, "  addToCart(item);", "addToCart")
                    }
                })
                .collect()
        };
        let listed =
            |file: &RankedFile| file.lines.iter().map(|line| line.line).collect::<Vec<_>>();

        let ten = RankedFile::from_matches(lines(10), 10).expect("file");
        assert_eq!(listed(&ten), [7, 1, 2, 3, 4, 5, 6, 8, 9, 10]);
        assert_eq!(ten.more_in_file, 0);

        let many = RankedFile::from_matches(lines(20), 30).expect("file");
        assert_eq!(listed(&many), [7, 1, 2, 3, 4]);
        assert_eq!(many.more_in_file, 25);

        let summary = rank_collection(
            collection(Vec::new(), false),
            Path::new("/nonexistent"),
            "addToCart",
            RecencyTiebreak::NewestFirst,
        )
        .summary;
        let text = format_ranked_page(&[many], &summary, Path::new("/nonexistent"));
        assert!(
            text.starts_with(
                "src/cart.js\n7: export function addToCart(item) {\n1:   addToCart(item);"
            ),
            "{text}"
        );
        assert!(
            text.contains("\n4:   addToCart(item);\n+25 more in this file\n\n0 files match"),
            "{text}"
        );
    }

    #[test]
    fn collector_examines_source_first_and_counts_every_line() {
        use crate::pattern_compile::{compile, CompileOpts, CompileResult};
        use crate::search_index::{PathFilters, SearchIndex};

        let project = tempfile::tempdir().expect("project dir");
        let root = std::fs::canonicalize(project.path()).expect("canonical root");
        std::fs::write(
            root.join("a.rs"),
            "fn alpha() {}\nalpha();\nALPHA\nalpha(); alpha();\n",
        )
        .expect("write a.rs");
        std::fs::write(root.join("b.md"), "alpha beta\n").expect("write b.md");
        std::fs::write(root.join("c.json"), "{\"beta\": 1}\n").expect("write c.json");
        let snapshot = SearchIndex::build(&root).snapshot();
        let CompileResult::Ok(pattern) = compile("alpha|beta", CompileOpts::default()) else {
            panic!("pattern compiles");
        };
        let collect = |max_files: usize, max_lines_per_file: usize| {
            snapshot.collect_grep_matches_by_file(
                &pattern,
                &PathFilters::default(),
                &root,
                None,
                GrepExaminationLimits {
                    max_files,
                    budget: Duration::from_secs(60),
                    max_lines_per_file,
                },
                &examine_priority,
                &|grep_match: &GrepMatch| grep_match.line_text.contains("alpha(); alpha"),
            )
        };

        let all = collect(100, 100);
        assert!(!all.examination_capped);
        assert_eq!((all.candidate_files, all.files_examined), (3, 3));
        let lines = |collection: &GrepFileCollection, name: &str| {
            collection
                .files
                .iter()
                .find(|file| file.path.ends_with(name))
                .map(|file| {
                    (
                        file.matched_lines,
                        file.matches.iter().map(|m| m.line).collect::<Vec<_>>(),
                    )
                })
        };
        // Case-sensitive: `ALPHA` on line 3 is not a match, and line 4 counts
        // once however many matches it holds.
        assert_eq!(lines(&all, "a.rs"), Some((3, vec![1, 2, 4])));
        assert_eq!(lines(&all, "b.md"), Some((1, vec![1])));
        assert_eq!(lines(&all, "c.json"), Some((1, vec![1])));

        // One listed line per file, plus the line the caller asked to keep.
        let limited = collect(100, 1);
        assert_eq!(lines(&limited, "a.rs"), Some((3, vec![1, 4])));

        // A one-file bound examines the source file and says it stopped.
        let bounded = collect(1, 100);
        assert!(bounded.examination_capped);
        assert_eq!((bounded.candidate_files, bounded.files_examined), (3, 1));
        assert_eq!(bounded.files.len(), 1);
        assert!(bounded.files[0].path.ends_with("a.rs"));
    }

    #[test]
    fn capped_examination_is_reported_and_each_file_is_one_result() {
        let capped = rank_collection(
            collection(
                vec![file(
                    "/nonexistent/a.rs",
                    0,
                    vec![grep_match("/nonexistent/a.rs", "x", "x")],
                    1,
                )],
                true,
            ),
            Path::new("/nonexistent"),
            "x",
            RecencyTiebreak::NewestFirst,
        );
        assert!(capped.summary.truncated && capped.summary.engine_capped);

        let many = "/nonexistent/many.rs";
        let complete = rank_collection(
            collection(
                vec![file(
                    many,
                    0,
                    vec![
                        grep_match(many, "    load();", "load"),
                        grep_match(many, "    load: true,", "load"),
                        grep_match(many, "pub fn load() {", "load"),
                    ],
                    40,
                )],
                false,
            ),
            Path::new("/nonexistent"),
            "load",
            RecencyTiebreak::NewestFirst,
        );
        assert!(!complete.summary.truncated && !complete.summary.engine_capped);
        assert_eq!(complete.summary.total_matches, 40);
        // The file's one result leads with its keyword declaration, then its
        // other lines in file order; 37 of the 40 matching lines are not listed.
        assert_eq!(complete.files.len(), 1);
        let lines: Vec<&str> = complete.files[0]
            .lines
            .iter()
            .map(|line| line.line_text.as_str())
            .collect();
        assert_eq!(lines, ["pub fn load() {", "    load();", "    load: true,"]);
        assert_eq!(complete.files[0].more_in_file, 37);
    }
}
