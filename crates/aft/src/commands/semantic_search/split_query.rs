//! Split-query search: a request that carries both `query` (prose) and
//! `pattern` (a regex in grep's syntax).
//!
//! The two inputs answer different questions. The pattern says where a name
//! occurs; the prose says which file explains something. The ranking is
//! query-primary: the base order is exactly the ranking `query` alone
//! returns, and the pattern only adds bounded evidence on top of it (see
//! [`query_primary_order`]):
//!
//! - a leading query result is lifted a little by its own matches of a
//!   selective alternative of the pattern, damped by how many files that
//!   alternative matched; a declaration of a matched name counts more than a
//!   mention, shared 1/n^2 among the files declaring the same name, and never
//!   less than a mention;
//! - an alternative's definition is placed right after the best-placed
//!   leading result that mentions the name, never above it. A call site
//!   always mentions the name it calls, so this places a definition after
//!   its caller without consulting the call graph, and every page sees the
//!   same order;
//! - with no mentioning result near the top, the definition is admitted only
//!   when the query has no semantic lane or the definition has query
//!   relevance of its own (its semantic or lexical score for the query). An
//!   admitted definition is a separate result, so its extra file open is
//!   counted where it is shown.
//!
//! Everything a reader needs to find the named definitions regardless of the
//! order is in the pattern summary line ([`PatternList::summary_line`]),
//! which is computed from the pattern list alone and so is the same on every
//! page.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::search_index::{GrepFileCollection, GrepFileMatches, GrepMatch, GrepResult};

use super::comparator::CandidateResult;
use super::evidence_descriptor::EvidenceDescriptor;
use super::plan_table::SearchLaneKind;
use super::regex_route::{self, RankedFile};
use super::scoring::{LaneScoringRule, ScoringError, ScoringPolicy};

// Ranking constants for split-query requests, all in one place.
//
// Scale: a candidate at base position r (0-based) of the query's own ranking
// starts at 1 / (BASE_RRF_CONSTANT + r + 1). With 60 the first position is
// 0.0164 and neighbouring positions near the top differ by about 0.00026, so
// a bonus of 0.0006 is worth about two positions there and one of 0.002
// about eight.
//
// Tuning source: the eleven split tuning rows
// (benchmarks/aft-search/split-tuning-manifest.json) replayed with the call
// graph off, as the gate runs, never the rows the search-quality gate
// compares against its reference. There the query alone scores MRR@10 0.2403
// and hit@3 0.2727, and these values 0.2985 / 0.3636 with no row ranking its
// concept answer below the query alone. One-at-a-time sweeps of every
// constant below left that unchanged except the mention bonus: 0 or 0.0003
// scored lower, 0.0009 or more lifted hit@3 but put a mentioning test file
// above a concept answer the query ranked first. The placement rules, not
// the bonus sizes, decide where definitions go.

/// Reciprocal-rank constant of the base order.
pub(crate) const BASE_RRF_CONSTANT: f32 = 60.0;
/// Score a leading query result gets for its own match of a selective
/// alternative (a line that matches it), before damping by the alternative's
/// matched-file count.
pub(crate) const MENTION_ANCHOR_BOOST: f32 = 0.0006;
/// Score a leading query result gets for declaring a name a selective
/// alternative matches, before damping and the 1/n^2 share among the n files
/// that declare that same name. A declarer never gets less than the mention
/// score.
pub(crate) const DEFINITION_ANCHOR_BOOST: f32 = 0.0012;
/// Matched-file count up to which an alternative's bonuses are undamped.
pub(crate) const SELECTIVITY_THRESHOLD: usize = 10;
/// Exponent of the damping past [`SELECTIVITY_THRESHOLD`]: the multiplier is
/// (threshold / matched files) ^ decay.
pub(crate) const SELECTIVITY_DECAY: f32 = 1.0;
/// Most files an alternative may match and still be selective.
pub(crate) const SELECTIVE_ALTERNATIVE_MAX_FILES: usize = 50;
/// How many leading positions of the query's ranking count as "the query
/// found it near the top": only those get bonuses or host a definition.
pub(crate) const PROSE_TOP_K: usize = 10;
/// Base position a definition with no mentioning host is admitted at when
/// it has query relevance of its own.
pub(crate) const ADMISSION_BASE_POSITION: usize = 4;
/// Base position a definition with no mentioning host is admitted at when
/// the query has no semantic lane (identifier-shaped prose, or the semantic
/// index is still building): the pattern is then the strongest evidence.
pub(crate) const ADMISSION_BASE_POSITION_WITHOUT_SEMANTIC: usize = 0;
/// A definition has semantic relevance of its own when its best chunk's
/// cosine with the query is at least this share of the query's best cosine.
pub(crate) const SEMANTIC_ADMISSION_SHARE: f32 = 0.9;
/// A definition has lexical relevance of its own when its trigram score for
/// the query is at least this share of the best file's trigram score.
pub(crate) const LEXICAL_ADMISSION_SHARE: f32 = 0.5;
/// Most definitions one request scores for relevance or carries into the
/// list.
pub(crate) const MAX_PLACED_DEFINITIONS: usize = 8;
/// Most alternatives one group expands into (see `expand_group`); a larger
/// group stays one alternative.
pub(crate) const MAX_GROUP_EXPANSION: usize = 64;
/// Weight of the lane that carries definitions the query did not enumerate
/// into the canonical list. Not a ranking weight: the lane only makes such a
/// definition present in the list, and [`query_primary_order`] places it or
/// leaves it out. Small enough to change no base order beyond an exact tie.
pub(crate) const ADMISSION_LANE_WEIGHT: f32 = 1e-6;

/// Most matching lines a bounded grep scan collects for the pattern when no
/// ready trigram index can be read. Not a ranking constant: past it the scan
/// reports itself truncated and the reply says the pattern was bounded.
pub(crate) const BOUNDED_SCAN_MATCH_LIMIT: usize = 1_000;

/// Most definition sites the pattern summary line names.
const SUMMARY_DEFINITION_SITES: usize = 3;

/// (threshold / matched files) ^ decay past the threshold, 1 up to it. It
/// depends on the alternative's file list alone, never on the page.
pub(crate) fn damping_multiplier(matched_files: usize) -> f32 {
    if matched_files <= SELECTIVITY_THRESHOLD {
        return 1.0;
    }
    (SELECTIVITY_THRESHOLD as f32 / matched_files as f32).powf(SELECTIVITY_DECAY)
}

/// The base score of a position in the query's own ranking.
pub(crate) fn base_score(position: usize) -> f32 {
    1.0 / (BASE_RRF_CONSTANT + position as f32 + 1.0)
}

/// The query-only scoring policy with the admission lane added (see
/// [`ADMISSION_LANE_WEIGHT`]).
pub(crate) fn with_admission_lane(policy: ScoringPolicy) -> Result<ScoringPolicy, ScoringError> {
    policy.with_rule(
        SearchLaneKind::PatternDefinition,
        LaneScoringRule {
            weight: ADMISSION_LANE_WEIGHT,
            rrf_constant: BASE_RRF_CONSTANT,
            plan_order_index: SearchLaneKind::PatternDefinition.default_plan_order_index(),
        },
    )
}

/// Whether a definition has query relevance of its own, from its semantic
/// cosine or its lexical score for the query relative to the best of each.
/// Membership in a query lane is not relevance: a file can sit deep in a
/// lane for a single shared word.
pub(crate) fn has_query_relevance(
    semantic: Option<f32>,
    top_semantic: Option<f32>,
    lexical: Option<f32>,
    top_lexical: Option<f32>,
) -> bool {
    let share = |value: Option<f32>, top: Option<f32>, needed: f32| match (value, top) {
        (Some(value), Some(top)) if top > 0.0 => value >= top * needed,
        _ => false,
    };
    share(semantic, top_semantic, SEMANTIC_ADMISSION_SHARE)
        || share(lexical, top_lexical, LEXICAL_ADMISSION_SHARE)
}

/// How much of the candidate set the pattern's examination covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PatternExamination {
    /// A file or time bound left candidate files unexamined.
    pub capped: bool,
    pub files_examined: usize,
    pub candidate_files: usize,
}

