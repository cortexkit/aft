//! Cross-encoder reranking of the head of the canonical result list.
//!
//! A reranker scores the prose `query` against a short text built for each of
//! the first entries of the canonical list and reorders only those entries.
//! Only queries the live router classifies as prose are reranked (see
//! [`is_prose_query`]); identifier, path, literal and regex queries keep the
//! fused order exactly as with reranking off.
//! Backends implement [`RerankBackend`]; everything that decides *what* is
//! reranked (how many entries, which tiers, the candidate text, the defaults
//! and clamps of the `search.rerank` config block) lives in this module so it
//! stays inside the search-quality ranking fence.
//!
//! Paging contract. The canonical list is rebuilt on every request, and a
//! backend may answer one request and not the next (model still downloading,
//! worker busy, a timeout). If page one were served in fused order and page two
//! in reranked order, the stream would repeat some results and lose others. So
//! the first outcome for a list identity (a permutation, or the decision to
//! skip) is committed to a process-wide memo and every later page of the same
//! list replays it. The identity covers everything that defines the reranked
//! prefix and nothing that selects a page: `topK` and `offset` never enter it.

pub(crate) mod fixture;
pub(crate) mod onnx;
pub(crate) mod pool_export;
pub(crate) mod remote;
pub(crate) mod slot;
pub(crate) mod synapse;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::blocks::{BlockReply, CanonicalList, BLOCK_DEPTHS};
use super::comparator::CandidateResult;
use super::evidence_descriptor::EvidenceTier;
use crate::config::{RerankBackendKind, SearchConfig};

/// How many entries at the head of the canonical list are reranked when the
/// config does not say. Chosen so a cross-encoder answers within the default
/// deadline on a laptop CPU; entries below it keep their fused order.
pub(crate) const DEFAULT_TOP_N: usize = 20;
/// The reranked prefix never reaches past the first block of the canonical
/// list. That block holds every candidate found within the shallowest
/// retrieval depth, which every request observes, so its contents are the same
/// whatever `topK` and `offset` a request asks for.
pub(crate) const MAX_TOP_N: usize = BLOCK_DEPTHS[0];
/// Per-call scoring budget when the config does not set one.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 1_500;
pub(crate) const MIN_TIMEOUT_MS: u64 = 50;
pub(crate) const MAX_TIMEOUT_MS: u64 = 15_000;
/// Byte budget for one candidate text (path, symbol line and snippet lines).
/// About 300 tokens for code, which leaves room for the query inside a
/// 512-token cross-encoder window.
pub(crate) const CANDIDATE_TEXT_BUDGET_BYTES: usize = 1_024;
/// Names the candidate-text construction below. Bump it whenever that
/// construction changes, so memoized orders and recorded fixture scores made
/// with the old text are never reused.
pub(crate) const TEXT_POLICY_REVISION: &str = "path:line+name+lines/v2";
/// Lines of context kept above the best matching line of a whole-file result.
const FILE_CONTEXT_LINES_ABOVE: usize = 2;
/// Upper bound on committed outcomes kept in memory. Entries are tiny (at most
/// `MAX_TOP_N` indices or one short reason); the oldest go first.
const MEMO_CAPACITY: usize = 4_096;
/// Longest skip reason shown to the agent.
const MAX_REASON_CHARS: usize = 80;

/// Process environment variable naming a recorded score pack. When set, the
/// fixture backend replaces whatever the config selects, and a score missing
/// from the pack fails the search instead of silently keeping fused order.
pub(crate) const FIXTURE_PACK_ENV: &str = "AFT_RERANK_FIXTURE_PACK";
/// Process environment variable naming a pack file that the ONNX backend's
/// scores are appended to while serving normally.
pub(crate) const RECORD_PACK_ENV: &str = "AFT_RERANK_RECORD_PACK";

/// One candidate document handed to a backend: the bounded candidate text.
pub(crate) struct RerankDoc<'a> {
    pub(crate) text: &'a str,
}

/// Names the exact scoring function a backend applies. Two backends with equal
/// fingerprints must produce equal scores for equal inputs, so the fingerprint
/// is part of every memoized ordering and every recorded fixture score.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RerankFingerprint {
    pub(crate) backend: &'static str,
    pub(crate) model: String,
    pub(crate) revision: String,
}

