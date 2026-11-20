//! Per-child search engines for a parent folder's `aft_search`.
//!
//! Each child gets a reader-only [`AppContext`] with its own search engine.
//! The worker installs the child's published text and semantic indexes into
//! that context. Queries search those indexes, or use the bounded disk scan
//! when no usable index is available. These query contexts never write a
//! store. The parent merges child results without applying a second set of
//! ranking rules. With the data layer on, each engine instead answers from
//! the checkout the session holds for its child in the data layer: its text
//! plane, and its meaning plane, which only reads the vectors the child's
//! repository already stored (see `data_layer::parent::search_children`).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::commands::semantic_search::{self, regex_route, split_query, ChildPatternPage};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::search_index::{GrepMatch, GrepResult, IndexStatus};

use crate::parent_search::engine::ChildEngine;
use crate::parent_search::query::{attach_gaps, Gaps};
use crate::parent_search::{display_relative, lock, read, Child, ParentSession};

// ---------------------------------------------------------------------------
// The parent's aft_search
// ---------------------------------------------------------------------------

/// Whether the parent's children answer from their checkouts in the data
/// layer: always, unless a test hook disables that path for this thread.
fn data_layer_answers() -> bool {
    #[cfg(test)]
    if crate::data_layer::text_scope::test_hooks::today_only() {
        return false;
    }
    true
}

/// The page size `aft_search` uses when the request names none.
const DEFAULT_TOP_K: usize = 10;

enum RowText {
    Ranked(regex_route::RankedFile),
    Grep(GrepMatch),
    Split(semantic_search::HybridResult),
}

struct Row {
    wire: Value,
    text: Option<RowText>,
}

impl Row {
    fn regex_rank(&self) -> Option<regex_route::RankKey> {
        match &self.text {
            Some(RowText::Ranked(file)) => Some(file.rank),
            _ => None,
        }
    }

    fn score(&self, field: &str) -> f64 {
        self.wire[field].as_f64().unwrap_or(f64::MIN)
    }
}