/// One file the pattern matched, in pattern rank order.
#[derive(Clone, Debug)]
pub(crate) struct PatternFile {
    pub file: RankedFile,
    pub definition: bool,
}

impl PatternFile {
    pub(crate) fn path(&self) -> &Path {
        &self.file.lines[0].file
    }

    /// The line shown for the file: its declaration of a matched name, or
    /// else its first matching line.
    pub(crate) fn leading_line(&self) -> &GrepMatch {
        &self.file.lines[0]
    }
}

/// The pattern's complete, ranked file list, materialized before any block is
/// frozen so its positions cannot depend on the requested page.
#[derive(Clone, Debug)]
pub(crate) struct PatternList {
    /// The pattern as the agent wrote it.
    pub pattern: String,
    pub files: Vec<PatternFile>,
    pub examination: PatternExamination,
    /// The pattern's top-level alternatives, each judged on its own.
    pub alternatives: Vec<PatternAlternative>,
}

impl PatternList {
    /// Rank a collection of matched files. The data-file classifier sees the
    /// pattern: a pattern that names a JSON file or asks for JSON keeps JSON
    /// files from being demoted, as it does for a pattern-only search.
    pub(crate) fn from_collection(
        collection: GrepFileCollection,
        project_root: &Path,
        pattern: &str,
    ) -> Self {
        let examination = PatternExamination {
            capped: collection.examination_capped,
            files_examined: collection.files_examined,
            candidate_files: collection.candidate_files,
        };
        let alternatives = judge_alternatives(&collection.files, pattern);
        let ranked = regex_route::rank_collection(
            collection,
            project_root,
            pattern,
            regex_route::RecencyTiebreak::None,
        );
        let files = ranked
            .files
            .into_iter()
            .map(|file| PatternFile {
                definition: file.declares_match(),
                file,
            })
            .collect();
        Self {
            pattern: pattern.to_string(),
            files,
            examination,
            alternatives,
        }
    }

    /// The definitions of selective alternatives, at most
    /// [`MAX_PLACED_DEFINITIONS`], in alternative order.
    pub(crate) fn selective_definitions(&self) -> Vec<&AlternativeDefinition> {
        self.alternatives
            .iter()
            .filter(|alternative| alternative.is_selective())
            .flat_map(|alternative| alternative.definitions.iter())
            .take(MAX_PLACED_DEFINITIONS)
            .collect()
    }

    /// The definitions to carry into the list because the query did not
    /// enumerate them: for each selective alternative none of whose defining
    /// files is in `carried`, its best definer by pattern rank, at most
    /// [`MAX_PLACED_DEFINITIONS`] files.
    pub(crate) fn admission_definers(&self, carried: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let rank = |path: &Path| {
            self.files
                .iter()
                .position(|file| file.path() == path)
                .unwrap_or(usize::MAX)
        };
        let mut definers = Vec::new();
        for alternative in self
            .alternatives
            .iter()
            .filter(|alternative| alternative.is_selective())
        {
            if alternative
                .definitions
                .iter()
                .any(|definition| carried.contains(&definition.path))
            {
                continue;
            }
            let best = alternative
                .definitions
                .iter()
                .min_by_key(|definition| (rank(&definition.path), definition.path.clone()));
            if let Some(best) = best {
                if !definers.contains(&best.path) {
                    definers.push(best.path.clone());
                }
            }
        }
        definers.truncate(MAX_PLACED_DEFINITIONS);
        definers
    }

    /// Build the list from a bounded grep scan, used when no ready trigram
    /// index can be read. The scan reports no candidate count, so the files it
    /// searched stand in for it; `truncated` marks a scan stopped by its bound.
    pub(crate) fn from_bounded_scan(
        result: GrepResult,
        project_root: &Path,
        pattern: &str,
    ) -> Self {
        let mut files: Vec<GrepFileMatches> = Vec::new();
        for grep_match in result.matches {
            match files.iter_mut().find(|file| file.path == grep_match.file) {
                Some(file) => {
                    file.matched_lines += 1;
                    file.matches.push(grep_match);
                }
                None => files.push(GrepFileMatches {
                    path: grep_match.file.clone(),
                    modified: std::time::SystemTime::UNIX_EPOCH,
                    matches: vec![grep_match],
                    matched_lines: 1,
                }),
            }
        }
        // A scan stopped by its match bound, its walk bound or grep's
        // deadline left files unread.
        let capped = result.truncated
            || result.walk_truncated
            || result.scan_deadline_reached
            || result.walk_bound.is_some();
        Self::from_collection(
            GrepFileCollection {
                files,
                candidate_files: result.files_searched,
                files_examined: result.files_searched,
                examination_capped: capped,
                fully_degraded: result.fully_degraded,
                index_status: result.index_status,
                missing_on_disk: result.missing_on_disk,
            },
            project_root,
            pattern,
        )
    }

    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn paths(&self) -> HashSet<PathBuf> {
        self.files
            .iter()
            .map(|file| file.path().to_path_buf())
            .collect()
    }

    pub(crate) fn file(&self, path: &Path) -> Option<&PatternFile> {
        self.files.iter().find(|file| file.path() == path)
    }

    /// The admission lane's candidates: the given admitted definition files,
    /// file-level like every other split candidate, in the order given.
    pub(crate) fn admission_candidates(paths: &[PathBuf]) -> Vec<CandidateResult> {
        paths
            .iter()
            .map(|path| CandidateResult {
                path: path.clone(),
                symbol_range: None,
                evidence: EvidenceDescriptor::for_non_exact(false, false),
                fusion_score: None,
                // The admission lane carries presence, not a graded score.
                lane_score: Some(0.0),
                best_lane: Some(SearchLaneKind::PatternDefinition),
            })
            .collect()
    }

    /// One line describing the pattern's matches, computed from the pattern
    /// list and the set of files the prose lanes enumerated, never from the
    /// page, so every page of one search shows the same line.
    pub(crate) fn summary_line(
        &self,
        prose_found: &HashSet<PathBuf>,
        project_root: &Path,
    ) -> String {
        let pattern = &self.pattern;
        let examined = &self.examination;
        if self.files.is_empty() {
            return if examined.capped {
                format!(
                    "[pattern `{pattern}`: no match in the {} of {} candidate files examined; files not examined were not searched]",
                    examined.files_examined, examined.candidate_files
                )
            } else {
                format!("[pattern `{pattern}`: no match]")
            };
        }
        let files = self.files.len();
        let overlap = self
            .files
            .iter()
            .filter(|file| prose_found.contains(file.path()))
            .count();
        let mut line = format!(
            "[pattern `{pattern}`: {files} file{} matched, {overlap} also found by the query",
            if files == 1 { "" } else { "s" }
        );
        let sites = self
            .definition_sites()
            .into_iter()
            .take(SUMMARY_DEFINITION_SITES)
            .map(|(path, line)| {
                format!(
                    "{}:{}",
                    path.strip_prefix(project_root).unwrap_or(&path).display(),
                    line
                )
            })
            .collect::<Vec<_>>();
        if sites.is_empty() {
            line.push_str("; no definition found");
        } else {
            line.push_str("; defined in ");
            line.push_str(&sites.join(", "));
        }
        if examined.capped {
            line.push_str(&format!(
                "; examined {} of {} candidate files",
                examined.files_examined, examined.candidate_files
            ));
        }
        line.push(']');
        line
    }