/// Why a backend returned no scores. None of these is a search error: the
/// caller keeps the fused order and reports one short note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RerankError {
    /// The backend cannot serve now (model not provisioned yet, runtime
    /// missing, worker busy, process shutting down).
    Unavailable(String),
    /// No answer arrived before the caller's deadline.
    Timeout,
    /// The backend rejected the request (bad input, over a size limit).
    Refused(String),
    /// Scoring started and failed.
    Failed(String),
}

impl RerankError {
    fn skip_reason(&self) -> String {
        let reason = match self {
            Self::Unavailable(reason) => reason.clone(),
            Self::Timeout => "timeout".to_string(),
            Self::Refused(reason) => format!("refused: {reason}"),
            Self::Failed(reason) => format!("failed: {reason}"),
        };
        short_reason(&reason)
    }
}

pub(crate) trait RerankBackend: Send + Sync {
    /// The identity of the scoring function. It must be constant for the
    /// lifetime of the instance and must do no I/O: it is read on the search
    /// path and is part of the key under which a list's rerank outcome is
    /// committed, so a value that changed mid-paging would rerank a list whose
    /// earlier page was already served in another order. A backend that needs
    /// I/O to learn its identity (asking a server which model it serves) must
    /// learn it while it is being built, off the search path; until it is
    /// built no backend is installed, which counts as reranking off.
    fn fingerprint(&self) -> RerankFingerprint;
    fn max_batch(&self) -> usize;
    /// One score per doc, same order; must return by `deadline` or Err(Timeout).
    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError>;
}

/// A backend that scores every document zero, so a stable reorder keeps the
/// prior order. Tests use it where a backend must exist but must not move
/// anything.
#[cfg(test)]
pub(crate) struct NoopRerankBackend;

#[cfg(test)]
impl RerankBackend for NoopRerankBackend {
    fn fingerprint(&self) -> RerankFingerprint {
        RerankFingerprint {
            backend: "noop",
            model: "noop".to_string(),
            revision: "0".to_string(),
        }
    }

    fn max_batch(&self) -> usize {
        usize::MAX
    }

    fn score(
        &self,
        _query: &str,
        docs: &[RerankDoc<'_>],
        _deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        Ok(vec![0.0; docs.len()])
    }
}

/// The `search.rerank` block after defaults and clamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RerankSettings {
    pub(crate) backend: RerankBackendKind,
    pub(crate) model: Option<String>,
    pub(crate) top_n: usize,
    pub(crate) timeout: Duration,
}