fn prefix_path(parent: &Path, child: &Child, path: &Path) -> String {
    if path.is_absolute() {
        display_relative(parent, path)
    } else {
        child
            .relative
            .join(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

/// Rewrites a child row's paths to be relative to the parent folder. The
/// `location` field is rewritten only when it begins with the row's file:
/// some rows carry a description there instead of a path.
fn prefix_row(row: &mut Value, parent: &Path, child: &Child) {
    let Some(file) = row["file"].as_str().map(str::to_owned) else {
        return;
    };
    let prefixed = prefix_path(parent, child, Path::new(&file));
    if let Some(Value::String(location)) = row.get_mut("location") {
        if let Some(rest) = location.strip_prefix(file.as_str()) {
            *location = format!("{prefixed}{rest}");
        }
    }
    if let Some(Value::String(supports)) = row.get_mut("supports") {
        *supports = prefix_path(parent, child, Path::new(supports));
    }
    row["file"] = Value::String(prefixed);
}

/// Merges the children's ranked lists without reordering any one of them:
/// at each step the child whose next row has the highest engine score
/// (combined score, or an individual search score when available) gives its
/// next row. Ties prefer the higher semantic similarity, which uses the same
/// model in every child, then the earlier child.
fn merge(mut lists: Vec<VecDeque<Row>>, limit: usize) -> Vec<Row> {
    let mut merged = Vec::new();
    while merged.len() < limit {
        let best = lists
            .iter()
            .enumerate()
            .filter_map(|(index, list)| list.front().map(|row| (index, row)))
            .max_by(|(left_index, left), (right_index, right)| {
                match (left.regex_rank(), right.regex_rank()) {
                    (Some(left_rank), Some(right_rank)) => right_rank
                        .cmp(&left_rank)
                        .then_with(|| right.wire["file"].as_str().cmp(&left.wire["file"].as_str())),
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (None, None) => left
                        .score("score")
                        .total_cmp(&right.score("score"))
                        .then_with(|| {
                            left.score("semantic_score")
                                .total_cmp(&right.score("semantic_score"))
                        }),
                }
                .then_with(|| right_index.cmp(left_index))
            })
            .map(|(index, _)| index);
        let Some(index) = best else {
            break;
        };
        if let Some(row) = lists[index].pop_front() {
            merged.push(row);
        }
    }
    merged
}

#[derive(Default)]
struct PatternRendering {
    grep: Option<GrepResult>,
    ranked: bool,
    candidate_files: usize,
    files_examined: usize,
    examination_capped: bool,
    split: split_query::PatternSummary,
    snippets_incomplete: bool,
}

impl PatternRendering {
    fn collect(&mut self, page: ChildPatternPage, root: &Path, child: &Child) -> Vec<RowText> {
        match page {
            ChildPatternPage::Regex(mut page) => {
                self.ranked |= page.ranked.is_some();
                self.candidate_files += page.candidate_files;
                if page.ranked.is_some() {
                    self.files_examined += page.summary.files_searched;
                }
                self.examination_capped |= page.ranked.is_some() && page.summary.engine_capped;
                let matches = std::mem::take(&mut page.summary.matches);
                if let Some(total) = &mut self.grep {
                    total.files_with_matches += page.summary.files_with_matches;
                    total.total_matches += page.summary.total_matches;
                    total.files_searched += page.summary.files_searched;
                    total.truncated |= page.summary.truncated;
                    total.engine_capped |= page.summary.engine_capped;
                    total.index_read_gaps.extend(page.summary.index_read_gaps);
                    if total.index_status == IndexStatus::Ready {
                        total.index_status = page.summary.index_status;
                    }
                } else {
                    self.grep = Some(page.summary);
                }
                match page.ranked {
                    Some(files) => files.into_iter().map(RowText::Ranked).collect(),
                    None => matches.into_iter().map(RowText::Grep).collect(),
                }
            }
            ChildPatternPage::Split(mut page) => {
                for (file, _) in &mut page.summary.sites {
                    *file = prefix_path(root, child, file).into();
                }
                self.split.merge(page.summary);
                self.snippets_incomplete |= page.snippets_incomplete;
                page.results.into_iter().map(RowText::Split).collect()
            }
        }
    }

    fn render(
        self,
        rows: Vec<RowText>,
        pattern: &str,
        root: &Path,
        ctx: &AppContext,
        offset: usize,
    ) -> String {
        if let Some(mut grep) = self.grep {
            let no_match = rows.is_empty() && offset == 0 && grep.index_read_gaps.is_empty();
            let note = self.examination_capped.then(|| {
                regex_route::examination_disclosure(self.files_examined, self.candidate_files)
            });
            let mut ranked = Vec::new();
            for row in rows {
                match row {
                    RowText::Ranked(file) => ranked.push(file),
                    RowText::Grep(line) if self.ranked => {
                        ranked.push(regex_route::RankedFile::unranked(line))
                    }
                    RowText::Grep(line) => grep.matches.push(line),
                    RowText::Split(_) => {
                        unreachable!("a pattern-only page cannot contain split results")
                    }
                }
            }
            regex_route::format_search_page(
                self.ranked.then_some(&ranked),
                &grep,
                root,
                "regex",
                note.as_deref(),
                no_match,
            )
        } else {
            let results = rows
                .into_iter()
                .filter_map(|row| match row {
                    RowText::Split(result) => Some(result),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let (text, _) = semantic_search::format_ranked_text_counted(
                &results,
                root,
                false,
                self.snippets_incomplete,
                Some(ctx),
                None,
            );
            format!("{}\n\n{text}", self.split.line(pattern, root))
        }
    }
}

fn row_with_text(mut wire: Value, mut text: Option<RowText>, root: &Path, child: &Child) -> Row {
    prefix_row(&mut wire, root, child);
    if let Some(path) = wire["file"].as_str() {
        match &mut text {
            Some(RowText::Ranked(file)) => {
                for line in &mut file.lines {
                    line.file = path.into();
                }
            }
            Some(RowText::Grep(line)) => line.file = path.into(),
            Some(RowText::Split(result)) => result.file = path.into(),
            None => {}
        }
    }
    Row { wire, text }
}

fn render_text(query: &str, page: &[Value], more_available: bool) -> String {
    if page.is_empty() {
        return format!("No results for \"{query}\" in this parent folder's repositories.");
    }
    let mut lines = page
        .iter()
        .map(|row| {
            let location = format!(
                "{}:{}-{}",
                row["file"].as_str().unwrap_or_default(),
                row["start_line"]
                    .as_u64()
                    .or_else(|| row["line"].as_u64())
                    .unwrap_or(0),
                row["end_line"]
                    .as_u64()
                    .or_else(|| row["line"].as_u64())
                    .unwrap_or(0)
            );
            let mut line = format!("{location} {}", row["name"].as_str().unwrap_or_default());
            if let Some(first) = row["snippet"]
                .as_str()
                .or_else(|| row["line_text"].as_str())
                .and_then(|snippet| snippet.lines().map(str::trim).find(|line| !line.is_empty()))
            {
                line.push_str(&format!("\n  {first}"));
            }
            line
        })
        .collect::<Vec<_>>();
    if more_available {
        lines.push("(More results are available: raise topK or pass offset.)".into());
    }
    lines.join("\n")
}

pub(super) fn search(req: &RawRequest, ctx: &AppContext, session: &ParentSession) -> Response {
    crate::bounded_io::with_skip_disclosure(|| search_with_disclosure(req, ctx, session))
}

fn search_with_disclosure(req: &RawRequest, ctx: &AppContext, session: &ParentSession) -> Response {
    let semantic_search::entry::SearchInput { query, pattern } =
        match semantic_search::entry::SearchInput::parse(req) {
            Ok(input) => input,
            Err(response) => return response,
        };
    let pattern = pattern.map(|(text, _)| text);
    let query_for_embedding = (!query.trim().is_empty()).then_some(query);
    let top_k = req
        .params
        .get("top_k")
        .or_else(|| req.params.get("topK"))
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TOP_K);
    let offset = req
        .params
        .get("offset")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(0);
    let include_tests = req
        .params
        .get("include_tests")
        .or_else(|| req.params.get("includeTests"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope = match req.params.get("path").and_then(Value::as_str) {
        Some(raw) => match ctx.validate_read_path(&req.id, req.session(), Path::new(raw)) {
            Ok(path) => {
                let path = if path.is_relative() {
                    session.root().join(path)
                } else {
                    path
                };
                std::fs::canonicalize(&path).unwrap_or(path)
            }
            Err(response) => return response,
        },
        None => session.root().to_path_buf(),
    };

    let mut gaps = Gaps::default();
    if !crate::parent_search::query::discovered(session, &mut gaps) {
        let mut body = json!({
            "status": "building",
            "complete": false,
            "text": "",
            "query": query,
            "include_tests": include_tests,
            "result_count": 0,
            "results": [],
            "more_available": false,
            "engine_capped": false,
            // No search ran yet, so no answer was read from files directly.
            "fully_degraded": false,
        });
        attach_gaps(&mut body, gaps.into_values());
        return Response::success(&req.id, body);
    }
    let wanted = offset.saturating_add(top_k);
    // With semantic search on, the session embeds the query once with its
    // one model; every child engine then runs in parallel with that vector in
    // place of its own embedding, so no child starts a model. If the query
    // cannot be embedded, the model is lent to each engine in turn instead,
    // which reports the failure the way a direct search would.
    let semantic = read(&session.semantic).clone();
    let mut model = query_for_embedding
        .and_then(|_| semantic.ready())
        .and_then(|semantic| lock(&semantic.model).take());
    let vector = model.as_mut().and_then(|model| {
        model
            .embed_query_cached(
                query_for_embedding?,
                crate::semantic_index::QueryBudget::from_config(&session.semantic_config),
            )
            .ok()
    });
    if vector.is_some() {
        if let Some(semantic) = semantic.ready() {
            *lock(&semantic.model) = model.take();
        }
    }
    if let Some(reason) = semantic.gap_reason("semantic") {
        // A normal answer without semantic search is incomplete too; the
        // parent says why once instead of once per child.
        gaps.push("parent_semantic_unavailable", ".".into(), reason);
    }
    let children = session
        .children()
        .into_iter()
        .filter(|child| child.root.starts_with(&scope) || scope.starts_with(&child.root))
        .collect::<Vec<_>>();
    // With the data layer, each child's engine answers from the checkout the
    // session holds for that child, as grep does: its text plane, and its
    // meaning plane reading the vectors the repository's store holds.
    let data_children = data_layer_answers().then(|| {
        let roots = children
            .iter()
            .map(|child| child.root.as_path())
            .collect::<Vec<_>>();
        let meaning = semantic
            .ready()
            .filter(|_| session.planes.semantic)
            .map(|semantic| (&session.semantic_config, &semantic.fingerprint));
        crate::data_layer::parent::search_children(ctx, session.root(), &roots, meaning)
    });
    let checkouts = match &data_children {
        Some(searches) => searches
            .iter()
            .map(|search| search.checkout.clone())
            .collect::<Vec<_>>(),
        None => vec![None; children.len()],
    };
    let skip_scope = crate::bounded_io::capture();
    let run = |child: &Child,
               checkout: Option<&Arc<crate::data_layer::runtime::DataCheckout>>,
               lent: Option<crate::semantic_index::EmbeddingModel>,
               vector: Option<&Vec<f32>>| {
        let mut engine = lock(&child.engine);
        let engine = engine.get_or_insert_with(|| ChildEngine::new(session, child));
        if let Some(checkout) = checkout {
            engine.ctx.pin_data_checkout(Arc::clone(checkout));
        }
        let mut params = req
            .params
            .as_object()
            .expect("search parameter object")
            .clone();
        // Each child supplies the prefix of its list; the offset is applied
        // once to the merged list, not separately to each child's list.
        for key in ["topK", "offset", "includeTests", "path"] {
            params.remove(key);
        }
        params.insert("top_k".into(), json!(wanted));
        params.insert("include_tests".into(), json!(include_tests));
        if scope != session.root() && scope.starts_with(&child.root) {
            params.insert("path".into(), json!(scope));
        }
        let child_req = RawRequest {
            id: req.id.clone(),
            command: req.command.clone(),
            lsp_hints: None,
            session_id: None,
            params: Value::Object(params),
        };
        let lending = lent.is_some();
        if lending {
            *engine.ctx.semantic_embedding_model().lock() = lent;
        }
        let mut pattern_page = None;
        let mut search = || {
            semantic_search::handle_child_search(
                &child_req,
                &engine.ctx,
                pattern.map(|_| &mut pattern_page),
            )
        };
        let response = crate::bounded_io::with_budget(skip_scope.clone(), || {
            match (query_for_embedding, vector) {
                (Some(query), Some(vector)) => {
                    semantic_search::with_precomputed_query_vector(query, vector.clone(), search)
                }
                _ => search(),
            }
        });
        let returned = if lending {
            engine.ctx.semantic_embedding_model().lock().take()
        } else {
            None
        };
        if !response.success {
            return (
                Err(format!(
                    "aft_search failed in this repository: {}",
                    response.data["message"].as_str().unwrap_or("unknown error")
                )),
                returned,
            );
        }
        (Ok((response.data, pattern_page)), returned)
    };
    let outcomes = if model.is_some() {
        let mut outcomes = Vec::new();
        for (child, checkout) in children.iter().zip(&checkouts) {
            let (outcome, returned) = run(child, checkout.as_ref(), model.take(), None);
            model = returned;
            outcomes.push(outcome);
        }
        if let Some(semantic) = semantic.ready() {
            *lock(&semantic.model) = model;
        }
        outcomes
    } else {
        use rayon::prelude::*;
        children
            .par_iter()
            .zip(checkouts.par_iter())
            .map(|(child, checkout)| run(child, checkout.as_ref(), None, vector.as_ref()).0)
            .collect::<Vec<_>>()
    };

    let mut lists = Vec::new();
    let mut first: Option<Value> = None;
    let mut more_available = false;
    let mut engine_capped = false;
    let mut fully_degraded = true;
    let mut lexical_only_fallback = false;
    let mut bounded_disk_fallback = false;
    let mut semantic_statuses = Vec::new();
    let mut rendering = PatternRendering::default();
    for (index, (child, outcome)) in children.iter().zip(outcomes).enumerate() {
        let data_child = data_children.as_ref().map(|searches| &searches[index]);
        if session.planes.trigram {
            let reason = data_child.and_then(|search| search.text.gap_reason("trigram"));
            if let Some(reason) = reason {
                gaps.child(child, reason);
            }
        }
        if session.planes.semantic && semantic.ready().is_some() {
            let reason = data_child.and_then(|search| search.meaning_gap.clone());
            if let Some(reason) = reason {
                gaps.child(child, reason);
            }
        }
        let (data, pattern_page) = match outcome {
            Ok(data) => data,
            Err(reason) => {
                gaps.child(child, reason);
                continue;
            }
        };
        // Without semantic search every child's answer is incomplete for the
        // reason already named once above.
        if data["complete"] == false && semantic.ready().is_some() {
            gaps.child(
                child,
                format!(
                    "aft_search: this repository's answer is incomplete (semantic status: {})",
                    data["semantic_status"].as_str().unwrap_or("unknown")
                ),
            );
        }
        more_available |= data["more_available"].as_bool().unwrap_or(false);
        engine_capped |= data["engine_capped"].as_bool().unwrap_or(false);
        fully_degraded &= data["fully_degraded"].as_bool().unwrap_or(false);
        lexical_only_fallback |= data["lexical_only_fallback"].as_bool().unwrap_or(false);
        bounded_disk_fallback |=
            data["lexical_only_fallback"] == true && data["fully_degraded"] == true;
        if let Some(status) = data["semantic_status"].as_str() {
            semantic_statuses.push(status.to_owned());
        }
        let mut display_rows = pattern_page
            .map(|page| rendering.collect(page, session.root(), child))
            .unwrap_or_default()
            .into_iter();
        let rows = data["results"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|row| row_with_text(row, display_rows.next(), session.root(), child))
            .collect::<VecDeque<_>>();
        lists.push(rows);
        first.get_or_insert(data);
    }
    gaps.scope(session, &scope);

    let total = lists.iter().map(VecDeque::len).sum::<usize>();
    let merged = merge(lists, wanted);
    more_available |= total > wanted;
    let (page, display_rows): (Vec<_>, Vec<_>) = merged
        .into_iter()
        .skip(offset)
        .map(|row| (row.wire, row.text))
        .unzip();
    let envelope = crate::list_envelope::ListEnvelope::new(
        page.len(),
        if more_available {
            crate::list_envelope::Total::AtLeast(offset + page.len() + 1)
        } else {
            crate::list_envelope::Total::Exact(total)
        },
        crate::list_surfaces::search::SEARCH_UNIT,
        if more_available {
            vec![crate::list_envelope::Reason::Cap]
        } else {
            Vec::new()
        },
        crate::list_surfaces::search::SEARCH_NARROW,
    );
    semantic_statuses.sort();
    semantic_statuses.dedup();
    let semantic_status = match semantic_statuses.as_slice() {
        [single] => single.clone(),
        [] => "unavailable".to_string(),
        _ => "partial".to_string(),
    };
    let searched = first.is_some();
    let first = first.unwrap_or(Value::Null);
    let mut text = if searched {
        match pattern {
            Some(pattern) => rendering.render(
                display_rows.into_iter().flatten().collect(),
                pattern,
                session.root(),
                ctx,
                offset,
            ),
            None => render_text(query, &page, more_available),
        }
    } else {
        let search = query_for_embedding.or(pattern).unwrap_or_default();
        format!("Search unavailable for \"{search}\" in this parent folder's repositories.")
    };
    if bounded_disk_fallback {
        text.push_str(
            "\n\n(Child indexes unavailable: literal matches were read from a bounded disk scan.)",
        );
    }
    let mut body = json!({
        "status": first["status"].as_str().unwrap_or("unavailable"),
        "complete": true,
        "text": text,
        "query": query,
        "include_tests": include_tests,
        "interpreted_as": first["interpreted_as"],
        "query_kind": first["query_kind"],
        "result_count": page.len(),
        "results": page,
        "more_available": more_available,
        "engine_capped": engine_capped,
        // Every child that answered read its files directly; with no child
        // answer at all, nothing was read.
        "fully_degraded": fully_degraded && searched,
        "lexical_only_fallback": lexical_only_fallback,
        "semantic_status": semantic_status,
    });
    if let Some(object) = body.as_object_mut() {
        crate::list_surfaces::search::attach_projected_search_envelope(object, &envelope);
    }
    attach_gaps(&mut body, gaps.into_values());
    Response::success(&req.id, body)
}