    /// Every file that defines something the pattern names, one site per
    /// file, best first; the summary line names the first few and the reply's
    /// `pattern_summary` counts them all.
    ///
    /// 1. Declarations of a selective alternative, in pattern rank order. A
    ///    selective alternative names something specific, and its
    ///    declarations include longer names it matches
    ///    (`runAutoSearchHintForPi` for `runAutoSearch`) and exclude local
    ///    variables (see `judge_alternatives`).
    /// 2. Files whose leading line declares a matched name as a whole, in
    ///    pattern rank order, unless that line is a local variable inside a
    ///    function body. A broad alternative such as `Error` contributes only
    ///    here, so a name that merely contains it is not listed.
    /// 3. Only when both are empty, the local variables from step 2, so the
    ///    line still names the one declaration there is.
    pub(crate) fn definition_sites(&self) -> Vec<(PathBuf, u32)> {
        let rank = |path: &Path| {
            self.files
                .iter()
                .position(|file| file.path() == path)
                .unwrap_or(usize::MAX)
        };
        let mut selective = self
            .alternatives
            .iter()
            .filter(|alternative| alternative.is_selective())
            .flat_map(|alternative| alternative.definitions.iter())
            .map(|definition| (definition.path.clone(), definition.line))
            .collect::<Vec<_>>();
        selective.sort_by_key(|(path, line)| (rank(path), *line));
        let (locals, declared): (Vec<_>, Vec<_>) = self
            .files
            .iter()
            .filter(|file| file.definition)
            .map(|file| file.leading_line())
            .partition(|leading| is_local_binding(&leading.line_text));
        let whole_name = |lines: Vec<&GrepMatch>| {
            lines
                .into_iter()
                .map(|leading| (leading.file.clone(), leading.line))
                .collect::<Vec<_>>()
        };
        let mut sites = selective;
        sites.extend(whole_name(declared));
        if sites.is_empty() {
            sites = whole_name(locals);
        }
        let mut seen = HashSet::new();
        sites.retain(|(path, _)| seen.insert(path.clone()));
        sites
    }
}

/// Which input found a result, from explicit membership in the pattern's file
/// list and in the prose lanes' enumerations (not from fusion contributions,
/// which admission can drop).
pub(crate) fn matched_by(
    path: &Path,
    pattern_found: &HashSet<PathBuf>,
    prose_found: &HashSet<PathBuf>,
) -> &'static str {
    match (pattern_found.contains(path), prose_found.contains(path)) {
        (true, true) => "both",
        (true, false) => "pattern",
        _ => "query",
    }
}

/// Split a pattern at its top-level `|`: the alternation grep would try as
/// separate branches. A `|` inside a group, a character class or after a
/// backslash stays inside its alternative. A pattern with no top-level `|` is
/// one alternative.
pub(crate) fn top_level_alternatives(pattern: &str) -> Vec<String> {
    let mut alternatives = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut in_class = false;
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            '[' if !in_class => {
                in_class = true;
                current.push(ch);
                // A `]` right after `[` or `[^` is a literal member.
                if chars.peek() == Some(&'^') {
                    current.push(chars.next().expect("peeked"));
                }
                if chars.peek() == Some(&']') {
                    current.push(chars.next().expect("peeked"));
                }
                continue;
            }
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth = depth.saturating_sub(1),
            '|' if !in_class && depth == 0 => {
                alternatives.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    alternatives.push(current);
    alternatives
}

/// Expand an alternative with one group into one alternative per branch of
/// the group: `P(A|B)S` becomes `PAS` and `PBS`, which match the same lines.
/// This applies only when it is plainly the same regex: exactly one group,
/// plain or non-capturing, not nested, with a `|` at its own top level, not
/// followed by a quantifier, with no backreference outside it, and at most
/// [`MAX_GROUP_EXPANSION`] branches. Otherwise `None`, and the alternative
/// stays whole.
fn expand_group(alternative: &str) -> Option<Vec<String>> {
    let chars = alternative.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut in_class = false;
    let mut depth = 0usize;
    let mut group = None;
    let mut groups = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' {
            index += 2;
            continue;
        }
        if in_class {
            in_class = ch != ']';
            index += 1;
            continue;
        }
        match ch {
            '[' => {
                in_class = true;
                // A `]` right after `[` or `[^` is a literal member.
                if chars.get(index + 1) == Some(&'^') {
                    index += 1;
                }
                if chars.get(index + 1) == Some(&']') {
                    index += 1;
                }
            }
            '(' => {
                depth += 1;
                if depth > 1 {
                    return None;
                }
                groups += 1;
                group = Some((index, None));
            }
            ')' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                group = group.map(|(open, _)| (open, Some(index)));
            }
            _ => {}
        }
        index += 1;
    }
    let (open, close) = match group {
        Some((open, Some(close))) if groups == 1 && depth == 0 => (open, close),
        _ => return None,
    };
    if matches!(chars.get(close + 1), Some('*' | '+' | '?' | '{')) {
        return None;
    }
    let body_start = match (chars.get(open + 1), chars.get(open + 2)) {
        (Some('?'), Some(':')) => open + 3,
        (Some('?'), _) => return None,
        _ => open + 1,
    };
    let prefix = chars[..open].iter().collect::<String>();
    let suffix = chars[close + 1..].iter().collect::<String>();
    let backreference = |text: &str| {
        text.as_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'\\' && pair[1].is_ascii_digit())
    };
    if backreference(&prefix) || backreference(&suffix) {
        return None;
    }
    let body = chars[body_start..close].iter().collect::<String>();
    let branches = top_level_alternatives(&body);
    if branches.len() < 2 || branches.len() > MAX_GROUP_EXPANSION {
        return None;
    }
    Some(
        branches
            .iter()
            .map(|branch| format!("{prefix}{branch}{suffix}"))
            .collect(),
    )
}

/// A keyword declaration of a name one alternative matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AlternativeDefinition {
    pub path: PathBuf,
    pub name: String,
    pub line: u32,
}

/// One top-level alternative of the pattern, judged on its own: a generic
/// alternative such as `Error` beside `tracked_files` matches hundreds of
/// files and defines dozens of names, and none of that may decide anything
/// for `tracked_files`.
#[derive(Clone, Debug)]
pub(crate) struct PatternAlternative {
    pub text: String,
    /// Files with a line this alternative matches.
    pub matched_files: usize,
    /// The same files, for membership tests.
    pub files: HashSet<PathBuf>,
    /// One entry per file that declares a name this alternative matches.
    pub definitions: Vec<AlternativeDefinition>,
}

impl PatternAlternative {
    /// A selective alternative matches few enough files to name something
    /// specific, and has at least one definition.
    pub(crate) fn is_selective(&self) -> bool {
        !self.definitions.is_empty() && self.matched_files <= SELECTIVE_ALTERNATIVE_MAX_FILES
    }

    /// The share of the definition bonus `path` gets, or `None` when it
    /// declares nothing this alternative matches: 1 / n^2 for a name declared
    /// in n files, so a unique declaration gets all of it and two declarers
    /// of the same name a quarter each. Declarers of different names do not
    /// share; breadth is handled by damping on the matched-file count.
    pub(crate) fn definition_share(&self, path: &Path) -> Option<f32> {
        self.definitions
            .iter()
            .filter(|definition| definition.path == path)
            .map(|definition| {
                let declarers = self
                    .definitions
                    .iter()
                    .filter(|other| other.name == definition.name)
                    .count()
                    .max(1) as f32;
                1.0 / (declarers * declarers)
            })
            .reduce(f32::max)
    }
}

fn compiled_matches(pattern: &crate::pattern_compile::CompiledPattern, text: &str) -> bool {
    use crate::pattern_compile::CompiledPattern;
    match pattern {
        CompiledPattern::Regex { compiled, .. } => compiled.is_match(text.as_bytes()),
        CompiledPattern::Literal(literal) => {
            let haystack = text.as_bytes();
            let needle = literal.needle.as_slice();
            if needle.is_empty() {
                return true;
            }
            haystack.windows(needle.len()).any(|window| {
                if literal.case_insensitive_ascii {
                    window.eq_ignore_ascii_case(needle)
                } else {
                    window == needle
                }
            })
        }
    }
}

/// A `let`, `const` or `var` binding written inside a block (indented, with
/// no visibility or export modifier) is a local variable of a function body,
/// not a definition a reader looks up by name. `const fn` is a function.
fn is_local_binding(line: &str) -> bool {
    fn word(text: &str) -> (&str, &str) {
        let end = text
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(text.len());
        (&text[..end], text[end..].trim_start())
    }
    let trimmed = line.trim_start();
    if trimmed.len() == line.len() {
        return false;
    }
    let (keyword, rest) = word(trimmed);
    matches!(keyword, "let" | "const" | "var") && word(rest).0 != "fn"
}