impl RerankSettings {
    pub(crate) fn resolve(search: &SearchConfig) -> Self {
        let rerank = search.rerank.clone().unwrap_or_default();
        let top_n = rerank
            .top_n
            .map_or(DEFAULT_TOP_N, |value| {
                usize::try_from(value).unwrap_or(usize::MAX)
            })
            .clamp(1, MAX_TOP_N);
        let timeout_ms = rerank
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        Self {
            backend: rerank.backend.unwrap_or(RerankBackendKind::Off),
            model: rerank.model,
            top_n,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

/// A backend chosen for one request. `fail_closed` backends turn every
/// scoring failure into a search error. Only the recorded-score fixture is one:
/// it exists so the search-quality benchmark measures ranking with the
/// reranker, and a silent fallback to fused order would make that benchmark
/// measure the engine without it.
#[derive(Clone)]
pub(crate) struct SelectedBackend {
    pub(crate) backend: Arc<dyn RerankBackend>,
    pub(crate) fail_closed: bool,
}

/// Inputs of one rerank decision, all independent of the requested page.
pub(crate) struct RerankRequest<'a> {
    pub(crate) search: &'a SearchConfig,
    /// The backend installed for this context when the search started.
    pub(crate) backend: slot::Installed,
    pub(crate) project_root: &'a Path,
    /// The prose question. `None` (a pattern-only search) means no rerank.
    pub(crate) prose: Option<&'a str>,
    /// The query as the caller sent it, before any route rewrote it. Only a
    /// query this classifies as prose is reranked (see [`is_prose_query`]).
    pub(crate) public_query: &'a str,
    /// Files the caller asked to prefer. Scope order is kept: a reranked entry
    /// never crosses from outside the scope to inside it or back.
    pub(crate) path_scope: Option<&'a HashSet<PathBuf>>,
}

/// What happened to the head of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadOutcome {
    /// Reranking is off, the request has no prose, the query is not prose
    /// (see [`is_prose_query`]), or there are fewer than two entries to order.
    NotApplicable,
    /// The committed permutation was applied (it may be the identity).
    Reordered,
    /// Fused order kept; the reason is shown to the agent.
    Skipped(String),
}

/// Whether a query has prose for the reranker to judge. Reranking is skipped
/// entirely when the whole query is an identifier or a literal with no prose.
///
/// A cross-encoder judges how well a passage answers a question, and a lone
/// identifier, path, error code, quoted literal or regex is not one; on the
/// search-quality benchmark (benchmarks/aft-search) every reranker tried
/// lifted natural-language and mixed questions while roughly halving the MRR
/// of identifier-shaped ones. So a
/// query is skipped when the router reads it as an identifier, a path, a
/// regex or a whole quoted literal, or when the query-shape classifier finds
/// no prose words in it (identifier, error code, path or regex). Anything
/// with prose words is reranked, including prose the router reads as a code
/// literal or log excerpt (a question with a parenthesised list, or with the
/// word "error"). Both classifiers read the query the caller sent, not the
/// lane plan or the text a route ranks, because the code-literal route strips
/// its quotes and zero-result escalation hands a regex or literal query a
/// natural-language plan.
pub(crate) fn is_prose_query(query: &str) -> bool {
    use super::extensions::RawQuery;
    use super::plan_table::SearchShape;
    use crate::query_shape::QueryKind;

    let (shape, _) = crate::search_b2::install_defaults().classify(&RawQuery::new(query));
    if matches!(
        shape,
        SearchShape::Identifier | SearchShape::Path | SearchShape::Regex
    ) || is_whole_quoted(query)
    {
        return false;
    }
    matches!(
        crate::query_shape::classify(query).kind,
        QueryKind::NaturalLanguage | QueryKind::Mixed
    )
}

/// A query that is one quoted literal from end to end, such as `"not found"`.
fn is_whole_quoted(query: &str) -> bool {
    let trimmed = query.trim();
    let mut characters = trimmed.chars();
    match (characters.next(), characters.next_back()) {
        (Some(open), Some(close)) => open == close && matches!(open, '"' | '\'' | '`'),
        _ => false,
    }
}

/// The prose a request may be reranked against: present, not blank, and sent
/// by the caller as a query [`is_prose_query`] accepts. `None` means the
/// request is not reranked at all, exactly as with reranking off: no backend
/// call, no committed outcome and no note.
fn rerank_prose<'a>(request: &RerankRequest<'a>) -> Option<&'a str> {
    request
        .prose
        .map(str::trim)
        .filter(|prose| !prose.is_empty())
        .filter(|_| is_prose_query(request.public_query))
}

/// Rerank the head of the canonical list in `reply`, then re-cut the page
/// from the canonical list so it shows the committed order. Returns the note
/// to show when reranking was skipped. An error is returned only for a
/// fail-closed backend, or when the benchmark fixture pack is configured but
/// not installed.
pub(crate) fn rerank_canonical_head(
    reply: &mut BlockReply,
    offset: usize,
    top_k: usize,
    request: RerankRequest<'_>,
) -> Result<Option<String>, String> {
    let export = pool_export::start(
        &reply.canonical_list,
        &request,
        RerankSettings::resolve(request.search).top_n,
    );
    let outcome = rerank_head(reply, offset, top_k, &request);
    if let Some(export) = export {
        export.finish(&reply.canonical_list, request.project_root, &outcome);
    }
    outcome
}

fn rerank_head(
    reply: &mut BlockReply,
    offset: usize,
    top_k: usize,
    request: &RerankRequest<'_>,
) -> Result<Option<String>, String> {
    if rerank_prose(request).is_none() {
        return Ok(None);
    }
    let selected = match &request.backend {
        slot::Installed::Off => return Ok(None),
        slot::Installed::Ready(selected) => Ok(selected.clone()),
        slot::Installed::NotReady(reason) => {
            if crate::environment::non_empty_os_var(FIXTURE_PACK_ENV).is_some() {
                return Err(format!("reranker fixture not installed: {reason}"));
            }
            Err(reason.clone())
        }
    };
    let settings = RerankSettings::resolve(request.search);
    match rerank_block_zero(&mut reply.canonical_list, &selected, &settings, request)? {
        HeadOutcome::NotApplicable => Ok(None),
        HeadOutcome::Reordered => {
            reply.page = reply
                .canonical_list
                .entries()
                .skip(offset)
                .take(top_k)
                .cloned()
                .collect();
            Ok(None)
        }
        HeadOutcome::Skipped(reason) => Ok(Some(format!("rerank skipped: {reason}"))),
    }
}

/// Apply the committed outcome for this list identity to the first block,
/// computing and committing it first when this is the first request.
pub(crate) fn rerank_block_zero(
    list: &mut CanonicalList,
    selected: &Result<SelectedBackend, String>,
    settings: &RerankSettings,
    request: &RerankRequest<'_>,
) -> Result<HeadOutcome, String> {
    let Some(prose) = rerank_prose(request) else {
        return Ok(HeadOutcome::NotApplicable);
    };
    let selected = match selected {
        Ok(selected) => selected,
        // No backend is installed yet (it is being built, or its last build
        // failed). The skip is not committed: once a backend is installed the
        // list is reranked.
        Err(reason) => return Ok(HeadOutcome::Skipped(short_reason(reason))),
    };
    let Some(block) = list.blocks.iter_mut().find(|block| block.tier_index == 0) else {
        return Ok(HeadOutcome::NotApplicable);
    };
    let fingerprint = selected.backend.fingerprint();
    // Only non-exact entries are reranked. Exact-tier entries keep the
    // engine's own order (definitions first) and their positions, so the
    // reranker can neither reorder them nor move anything across them.
    let positions = rerank_positions(block, settings.top_n.min(selected.backend.max_batch()));
    let depth = positions.len();
    if depth < 2 {
        return Ok(HeadOutcome::NotApplicable);
    }

    let members = positions
        .iter()
        .map(|position| &block.entries[*position])
        .collect::<Vec<_>>();
    let segments = members
        .iter()
        .map(|entry| in_scope(&entry.result, request.path_scope))
        .collect::<Vec<_>>();
    let identity = memo_identity(
        &list.key,
        prose,
        request.path_scope,
        &fingerprint,
        depth,
        members.iter().map(|entry| &entry.result),
    );

    let outcome = match memo().lookup(&identity) {
        Some(committed) => committed,
        None => {
            let texts = members
                .iter()
                .map(|entry| candidate_text(request.project_root, &entry.result, prose))
                .collect::<Vec<_>>();
            let docs = texts
                .iter()
                .map(|text| RerankDoc { text })
                .collect::<Vec<_>>();
            let deadline = Instant::now() + settings.timeout;
            let computed = match selected.backend.score(prose, &docs, deadline) {
                Ok(scores) => match validate_scores(&scores, depth) {
                    Ok(()) => CommittedOutcome::Reordered(permutation(&scores, &segments).into()),
                    Err(error) => {
                        if selected.fail_closed {
                            return Err(format!("reranker fixture: {}", error.skip_reason()));
                        }
                        CommittedOutcome::Skipped(error.skip_reason())
                    }
                },
                Err(error) => {
                    if selected.fail_closed {
                        return Err(format!("reranker fixture: {}", error.skip_reason()));
                    }
                    CommittedOutcome::Skipped(error.skip_reason())
                }
            };
            memo().commit(identity, computed)
        }
    };

    match outcome {
        CommittedOutcome::Skipped(reason) => Ok(HeadOutcome::Skipped(reason)),
        CommittedOutcome::Reordered(order) => {
            apply_permutation(&mut block.entries, &positions, &order);
            Ok(HeadOutcome::Reordered)
        }
    }
}