/// Judge each top-level alternative of `pattern` over the collected files.
/// Every file keeps its first matching lines and any later keyword
/// declaration (see `regex_route::keep_past_line_limit`), so definitions are
/// complete; an alternative that matches only past a file's kept lines is not
/// counted for that file.
fn judge_alternatives(files: &[GrepFileMatches], pattern: &str) -> Vec<PatternAlternative> {
    use crate::pattern_compile::{compile, CompileOpts, CompileResult};
    let texts = top_level_alternatives(pattern)
        .into_iter()
        .flat_map(|text| expand_group(&text).unwrap_or_else(|| vec![text]))
        .collect::<Vec<_>>();
    let compiled = texts
        .iter()
        .map(|text| match compile(text, CompileOpts::default()) {
            CompileResult::Ok(compiled) => Some(compiled),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    // An alternative that does not compile on its own leaves the whole
    // pattern as one alternative.
    let (texts, compiled) = match compiled {
        Some(compiled) if texts.len() > 1 => (texts, compiled),
        _ => match compile(pattern, CompileOpts::default()) {
            CompileResult::Ok(compiled) => (vec![pattern.to_string()], vec![compiled]),
            _ => return Vec::new(),
        },
    };
    texts
        .into_iter()
        .zip(compiled)
        .map(|(text, matcher)| {
            let mut matched_files = 0;
            let mut matched_paths = HashSet::new();
            let mut definitions = Vec::new();
            for file in files {
                let mut matched = false;
                let mut defined = false;
                for grep_match in &file.matches {
                    if compiled_matches(&matcher, &grep_match.match_text) {
                        matched = true;
                    }
                    if defined {
                        continue;
                    }
                    let Some((name, regex_route::Declaration::Keyword)) =
                        regex_route::declared_name(&grep_match.line_text)
                    else {
                        continue;
                    };
                    if is_local_binding(&grep_match.line_text) {
                        continue;
                    }
                    // The alternative names this declaration when it matches
                    // the declared name itself, which may be longer than the
                    // match (`runAutoSearch` names `runAutoSearchHintForPi`),
                    // or when the matched text holds the whole name and the
                    // alternative is written as the declaration
                    // (`^pub struct Name`, `fn name`) rather than as the name.
                    if compiled_matches(&matcher, name)
                        || (regex_route::match_declaration(grep_match)
                            == Some(regex_route::Declaration::Keyword)
                            && compiled_matches(&matcher, &grep_match.match_text))
                    {
                        defined = true;
                        definitions.push(AlternativeDefinition {
                            path: file.path.clone(),
                            name: name.to_string(),
                            line: grep_match.line,
                        });
                    }
                }
                if matched || defined {
                    matched_files += 1;
                    matched_paths.insert(file.path.clone());
                }
            }
            PatternAlternative {
                text,
                matched_files,
                files: matched_paths,
                definitions,
            }
        })
        .collect()
}

/// One entry of the canonical list's first block (the leading entries every
/// request builds, whatever its page), as the query ranked it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseCandidate {
    pub path: PathBuf,
    /// Position in the query's own ranking; `None` for a definition only the
    /// admission lane carried into the list.
    pub base_position: Option<usize>,
    /// Index of the entry in the block, to put entries back in the new order.
    pub entry: usize,
    /// The entry is in the exact tier (verbatim evidence for the query).
    /// The exact tier keeps its place and order ahead of every other entry.
    pub exact: bool,
}

/// Where one candidate was placed, and why.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct Placement {
    #[serde(skip)]
    pub entry: usize,
    pub path: PathBuf,
    pub base_position: Option<usize>,
    /// From the candidate's own matches of selective alternatives.
    pub anchor_bonus: f32,
    /// A definition the query did not rank near the top, placed by the
    /// pattern (after a mentioning host, or at an admission position).
    pub admitted: bool,
    /// The leading query result that mentions the alternative this
    /// definition answers, which the definition is placed right after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports: Option<PathBuf>,
    pub score: f32,
}

/// Order the first block of a split request's canonical list.
///
/// `candidates` is that block in the query's own order. `qualified` holds
/// the selective definitions with query relevance of their own (see
/// [`has_query_relevance`]); `semantic_absent` says the query ran without a
/// semantic lane. Everything depends only on the query's ranking and the
/// pattern list, never on the page, the clock or the call graph.
///
/// 1. A leading query result (base position below [`PROSE_TOP_K`]) scores
///    [`base_score`] of its position plus, per selective alternative it
///    matches, the mention bonus, or for a declarer the larger of that and
///    its definition share. Exact-tier entries keep their place and order
///    ahead of every other entry.
/// 2. A declarer never passes a leading result that mentions the same
///    alternative and that the query ranked above it.
/// 3. Per selective alternative, the host is the best-placed leading result
///    that matches it. When the host declares the alternative itself nothing
///    moves. Otherwise the alternative's best definer is placed immediately
///    after the host (a call site always mentions the name it calls, so this
///    is the caller relation without the call graph), never above it.
/// 4. With no host, the best definer is admitted only when the query has no
///    semantic lane (at [`ADMISSION_BASE_POSITION_WITHOUT_SEMANTIC`]) or it is
///    qualified (at [`ADMISSION_BASE_POSITION`]). Otherwise it keeps its base
///    place if the query ranked it, and is left out if only the admission
///    lane carried it; the pattern summary line still names it.
pub(crate) fn query_primary_order(
    candidates: &[BaseCandidate],
    alternatives: &[PatternAlternative],
    qualified: &HashSet<PathBuf>,
    semantic_absent: bool,
) -> Vec<Placement> {
    let selective = alternatives
        .iter()
        .filter(|alternative| alternative.is_selective())
        .collect::<Vec<_>>();
    let near_top = |placement: &Placement| {
        placement
            .base_position
            .is_some_and(|position| position < PROSE_TOP_K)
    };
    let mut exact_tier = Vec::new();
    let mut ranked = Vec::new();
    let mut held = Vec::new();
    for candidate in candidates {
        let mut placement = Placement {
            entry: candidate.entry,
            path: candidate.path.clone(),
            base_position: candidate.base_position,
            anchor_bonus: 0.0,
            admitted: false,
            supports: None,
            score: 0.0,
        };
        if near_top(&placement) {
            for alternative in &selective {
                if !alternative.files.contains(&candidate.path) {
                    continue;
                }
                let damping = damping_multiplier(alternative.matched_files);
                let mention = MENTION_ANCHOR_BOOST * damping;
                placement.anchor_bonus += match alternative.definition_share(&candidate.path) {
                    Some(share) => mention.max(DEFINITION_ANCHOR_BOOST * damping * share),
                    None => mention,
                };
            }
        }
        placement.score = candidate.base_position.map_or(0.0, base_score) + placement.anchor_bonus;
        match (candidate.base_position, candidate.exact) {
            (None, _) => held.push(placement),
            (Some(_), true) => exact_tier.push(placement),
            (Some(_), false) => ranked.push(placement),
        }
    }
    ranked.sort_by(placement_order);

    // 2. No declarer passes a mentioning result the query ranked above it.
    for alternative in &selective {
        let declares = |path: &Path| alternative.definition_share(path).is_some();
        let definers = ranked
            .iter()
            .filter(|placement| declares(&placement.path))
            .map(|placement| placement.path.clone())
            .collect::<Vec<_>>();
        for definer in definers {
            let Some(at) = ranked
                .iter()
                .position(|placement| placement.path == definer)
            else {
                continue;
            };
            let base = ranked[at].base_position.unwrap_or(usize::MAX);
            let last_passed = ranked
                .iter()
                .enumerate()
                .skip(at + 1)
                .filter(|(_, placement)| {
                    near_top(placement)
                        && alternative.files.contains(&placement.path)
                        && !declares(&placement.path)
                        && placement
                            .base_position
                            .is_some_and(|position| position < base)
                })
                .map(|(index, _)| index)
                .last();
            if let Some(last) = last_passed {
                let moved = ranked.remove(at);
                ranked.insert(last, moved);
            }
        }
    }

    // 3 and 4. Place each selective alternative's best definer.
    for alternative in &selective {
        let declares = |path: &Path| alternative.definition_share(path).is_some();
        let host = exact_tier
            .iter()
            .chain(ranked.iter())
            .position(|placement| {
                near_top(placement) && alternative.files.contains(&placement.path)
            });
        let host_declares = host.is_some_and(|host| {
            declares(
                &exact_tier
                    .iter()
                    .chain(ranked.iter())
                    .nth(host)
                    .expect("host")
                    .path,
            )
        });
        if host_declares {
            continue;
        }
        // The best definer: the best-placed declarer in the list, else the
        // one the admission lane carried. A declarer already placed for an
        // earlier alternative, or in the exact tier, stays where it is.
        let in_ranked = ranked
            .iter()
            .position(|placement| declares(&placement.path));
        let in_held = held.iter().position(|placement| declares(&placement.path));
        let mut definer = match (in_ranked, in_held) {
            (Some(at), _) if ranked[at].supports.is_none() && !ranked[at].admitted => {
                ranked.remove(at)
            }
            (None, Some(at)) => held.remove(at),
            _ => continue,
        };
        match host {
            Some(host) => {
                let host_path = exact_tier
                    .iter()
                    .chain(ranked.iter())
                    .nth(host)
                    .expect("host")
                    .path
                    .clone();
                let host_in_ranked = ranked
                    .iter()
                    .position(|placement| placement.path == host_path);
                // Right after the host and any definition already placed
                // after it; a host in the exact tier puts it first after
                // that tier.
                let mut index = host_in_ranked.map_or(0, |at| at + 1);
                while ranked
                    .get(index)
                    .is_some_and(|placement| placement.supports.as_ref() == Some(&host_path))
                {
                    index += 1;
                }
                definer.admitted = !near_top(&definer);
                definer.supports = Some(host_path);
                ranked.insert(index, definer);
            }
            None => {
                let position = if semantic_absent {
                    Some(ADMISSION_BASE_POSITION_WITHOUT_SEMANTIC)
                } else if qualified.contains(&definer.path) {
                    Some(ADMISSION_BASE_POSITION)
                } else {
                    None
                };
                match (position, definer.base_position) {
                    (Some(position), _) => {
                        definer.admitted = true;
                        definer.score = base_score(position) + definer.anchor_bonus;
                        let index = ranked
                            .iter()
                            .position(|placement| placement.score < definer.score)
                            .unwrap_or(ranked.len());
                        ranked.insert(index, definer);
                    }
                    // Not admitted: a definer the query ranked keeps its
                    // place; one only the admission lane carried is left out.
                    (None, Some(_)) => {
                        let index = ranked
                            .iter()
                            .position(|placement| placement_order(placement, &definer).is_gt())
                            .unwrap_or(ranked.len());
                        ranked.insert(index, definer);
                    }
                    (None, None) => {}
                }
            }
        }
    }
    exact_tier.extend(ranked);
    exact_tier
}