/// The backend this search may use, read from the context's slot without I/O
/// and without waiting. Tests may install one for their own thread instead.
pub(crate) fn installed_backend(ctx: &crate::context::AppContext) -> slot::Installed {
    if let Some(reason) = ctx.cache_unavailable_reason() {
        return slot::Installed::NotReady(format!("cache_dir_unavailable: {reason}"));
    }
    #[cfg(test)]
    if let Some(selected) = tests::test_backend_override() {
        return slot::Installed::Ready(selected);
    }
    let config = ctx.config();
    ctx.rerank_slot().read(slot::BuildInputs::from_config(
        &config,
        !ctx.daemonless_query_mode(),
    ))
}

fn validate_scores(scores: &[f32], expected: usize) -> Result<(), RerankError> {
    if scores.len() != expected {
        return Err(RerankError::Failed(format!(
            "{} scores for {expected} candidates",
            scores.len()
        )));
    }
    if scores.iter().any(|score| !score.is_finite()) {
        return Err(RerankError::Failed("non-finite score".to_string()));
    }
    Ok(())
}

/// Positions in `block` of the entries a reranker may reorder: the first
/// `limit` non-exact entries. The first block holds every candidate found
/// within the shallowest retrieval depth, whatever page is requested, so this
/// set is the same for every page of a list.
pub(crate) fn rerank_positions(block: &super::blocks::FrozenBlock, limit: usize) -> Vec<usize> {
    block
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.result.evidence.tier != EvidenceTier::Exact)
        .map(|(position, _)| position)
        .take(limit)
        .collect()
}

/// Entries may only move within a run of equal path-scope membership, so
/// path-scope preference stays in force.
fn in_scope(result: &CandidateResult, path_scope: Option<&HashSet<PathBuf>>) -> bool {
    path_scope.is_some_and(|scope| scope.contains(&result.path))
}

/// `order[i]` is the prior position of the entry placed at position `i`.
/// Within each maximal run of equal segments, higher scores come first; equal
/// scores (under `total_cmp`) keep their prior order.
pub(crate) fn permutation<S: PartialEq>(scores: &[f32], segments: &[S]) -> Vec<usize> {
    let mut order = Vec::with_capacity(scores.len());
    let mut start = 0;
    while start < scores.len() {
        let mut end = start + 1;
        while end < scores.len() && segments[end] == segments[start] {
            end += 1;
        }
        let mut run = (start..end).collect::<Vec<_>>();
        run.sort_by(|left, right| {
            scores[*right]
                .total_cmp(&scores[*left])
                .then_with(|| left.cmp(right))
        });
        order.extend(run);
        start = end;
    }
    order
}

/// Reorder the entries at `positions` by `order` (`order[k]` is the index,
/// within `positions`, of the entry placed at `positions[k]`). Every other
/// entry, and every `r3_order_index`, stays where it is: only the reranked
/// entries move, and only among the positions they already held.
fn apply_permutation(
    entries: &mut [super::blocks::BlockEntry],
    positions: &[usize],
    order: &[usize],
) {
    let original = positions
        .iter()
        .map(|position| entries[*position].clone())
        .collect::<Vec<_>>();
    for (slot, prior) in order.iter().enumerate() {
        let position = positions[slot];
        let index = entries[position].r3_order_index;
        let mut entry = original[*prior].clone();
        entry.r3_order_index = index;
        entries[position] = entry;
    }
}

fn short_reason(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(MAX_REASON_CHARS) {
        None => line.to_string(),
        Some((cut, _)) => format!("{}…", &line[..cut]),
    }
}

/// The text a backend scores for one entry, in three parts:
///
/// 1. `path:line`: the project-relative path and the 1-based line the entry
///    points at (a symbol's first line; for a whole-file entry, the line that
///    best matches the prose, the same match the rendered snippet uses, or 1).
/// 2. A name line: the symbol's declared name for a symbol entry; for a
///    whole-file entry, the first code identifier of the prose that occurs in
///    the file, or else the file stem.
/// 3. The body: a symbol entry's own lines; a whole-file entry's lines from
///    just above its best match (or the file head).
///
/// In an evaluation of gte-reranker-modernbert on aft_search result pools, a
/// reranker given only the path and the body ranked the declaring file lower
/// on 4 of 5 identifier queries; the name and line give it what the agent
/// sees in the result list. Blank lines are
/// skipped and the body is cut so the whole text fits
/// `CANDIDATE_TEXT_BUDGET_BYTES` at a character boundary; the first two lines
/// are always kept whole. The result depends only on the project root, the
/// entry, the prose and the file, never on which page is being served.
pub(crate) fn candidate_text(project_root: &Path, result: &CandidateResult, prose: &str) -> String {
    let bytes = std::fs::read(&result.path).unwrap_or_default();
    let (raw_line, selected) = if let Some(range) = result.symbol_range {
        let start = floor_raw_boundary(&bytes, range.start.min(bytes.len()));
        let end = floor_raw_boundary(&bytes, range.end.min(bytes.len())).max(start);
        (
            bytes[..start].iter().filter(|&&byte| byte == b'\n').count(),
            &bytes[start..end],
        )
    } else {
        (0, bytes.as_slice())
    };
    // Stored ranges address raw bytes; decoding and newline normalization
    // must happen after slicing, including when earlier bytes are invalid UTF-8.
    let source = String::from_utf8_lossy(selected).replace("\r\n", "\n");
    let (line, name, body): (usize, String, Vec<&str>) = match result.symbol_range {
        Some(_) => {
            let lines = source.lines().collect::<Vec<_>>();
            let name = declared_name(&lines).unwrap_or_else(|| file_stem(&result.path));
            (raw_line, name, lines)
        }
        None => {
            let matched = super::matching_line_from_source(&result.path, prose, None)
                .map(|(line, _)| line as usize);
            let first = matched.map_or(0, |line| line.saturating_sub(FILE_CONTEXT_LINES_ABOVE));
            let name =
                matched_identifier(prose, &source).unwrap_or_else(|| file_stem(&result.path));
            (
                matched.unwrap_or(0),
                name,
                source.lines().skip(first).collect(),
            )
        }
    };
    let mut text = format!(
        "{}:{}\n{name}",
        display_path(project_root, &result.path),
        line + 1
    );
    for line in body {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if text.len() + 1 >= CANDIDATE_TEXT_BUDGET_BYTES {
            break;
        }
        text.push('\n');
        let room = CANDIDATE_TEXT_BUDGET_BYTES - text.len();
        text.push_str(&line[..floor_char_boundary(line, line.len().min(room))]);
    }
    text
}

fn floor_raw_boundary(bytes: &[u8], mut at: usize) -> usize {
    while at > 0 && at < bytes.len() && bytes[at] & 0xc0 == 0x80 {
        at -= 1;
    }
    at
}

fn display_path(project_root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(project_root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let root = std::fs::canonicalize(project_root).ok()?;
            let path = std::fs::canonicalize(path).ok()?;
            path.strip_prefix(root).ok().map(Path::to_path_buf)
        })
        .unwrap_or_else(|| path.to_path_buf());
    relative.to_string_lossy().replace('\\', "/")
}

/// Longest name kept on a candidate's name line.
const MAX_NAME_BYTES: usize = 120;

fn bounded_name(name: &str) -> String {
    name[..floor_char_boundary(name, name.len().min(MAX_NAME_BYTES))].to_string()
}

fn file_stem(path: &Path) -> String {
    bounded_name(
        &path
            .file_stem()
            .map(|stem| stem.to_string_lossy())
            .unwrap_or_default(),
    )
}

/// The name a symbol's source declares: the identifier after the first
/// declaration keyword on its first line of code (attributes, decorators and
/// comments are skipped), or else the first identifier on that line that is
/// not a modifier or keyword.
fn declared_name(lines: &[&str]) -> Option<String> {
    static DECLARATION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\b(?:fn|struct|enum|trait|union|type|mod|const|static|class|interface|function|def|func|let|var|macro_rules!)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        )
        .expect("declaration pattern")
    });
    static IDENTIFIER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"[A-Za-z_$][A-Za-z0-9_$]*").expect("identifier pattern")
    });
    const NOT_A_NAME: &[&str] = &[
        "pub",
        "crate",
        "async",
        "unsafe",
        "extern",
        "export",
        "default",
        "abstract",
        "public",
        "private",
        "protected",
        "static",
        "final",
        "impl",
        "for",
        "where",
        "declare",
        "readonly",
    ];
    let code = lines.iter().map(|line| line.trim()).find(|line| {
        !line.is_empty()
            && !["#[", "#!", "@", "//", "/*", "*", "#", "--", "\"\"\""]
                .iter()
                .any(|prefix| line.starts_with(prefix))
    })?;
    if let Some(captures) = DECLARATION.captures(code) {
        return Some(bounded_name(&captures[1]));
    }
    IDENTIFIER
        .find_iter(code)
        .map(|found| found.as_str())
        .find(|word| !NOT_A_NAME.contains(word))
        .map(bounded_name)
}