/// Higher score first, then the query's own order, then path.
fn placement_order(left: &Placement, right: &Placement) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| {
            left.base_position
                .unwrap_or(usize::MAX)
                .cmp(&right.base_position.unwrap_or(usize::MAX))
        })
        .then_with(|| left.path.cmp(&right.path))
}

/// The identity a split request adds to the canonical list key: the pattern
/// and the flags it was compiled with. Paging fields are never part of it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SplitListIdentity {
    pub pattern: String,
    pub case_insensitive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    use crate::search_index::IndexStatus;

    use super::super::blocks::CanonicalListKey;

    fn grep_match(path: &str, line: u32, text: &str, matched: &str) -> GrepMatch {
        GrepMatch {
            file: PathBuf::from(path),
            line,
            column: 1,
            line_text: text.to_string(),
            match_text: matched.to_string(),
        }
    }

    fn matched_file(path: &str, lines: Vec<GrepMatch>, age_secs: u64) -> GrepFileMatches {
        GrepFileMatches {
            path: PathBuf::from(path),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
            matched_lines: lines.len(),
            matches: lines,
        }
    }

    fn collection(files: Vec<GrepFileMatches>, capped: bool) -> GrepFileCollection {
        GrepFileCollection {
            candidate_files: files.len() + usize::from(capped) * 10,
            files_examined: files.len(),
            files,
            examination_capped: capped,
            fully_degraded: false,
            index_status: IndexStatus::Ready,
            missing_on_disk: 0,
        }
    }

    fn mention(path: &str) -> GrepFileMatches {
        matched_file(path, vec![grep_match(path, 3, "  load();", "load")], 0)
    }

    fn definition(path: &str) -> GrepFileMatches {
        matched_file(
            path,
            vec![grep_match(path, 7, "pub fn load() {", "load")],
            0,
        )
    }

    #[test]
    fn split_pattern_order_ignores_modification_time() {
        let files = |young: &str, old: &str| {
            vec![
                matched_file(young, vec![grep_match(young, 1, "load()", "load")], 0),
                matched_file(old, vec![grep_match(old, 1, "load()", "load")], 500),
            ]
        };
        // `b.rs` is newest in the first run and oldest in the second.
        let first = PatternList::from_collection(
            collection(files("/p/src/b.rs", "/p/src/a.rs"), false),
            Path::new("/p"),
            "load",
        );
        let second = PatternList::from_collection(
            collection(files("/p/src/a.rs", "/p/src/b.rs"), false),
            Path::new("/p"),
            "load",
        );
        let order = |list: &PatternList| {
            list.files
                .iter()
                .map(|file| file.path().display().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&first), ["/p/src/a.rs", "/p/src/b.rs"]);
        assert_eq!(order(&first), order(&second));
        // The pattern-only route keeps its newest-first tie-break.
        let recency = regex_route::rank_collection(
            collection(files("/p/src/b.rs", "/p/src/a.rs"), false),
            Path::new("/p"),
            "load",
            regex_route::RecencyTiebreak::NewestFirst,
        );
        assert_eq!(recency.files[0].lines[0].file, PathBuf::from("/p/src/b.rs"));
    }

    #[test]
    fn damping_is_full_up_to_the_threshold_then_decays() {
        assert_eq!(damping_multiplier(0), 1.0);
        assert_eq!(damping_multiplier(SELECTIVITY_THRESHOLD), 1.0);
        let past = damping_multiplier(SELECTIVITY_THRESHOLD * 2);
        assert!(past > 0.0 && past < 1.0, "{past}");
        assert!(damping_multiplier(SELECTIVITY_THRESHOLD * 30) < past);
    }

    fn key() -> CanonicalListKey {
        CanonicalListKey {
            project_root: PathBuf::from("/p"),
            snapshot_generation: "g".to_string(),
            normalized_query: "how are loads scheduled".to_string(),
            include_tests: false,
            split: Some(SplitListIdentity {
                pattern: "load".to_string(),
                case_insensitive: false,
            }),
        }
    }

    #[test]
    fn a_pattern_splits_at_its_top_level_alternation_only() {
        assert_eq!(top_level_alternatives("a|b|c"), ["a", "b", "c"]);
        assert_eq!(
            top_level_alternatives("tracked_files|Error"),
            ["tracked_files", "Error"]
        );
        assert_eq!(top_level_alternatives("(a|b)|c"), ["(a|b)", "c"]);
        assert_eq!(top_level_alternatives("[|]x|y"), ["[|]x", "y"]);
        assert_eq!(top_level_alternatives(r"[]|]x|y"), [r"[]|]x", "y"]);
        assert_eq!(top_level_alternatives(r"a\|b"), [r"a\|b"]);
        assert_eq!(top_level_alternatives("load"), ["load"]);
    }

    fn declares(path: &str, name: &str) -> GrepFileMatches {
        matched_file(
            path,
            vec![grep_match(path, 4, &format!("pub fn {name}() {{"), name)],
            0,
        )
    }

    fn uses(path: &str, name: &str) -> GrepFileMatches {
        matched_file(
            path,
            vec![grep_match(path, 9, &format!("    {name}();"), name)],
            0,
        )
    }

    /// `tracked_files|Error`: the generic alternative matches many files and
    /// defines several names, and none of it changes how `tracked_files` is
    /// judged.
    #[test]
    fn each_alternative_is_judged_on_its_own() {
        let mut files = vec![
            declares("/p/src/backup.rs", "tracked_files"),
            uses("/p/src/status.rs", "tracked_files"),
        ];
        for index in 0..60 {
            files.push(uses(&format!("/p/src/e{index:02}.rs"), "Error"));
        }
        for index in 0..5 {
            files.push(declares(&format!("/p/src/err{index}.rs"), "Error"));
        }
        let list = PatternList::from_collection(
            collection(files, false),
            Path::new("/p"),
            "tracked_files|Error",
        );
        let [tracked, error] = &list.alternatives[..] else {
            panic!("two alternatives: {:?}", list.alternatives);
        };
        assert_eq!(tracked.text, "tracked_files");
        assert_eq!(tracked.matched_files, 2);
        assert!(tracked.files.contains(Path::new("/p/src/status.rs")));
        assert!(!tracked.files.contains(Path::new("/p/src/e00.rs")));
        assert_eq!(tracked.definitions.len(), 1);
        assert_eq!(
            tracked.definitions[0].path,
            PathBuf::from("/p/src/backup.rs")
        );
        assert_eq!(tracked.definitions[0].name, "tracked_files");
        assert!(tracked.is_selective());
        assert_eq!(error.matched_files, 65);
        assert_eq!(error.definitions.len(), 5);
        assert!(
            !error.is_selective(),
            "a generic alternative is not selective"
        );
        // Only the selective alternative's definition is looked up.
        let looked_up = list.selective_definitions();
        assert_eq!(looked_up.len(), 1);
        assert_eq!(looked_up[0].name, "tracked_files");
    }

    /// The query's own order over `paths`, as the first block.
    fn base(paths: &[&str]) -> Vec<BaseCandidate> {
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| BaseCandidate {
                path: PathBuf::from(path),
                base_position: Some(index),
                entry: index,
                exact: false,
            })
            .collect()
    }

    fn query_paths(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("/p/q{index:02}.rs"))
            .collect()
    }

    /// An alternative matching `files`, of which `defining` declare `name`.
    fn alternative(name: &str, files: &[&str], defining: &[&str]) -> PatternAlternative {
        PatternAlternative {
            text: name.to_string(),
            matched_files: files.len(),
            files: files.iter().map(PathBuf::from).collect(),
            definitions: defining
                .iter()
                .map(|path| AlternativeDefinition {
                    path: PathBuf::from(path),
                    name: name.to_string(),
                    line: 1,
                })
                .collect(),
        }
    }

    fn order(placements: &[Placement]) -> Vec<String> {
        placements
            .iter()
            .map(|placement| placement.path.display().to_string())
            .collect()
    }

    fn placed<'a>(placements: &'a [Placement], path: &str) -> (usize, &'a Placement) {
        placements
            .iter()
            .enumerate()
            .find(|(_, placement)| placement.path == Path::new(path))
            .unwrap_or_else(|| panic!("{path} not placed: {:?}", order(placements)))
    }

    fn ten() -> (Vec<String>, Vec<BaseCandidate>) {
        let paths = query_paths(10);
        let refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let candidates = base(&refs);
        (paths, candidates)
    }

    #[test]
    fn without_selective_evidence_the_order_is_the_query_order() {
        let (paths, candidates) = ten();
        let none = HashSet::new();
        assert_eq!(
            order(&query_primary_order(&candidates, &[], &none, false)),
            paths
        );
        // An alternative matching too many files is not selective: no bonus.
        let mut broad = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let usages = (0..SELECTIVE_ALTERNATIVE_MAX_FILES)
            .map(|index| format!("/p/use{index}.rs"))
            .collect::<Vec<_>>();
        broad.extend(usages.iter().map(String::as_str));
        let placements = query_primary_order(
            &candidates,
            &[alternative("Error", &broad, &["/p/q09.rs"])],
            &none,
            false,
        );
        assert_eq!(order(&placements), paths);
        assert!(placements
            .iter()
            .all(|placement| placement.anchor_bonus == 0.0));
    }

    /// A stale name, which nothing declares any more, is not selective: the
    /// query results that still mention it gain nothing, and the query's
    /// order stands.
    #[test]
    fn a_stale_mention_does_not_move_the_list() {
        let (paths, candidates) = ten();
        let placements = query_primary_order(
            &candidates,
            &[alternative("old_name", &["/p/stale.rs", "/p/q05.rs"], &[])],
            &HashSet::new(),
            false,
        );
        assert_eq!(order(&placements), paths);
    }

    /// A leading result's own mention lifts it by a couple of positions,
    /// never past the query's leader; past the leading results it gets
    /// nothing; and among many matched files the bonus is damped below a
    /// neighbouring gap.
    #[test]
    fn a_leading_results_own_mention_lifts_it_by_a_bounded_amount() {
        let paths = query_paths(30);
        let refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let placements = query_primary_order(
            &base(&refs),
            &[alternative(
                "load",
                &["/p/q03.rs", "/p/q20.rs", "/p/q00.rs"],
                &["/p/q00.rs"],
            )],
            &HashSet::new(),
            false,
        );
        let (rank, mention) = placed(&placements, "/p/q03.rs");
        assert_eq!(mention.anchor_bonus, MENTION_ANCHOR_BOOST);
        assert!((1..3).contains(&rank), "{:?}", order(&placements));
        assert_eq!(placements[0].path, Path::new("/p/q00.rs"));
        assert_eq!(placed(&placements, "/p/q20.rs").1.anchor_bonus, 0.0);
        let many = (0..40)
            .map(|index| format!("/p/use{index}.rs"))
            .chain(["/p/q05.rs".to_string(), "/p/q00.rs".to_string()])
            .collect::<Vec<_>>();
        let many = many.iter().map(String::as_str).collect::<Vec<_>>();
        let placements = query_primary_order(
            &base(&refs),
            &[alternative("load", &many, &["/p/q00.rs"])],
            &HashSet::new(),
            false,
        );
        assert_eq!(order(&placements), paths);
    }

    /// A declarer's bonus is shared 1/n^2 among the n files declaring the
    /// same name, and never falls below the mention bonus; declarers of
    /// different names do not share.
    #[test]
    fn a_declarer_never_gets_less_than_a_mention() {
        let (_, candidates) = ten();
        for (count, expected) in [
            (1, DEFINITION_ANCHOR_BOOST),
            (2, MENTION_ANCHOR_BOOST.max(DEFINITION_ANCHOR_BOOST / 4.0)),
            (3, MENTION_ANCHOR_BOOST.max(DEFINITION_ANCHOR_BOOST / 9.0)),
        ] {
            let defining = ["/p/q05.rs", "/p/q06.rs", "/p/q07.rs"][..count].to_vec();
            let placements = query_primary_order(
                &candidates,
                &[alternative("load", &defining, &defining)],
                &HashSet::new(),
                false,
            );
            for path in &defining {
                let bonus = placed(&placements, path).1.anchor_bonus;
                assert!((bonus - expected).abs() < 1e-9, "{count}: {bonus}");
                assert!(bonus >= MENTION_ANCHOR_BOOST);
            }
        }
        let mut two_names = alternative("load_\\w+", &["/p/q05.rs", "/p/q06.rs"], &[]);
        for (path, name) in [("/p/q05.rs", "load_a"), ("/p/q06.rs", "load_b")] {
            two_names.definitions.push(AlternativeDefinition {
                path: PathBuf::from(path),
                name: name.to_string(),
                line: 1,
            });
        }
        let placements = query_primary_order(&candidates, &[two_names], &HashSet::new(), false);
        assert_eq!(
            placed(&placements, "/p/q06.rs").1.anchor_bonus,
            DEFINITION_ANCHOR_BOOST
        );
    }

    /// The caller and definition of the same name: the definer goes right
    /// after the best-placed result that mentions the name, never above it,
    /// passing the results between, and the mentioning result keeps or
    /// improves its place.
    #[test]
    fn the_definer_is_placed_right_after_its_mentioning_host() {
        let (_, candidates) = ten();
        let alternatives = [alternative(
            "shutdown_all",
            &["/p/q03.rs", "/p/q07.rs", "/p/q08.rs"],
            &["/p/q08.rs"],
        )];
        let placements = query_primary_order(&candidates, &alternatives, &HashSet::new(), false);
        let (host, _) = placed(&placements, "/p/q03.rs");
        let (rank, definer) = placed(&placements, "/p/q08.rs");
        assert!(host <= 3, "{:?}", order(&placements));
        assert_eq!(rank, host + 1, "{:?}", order(&placements));
        assert_eq!(definer.supports.as_deref(), Some(Path::new("/p/q03.rs")));
        assert!(!definer.admitted, "the query ranked it near the top");
    }

    /// A declarer never passes a mentioning result the query ranked above
    /// it, even when its own bonus is larger.
    #[test]
    fn a_declarer_never_passes_a_mentioning_result_ranked_above_it() {
        let (_, candidates) = ten();
        let placements = query_primary_order(
            &candidates,
            &[alternative(
                "load",
                &["/p/q03.rs", "/p/q04.rs"],
                &["/p/q04.rs"],
            )],
            &HashSet::new(),
            false,
        );
        let (mention, _) = placed(&placements, "/p/q03.rs");
        let (definer, _) = placed(&placements, "/p/q04.rs");
        assert_eq!(definer, mention + 1, "{:?}", order(&placements));
    }

    /// A definer that is itself the best-placed result mentioning the name
    /// hosts itself: it keeps its lead and nothing is placed after it.
    #[test]
    fn a_self_hosted_definer_keeps_its_place() {
        let (_, candidates) = ten();
        let placements = query_primary_order(
            &candidates,
            &[alternative(
                "load",
                &["/p/q01.rs", "/p/q05.rs"],
                &["/p/q01.rs"],
            )],
            &HashSet::new(),
            false,
        );
        let (rank, definer) = placed(&placements, "/p/q01.rs");
        assert!(rank <= 1, "{:?}", order(&placements));
        assert!(definer.supports.is_none());
        assert!(placements
            .iter()
            .all(|placement| placement.supports.is_none()));
    }

    /// A definition only the admission lane carried is placed after its
    /// mentioning host; with no host it is admitted only when the query has
    /// no semantic lane or the definition is relevant on its own, and is
    /// otherwise left out of the list.
    #[test]
    fn a_carried_definition_is_placed_by_host_or_admitted_or_left_out() {
        let (_, mut candidates) = ten();
        candidates.push(BaseCandidate {
            path: PathBuf::from("/p/defines.rs"),
            base_position: None,
            entry: 10,
            exact: false,
        });
        let hosted = [alternative(
            "load",
            &["/p/defines.rs", "/p/q02.rs"],
            &["/p/defines.rs"],
        )];
        let placements = query_primary_order(&candidates, &hosted, &HashSet::new(), false);
        let (host, _) = placed(&placements, "/p/q02.rs");
        let (rank, definer) = placed(&placements, "/p/defines.rs");
        assert_eq!(rank, host + 1);
        assert!(definer.admitted);
        assert_eq!(definer.supports.as_deref(), Some(Path::new("/p/q02.rs")));

        let unhosted = [alternative("load", &["/p/defines.rs"], &["/p/defines.rs"])];
        let without_semantic = query_primary_order(&candidates, &unhosted, &HashSet::new(), true);
        let (rank, definer) = placed(&without_semantic, "/p/defines.rs");
        assert!(
            definer.admitted && rank <= 1,
            "{:?}",
            order(&without_semantic)
        );
        let qualified = [PathBuf::from("/p/defines.rs")].into_iter().collect();
        let relevant = query_primary_order(&candidates, &unhosted, &qualified, false);
        let (rank, _) = placed(&relevant, "/p/defines.rs");
        assert!(
            (ADMISSION_BASE_POSITION..=ADMISSION_BASE_POSITION + 1).contains(&rank),
            "{:?}",
            order(&relevant)
        );
        let neither = query_primary_order(&candidates, &unhosted, &HashSet::new(), false);
        assert_eq!(order(&neither), query_paths(10), "left out of the list");
    }

    /// A definer the query ranked past the leading results, with no host and
    /// no relevance of its own, keeps its base place and gets no bonus.
    #[test]
    fn an_unhosted_unqualified_definer_keeps_its_base_place() {
        let paths = query_paths(40);
        let refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let placements = query_primary_order(
            &base(&refs),
            &[alternative("load", &["/p/q30.rs"], &["/p/q30.rs"])],
            &HashSet::new(),
            false,
        );
        assert_eq!(order(&placements), paths);
        assert!(!placed(&placements, "/p/q30.rs").1.admitted);
    }

    /// Exact-tier entries keep their place ahead of everything; a host in
    /// the exact tier puts its definer first after that tier.
    #[test]
    fn the_exact_tier_keeps_its_place() {
        let (_, mut candidates) = ten();
        candidates[0].exact = true;
        candidates[1].exact = true;
        let placements = query_primary_order(
            &candidates,
            &[alternative(
                "load",
                &["/p/q00.rs", "/p/q06.rs", "/p/q02.rs"],
                &["/p/q06.rs"],
            )],
            &HashSet::new(),
            false,
        );
        assert_eq!(
            &order(&placements)[..3],
            ["/p/q00.rs", "/p/q01.rs", "/p/q06.rs"]
        );
    }

    #[test]
    fn a_single_group_alternation_expands_into_one_alternative_per_branch() {
        assert_eq!(
            expand_group(r"^pub struct (FormatContext|PinOwner)\b"),
            Some(vec![
                r"^pub struct FormatContext\b".to_string(),
                r"^pub struct PinOwner\b".to_string()
            ])
        );
        assert_eq!(
            expand_group("fn (?:load|save)_all"),
            Some(vec!["fn load_all".to_string(), "fn save_all".to_string()])
        );
        // Not the same regex, or not a plain group: left whole.
        assert_eq!(expand_group("(a|b)+c"), None);
        assert_eq!(expand_group("(a|(b|c))"), None);
        assert_eq!(expand_group("(a|b)(c|d)"), None);
        assert_eq!(expand_group(r"(a|b)\1"), None);
        assert_eq!(expand_group("(?=a|b)c"), None);
        assert_eq!(expand_group("(ab)c"), None);
        assert_eq!(expand_group(r"[(]a|b"), None);
    }

    /// A declaration-shaped pattern names its declarations: each branch of
    /// `^pub struct (A|B)` is judged on its own and records the declared name.
    #[test]
    fn declaration_shaped_alternatives_record_their_definitions() {
        let pattern = r"^pub struct (Alpha|Beta)\b";
        let line = |path: &str, name: &str| {
            matched_file(
                path,
                vec![grep_match(
                    path,
                    3,
                    &format!("pub struct {name} {{"),
                    &format!("pub struct {name}"),
                )],
                0,
            )
        };
        let list = PatternList::from_collection(
            collection(
                vec![line("/p/a.rs", "Alpha"), line("/p/b.rs", "Beta")],
                false,
            ),
            Path::new("/p"),
            pattern,
        );
        assert_eq!(list.alternatives.len(), 2, "{:?}", list.alternatives);
        for (alternative, (path, name)) in list
            .alternatives
            .iter()
            .zip([("/p/a.rs", "Alpha"), ("/p/b.rs", "Beta")])
        {
            assert_eq!(alternative.definitions.len(), 1, "{alternative:?}");
            assert_eq!(alternative.definitions[0].path, PathBuf::from(path));
            assert_eq!(alternative.definitions[0].name, name);
            assert!(alternative.is_selective());
        }
    }

    /// The magic-context report: `autoSearch|auto_search|runAutoSearch`. The
    /// exported `runAutoSearchHintForPi` declares a name `runAutoSearch`
    /// matches although the match is only its prefix; the indented `const
    /// autoSearch` in a component body is a local variable, not a definition.
    fn auto_search_files() -> Vec<GrepFileMatches> {
        vec![
            matched_file(
                "/p/dashboard/ConfigEditor.tsx",
                vec![grep_match(
                    "/p/dashboard/ConfigEditor.tsx",
                    1362,
                    "                const autoSearch = () =>",
                    "autoSearch",
                )],
                0,
            ),
            matched_file(
                "/p/pi/auto-search-pi.ts",
                vec![grep_match(
                    "/p/pi/auto-search-pi.ts",
                    256,
                    "export async function runAutoSearchHintForPi(",
                    "runAutoSearch",
                )],
                0,
            ),
            matched_file(
                "/p/pi/context-handler.ts",
                vec![grep_match(
                    "/p/pi/context-handler.ts",
                    3954,
                    "    await runAutoSearchHintForPi(state);",
                    "runAutoSearch",
                )],
                0,
            ),
        ]
    }

    #[test]
    fn a_declared_name_longer_than_the_match_defines_the_alternative() {
        let list = PatternList::from_collection(
            collection(auto_search_files(), false),
            Path::new("/p"),
            "autoSearch|auto_search|runAutoSearch",
        );
        let [local, _, prefix] = &list.alternatives[..] else {
            panic!("three alternatives: {:?}", list.alternatives);
        };
        assert_eq!(local.matched_files, 1);
        assert!(
            local.definitions.is_empty(),
            "a local binding is not a definition: {local:?}"
        );
        assert!(!local.is_selective());
        assert_eq!(
            prefix.definitions,
            [AlternativeDefinition {
                path: PathBuf::from("/p/pi/auto-search-pi.ts"),
                name: "runAutoSearchHintForPi".to_string(),
                line: 256,
            }]
        );
        assert!(prefix.is_selective());
    }

    #[test]
    fn only_indented_unexported_value_bindings_are_local() {
        for line in [
            "    let total = 0;",
            "\tconst autoSearch = () =>",
            "  var x = 1",
        ] {
            assert!(is_local_binding(line), "{line:?}");
        }
        for line in [
            "const MAX: usize = 4;",
            "export const autoSearch = 1;",
            "    pub const MAX: usize = 4;",
            "    export const inner = 1;",
            "    const fn limit() -> usize {",
            "    fn helper() {",
            "    constant = 1",
        ] {
            assert!(!is_local_binding(line), "{line:?}");
        }
    }

    #[test]
    fn query_relevance_is_relative_to_the_best_of_each_signal() {
        assert!(has_query_relevance(Some(0.5), Some(0.5), None, None));
        assert!(!has_query_relevance(Some(0.3), Some(0.5), None, None));
        assert!(has_query_relevance(None, None, Some(6.0), Some(10.0)));
        assert!(!has_query_relevance(None, None, Some(4.0), Some(10.0)));
        // Missing signals are not relevance.
        assert!(!has_query_relevance(None, Some(0.5), None, Some(10.0)));
    }

    #[test]
    fn summary_line_counts_files_overlap_and_definition_sites() {
        let list = PatternList::from_collection(
            collection(
                vec![
                    definition("/p/src/a.rs"),
                    definition("/p/src/b.rs"),
                    definition("/p/src/c.rs"),
                    definition("/p/src/d.rs"),
                    mention("/p/src/e.rs"),
                ],
                false,
            ),
            Path::new("/p"),
            "load",
        );
        let prose = [PathBuf::from("/p/src/e.rs"), PathBuf::from("/p/src/a.rs")]
            .into_iter()
            .collect();
        assert_eq!(
            list.summary_line(&prose, Path::new("/p")),
            "[pattern `load`: 5 files matched, 2 also found by the query; defined in src/a.rs:7, src/b.rs:7, src/c.rs:7]"
        );
    }

    #[test]
    fn summary_line_names_module_level_declarations_before_local_bindings() {
        let list = PatternList::from_collection(
            collection(auto_search_files(), false),
            Path::new("/p"),
            "autoSearch|auto_search|runAutoSearch",
        );
        assert_eq!(
            list.summary_line(&HashSet::new(), Path::new("/p")),
            "[pattern `autoSearch|auto_search|runAutoSearch`: 3 files matched, 0 also found by the query; defined in pi/auto-search-pi.ts:256]"
        );
        // With no module-level declaration the local binding is still named.
        let local_only = PatternList::from_collection(
            collection(vec![auto_search_files().remove(0)], false),
            Path::new("/p"),
            "autoSearch",
        );
        assert_eq!(
            local_only.summary_line(&HashSet::new(), Path::new("/p")),
            "[pattern `autoSearch`: 1 file matched, 0 also found by the query; defined in dashboard/ConfigEditor.tsx:1362]"
        );
        // A broad alternative names only whole-name declarations: `Error` in
        // `ParseError` is not listed, `enum Error` is, after the selective
        // alternative's declaration.
        let mut files = vec![
            declares("/p/src/backup.rs", "tracked_files"),
            matched_file(
                "/p/src/parse.rs",
                vec![grep_match(
                    "/p/src/parse.rs",
                    4,
                    "pub struct ParseError {",
                    "Error",
                )],
                0,
            ),
            declares("/p/src/error.rs", "Error"),
        ];
        for index in 0..60 {
            files.push(uses(&format!("/p/src/e{index:02}.rs"), "Error"));
        }
        let broad = PatternList::from_collection(
            collection(files, false),
            Path::new("/p"),
            "tracked_files|Error",
        );
        assert_eq!(
            broad.definition_sites(),
            [
                (PathBuf::from("/p/src/backup.rs"), 4),
                (PathBuf::from("/p/src/error.rs"), 4)
            ]
        );
    }

    #[test]
    fn summary_line_reports_no_definition_and_capped_examination() {
        let list = PatternList::from_collection(
            collection(vec![mention("/p/src/e.rs")], true),
            Path::new("/p"),
            "old_name",
        );
        assert_eq!(
            list.summary_line(&HashSet::new(), Path::new("/p")),
            "[pattern `old_name`: 1 file matched, 0 also found by the query; no definition found; examined 1 of 11 candidate files]"
        );
    }

    #[test]
    fn capped_zero_match_is_reported_distinctly_from_a_complete_zero_match() {
        let complete =
            PatternList::from_collection(collection(Vec::new(), false), Path::new("/p"), "nope");
        let capped =
            PatternList::from_collection(collection(Vec::new(), true), Path::new("/p"), "nope");
        assert_eq!(
            complete.summary_line(&HashSet::new(), Path::new("/p")),
            "[pattern `nope`: no match]"
        );
        assert_eq!(
            capped.summary_line(&HashSet::new(), Path::new("/p")),
            "[pattern `nope`: no match in the 0 of 10 candidate files examined; files not examined were not searched]"
        );
    }

    #[test]
    fn matched_by_comes_from_explicit_membership() {
        let pattern: HashSet<PathBuf> = [PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs")]
            .into_iter()
            .collect();
        let prose: HashSet<PathBuf> = [PathBuf::from("/p/b.rs"), PathBuf::from("/p/c.rs")]
            .into_iter()
            .collect();
        assert_eq!(
            matched_by(Path::new("/p/a.rs"), &pattern, &prose),
            "pattern"
        );
        assert_eq!(matched_by(Path::new("/p/b.rs"), &pattern, &prose), "both");
        assert_eq!(matched_by(Path::new("/p/c.rs"), &pattern, &prose), "query");
    }

    #[test]
    fn the_split_marker_is_part_of_the_list_identity_and_absent_otherwise() {
        let split = key();
        let mut query_only = split.clone();
        query_only.split = None;
        let serialized = serde_json::to_value(&query_only).expect("serialize");
        assert!(
            serialized.get("split").is_none(),
            "a query-only key keeps its existing form: {serialized}"
        );
        assert_ne!(split, query_only);
        let mut other_pattern = split.clone();
        other_pattern.split = Some(SplitListIdentity {
            pattern: "Load".to_string(),
            case_insensitive: false,
        });
        assert_ne!(split, other_pattern);
        let serialized = serde_json::to_value(&split).expect("serialize");
        assert_eq!(serialized["split"]["pattern"], "load");
    }

    #[test]
    fn bounded_scan_groups_lines_by_file_and_keeps_the_bound() {
        let result = GrepResult {
            matches: vec![
                grep_match("/p/a.rs", 1, "load()", "load"),
                grep_match("/p/b.rs", 2, "pub fn load() {", "load"),
                grep_match("/p/a.rs", 5, "load()", "load"),
            ],
            total_matches: 3,
            files_searched: 4,
            files_with_matches: 2,
            index_status: IndexStatus::Fallback,
            truncated: true,
            fully_degraded: false,
            engine_capped: true,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: 0,
            scan_deadline_reached: false,
            files_read_directly: 4,
            walk_bound: None,
        };
        let list = PatternList::from_bounded_scan(result, Path::new("/p"), "load");
        assert_eq!(list.len(), 2);
        assert!(list.examination.capped);
        assert!(list.files[0].definition);
        assert_eq!(list.files[1].file.lines.len(), 2);
    }
}