/// The first code identifier of the prose (a word with an underscore, `::`,
/// `$`, or an inner capital) that occurs in `source`.
fn matched_identifier(prose: &str, source: &str) -> Option<String> {
    prose
        .split(|character: char| {
            !(character.is_alphanumeric() || matches!(character, '_' | ':' | '$'))
        })
        .map(|word| word.trim_matches(':'))
        .filter(|word| {
            word.contains('_')
                || word.contains("::")
                || word.contains('$')
                || word.chars().skip(1).any(char::is_uppercase)
        })
        .find(|word| source.contains(word))
        .map(bounded_name)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Digest of everything that defines the reranked prefix and how it is
/// scored: the canonical list key (root, snapshot generation, normalized query,
/// test inclusion), the exact prose, the path scope, the backend fingerprint,
/// the depth, the candidate-text policy and budget, and the prefix entries
/// themselves. Page position is deliberately absent.
fn memo_identity<'a>(
    key: &super::blocks::CanonicalListKey,
    prose: &str,
    path_scope: Option<&HashSet<PathBuf>>,
    fingerprint: &RerankFingerprint,
    depth: usize,
    prefix: impl Iterator<Item = &'a CandidateResult>,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(key.project_root.to_string_lossy().as_bytes());
    field(key.snapshot_generation.as_bytes());
    field(key.normalized_query.as_bytes());
    field(&[u8::from(key.include_tests)]);
    field(prose.as_bytes());
    match path_scope {
        None => field(b"no-scope"),
        Some(scope) => {
            let mut paths = scope
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            paths.sort();
            field(&(paths.len() as u64).to_le_bytes());
            for path in paths {
                field(path.as_bytes());
            }
        }
    }
    field(fingerprint.backend.as_bytes());
    field(fingerprint.model.as_bytes());
    field(fingerprint.revision.as_bytes());
    field(&(depth as u64).to_le_bytes());
    field(TEXT_POLICY_REVISION.as_bytes());
    field(&(CANDIDATE_TEXT_BUDGET_BYTES as u64).to_le_bytes());
    for result in prefix {
        field(result.path.to_string_lossy().as_bytes());
        match result.symbol_range {
            Some(range) => {
                field(&(range.start as u64).to_le_bytes());
                field(&(range.end as u64).to_le_bytes());
            }
            None => field(b"file"),
        }
    }
    *hasher.finalize().as_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommittedOutcome {
    Reordered(Arc<[usize]>),
    Skipped(String),
}

#[derive(Default)]
struct Memo {
    entries: Mutex<(HashMap<[u8; 32], CommittedOutcome>, VecDeque<[u8; 32]>)>,
}

impl Memo {
    fn lookup(&self, identity: &[u8; 32]) -> Option<CommittedOutcome> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .get(identity)
            .cloned()
    }

    /// Commit `outcome` unless another request committed first; either way,
    /// return the outcome now in force. A result that arrives after a skip was
    /// committed therefore never replaces it.
    fn commit(&self, identity: [u8; 32], outcome: CommittedOutcome) -> CommittedOutcome {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (entries, order) = &mut *guard;
        if let Some(existing) = entries.get(&identity) {
            return existing.clone();
        }
        while entries.len() >= MEMO_CAPACITY {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(identity, outcome.clone());
        order.push_back(identity);
        outcome
    }
}

fn memo() -> &'static Memo {
    static MEMO: OnceLock<Memo> = OnceLock::new();
    MEMO.get_or_init(Memo::default)
}

/// Marks a function that reads or writes files or the network. Tests count
/// the marks on their own thread to prove the search path's backend
/// selection never reaches one; in normal builds it does nothing.
#[inline]
pub(crate) fn note_io() {
    #[cfg(test)]
    tests::count_io();
}
