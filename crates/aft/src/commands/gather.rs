use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::commands::callgraph_store_adapter::{
    building_response, call_tree_result, impact_result, store_error_response, unavailable_response,
};
use crate::commands::semantic_search::handle_semantic_search;
use crate::commands::symbol_render::{
    render_symbol_within_budget, symbol_kind_string, BudgetedSymbolRenderStatus,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::grep_executor;
use crate::parser::detect_language;
use crate::protocol::{RawRequest, Response};

const DEFAULT_BUDGET: usize = 400;
const MAX_BUDGET: usize = 800;

/// Render-error marker matched by callee-suppression guard in build_pack.
/// The guard suppresses unresolved external callees from the stub list;
/// this constant ties the producer (render_symbol_section) and consumer
/// (suppression check) so the literal cannot drift apart.
const UNRESOLVED_MARKER: &str = "(symbol not resolved)";

/// Provenance prefix for callee neighbors — used by the suppression guard
/// to scope unresolved suppression to external callees only (not seeds
/// or callers, which remain visible stubs).
const CALLEE_PROVENANCE_PREFIX: &str = "callee-of-";

/// Normalize a file path to a consistent form for deduplication.
/// Converts absolute paths to repo-relative by stripping the project root
/// prefix, so that `/home/user/project/src/a.rs` and `src/a.rs` match.
fn normalize_file_path(raw: &str, project_root: &Path) -> String {
    let normalized_raw = raw.replace('\\', "/");
    let normalized_root = project_root.to_string_lossy().replace('\\', "/");
    let path = Path::new(&normalized_raw);
    let root = Path::new(&normalized_root);
    if let Ok(stripped) = path.strip_prefix(root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    // Strip any leading `./` for consistent relative form.
    normalized_raw.trim_start_matches("./").to_string()
}

/// Resolve the symbol that contains a given line in a file.
/// Uses `list_symbols` to enumerate all symbols, then returns the one
/// whose range contains `line` (1-based). Returns `None` when no symbol
/// covers the line (comment, blank, import, etc.).
fn resolve_containing_symbol(
    file_path: &Path,
    line: u32,
    ctx: &AppContext,
) -> Option<(String, u32)> {
    if line == 0 {
        return None;
    }
    let symbols = ctx.provider().list_symbols(file_path).ok()?;
    let line_0b = line.saturating_sub(1);
    // Prefer the innermost (smallest) containing symbol.
    symbols
        .iter()
        .filter(|s| s.range.start_line <= line_0b && line_0b <= s.range.end_line)
        .min_by_key(|s| {
            // Smaller range = more specific (innermost).
            s.range.end_line.saturating_sub(s.range.start_line)
        })
        .map(|s| (s.name.clone(), s.range.start_line.saturating_add(1)))
}

/// A candidate symbol to include in the pack.
#[derive(Debug, Clone)]
struct PackCandidate {
    file: String,
    name: String,
    start_line: u32,
    /// Where this candidate came from, for provenance in the pack output.
    provenance: String,
    /// Score for ranking (search score, or 0 for callgraph-derived).
    score: f32,
    /// Seed ordinal (0-based) if this is a seed; None for neighbors.
    seed_ordinal: Option<usize>,
    /// Hop distance from seed (0 for seeds, 1 for direct neighbors). Used for
    /// interleaving: candidates are ordered by (seed_ordinal, hop_distance) so
    /// a seed's body appears before its callers, then its callees.
    #[allow(dead_code)]
    hop_distance: u32,
}

/// Handle a `gather` request — assemble a deterministic context pack.
pub fn handle_gather(req: &RawRequest, ctx: &AppContext) -> Response {
    let question = req.params.get("question").and_then(|v| v.as_str());
    let symbol = req.params.get("symbol").and_then(|v| v.as_str());
    let file_path_str = req.params.get("filePath").and_then(|v| v.as_str());

    // Modes are mutually exclusive.
    let has_question = question.is_some();
    let has_symbol = symbol.is_some() && file_path_str.is_some();
    if has_question == has_symbol {
        return Response::error(
            &req.id,
            "invalid_request",
            "aft_gather_context: provide exactly ONE mode — either 'question' OR 'symbol'+'filePath'",
        );
    }

    let budget = req
        .params
        .get("budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_BUDGET as u64)
        .min(MAX_BUDGET as u64) as usize;
    let include_tests = req
        .params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if has_question {
        let q = question.unwrap();
        handle_gather_question(req, ctx, q, budget, include_tests)
    } else {
        let s = symbol.unwrap();
        let fp = file_path_str.unwrap();
        handle_gather_symbol(req, ctx, s, fp, budget, include_tests)
    }
}

fn handle_gather_question(
    req: &RawRequest,
    ctx: &AppContext,
    question: &str,
    budget: usize,
    include_tests: bool,
) -> Response {
    let search_req = RawRequest {
        id: format!("{}_search", req.id),
        command: "semantic_search".to_string(),
        lsp_hints: req.lsp_hints.clone(),
        session_id: req.session_id.clone(),
        params: serde_json::json!({
            "query": question,
            "top_k": 15,
            "hint": "auto",
            "include_tests": include_tests,
        }),
    };

    let search_resp = handle_semantic_search(&search_req, ctx);
    if !search_resp.success {
        return Response::error(
            &req.id,
            "search_failed",
            format!(
                "aft_gather_context: search failed: {}",
                serde_json::to_string(&search_resp.data).unwrap_or_default()
            ),
        );
    }

    let results = match search_resp.data.get("results").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            return Response::error(
                &req.id,
                "search_failed",
                "aft_gather_context: search returned no results array",
            );
        }
    };

    // Detect semantic-index degradation from the search response.
    // When the index is building, NL queries fall back to lexical-only
    // FileSummary results that carry no symbol names.
    let semantic_status = search_resp
        .data
        .get("semantic_status")
        .and_then(|v| v.as_str())
        .unwrap_or("ready");

    let project_root = grep_executor::project_root(ctx);

    let mut seeds: Vec<PackCandidate> = Vec::new();
    let mut no_symbol_stubs: Vec<String> = Vec::new();
    for result in results {
        let raw_file = result.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let name_str = result.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let score = result.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let start_line = result
            .get("start_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        // Handle grep-fallback results that carry a file+line but no symbol
        // name. Resolve the containing symbol by line instead of extracting
        // a name from the line text.
        let (name, resolved_start_line) = if !name_str.is_empty() {
            (name_str.to_string(), start_line)
        } else {
            // Resolve search-result paths against project_root so
            // filesystem ops work regardless of process CWD.
            let raw_file_path = if Path::new(raw_file).is_absolute() {
                PathBuf::from(raw_file)
            } else {
                project_root.join(raw_file)
            };
            let line = if start_line == 0 {
                result.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32
            } else {
                start_line
            };
            if !raw_file_path.exists() || line == 0 {
                // File doesn't exist or no line number — visible stub.
                no_symbol_stubs.push(format!(
                    "{}:{} — {} (no containing symbol)",
                    raw_file,
                    line.max(1),
                    format!("search score={:.3}", score),
                ));
                continue;
            }
            match resolve_containing_symbol(&raw_file_path, line, ctx) {
                Some((symbol_name, sym_start)) => (symbol_name, sym_start),
                None => {
                    // No symbol contains this line (comment, import, blank).
                    no_symbol_stubs.push(format!(
                        "{}:{} — {} (no containing symbol)",
                        raw_file,
                        line.max(1),
                        format!("search score={:.3}", score),
                    ));
                    continue;
                }
            }
        };

        if raw_file.is_empty() || name.is_empty() {
            continue;
        }

        seeds.push(PackCandidate {
            file: normalize_file_path(raw_file, &project_root),
            name,
            start_line: if start_line == 0 {
                resolved_start_line
            } else {
                start_line
            },
            provenance: format!("search score={:.3}", score),
            score,
            seed_ordinal: None, // filled in after sorting
            hop_distance: 0,
        });
    }

    seeds.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, seed) in seeds.iter_mut().enumerate() {
        seed.seed_ordinal = Some(i);
    }

    if seeds.is_empty() {
        if no_symbol_stubs.is_empty() {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "text": "aft_gather_context: no results found for question",
                }),
            );
        }
        // All hits were no-containing-symbol — produce a pack of visible stubs.
        let degraded = semantic_status != "ready";
        return build_pack(
            req,
            ctx,
            &[],
            &[],
            budget,
            "question",
            question,
            &no_symbol_stubs,
            degraded,
            false,
            &project_root,
        );
    }

    // Detect callgraph store unavailability so the header carries a notice
    // (question mode is best-effort — seeds alone are still useful).
    let callgraph_unavailable = !matches!(
        ctx.callgraph_store_for_ops(),
        CallgraphStoreAccess::Ready(_)
    );
    let neighbors = collect_callgraph_neighbors(ctx, &seeds, &project_root, include_tests);

    // Only flag degradation when zero symbol-level seeds resolved.
    // Even one real seed means the pack has usable content.
    // Intentionally always-false when seeds is non-empty — a degraded
    // semantic index that still produces symbol-level results is
    // functional enough; flagging it would be noise.
    let degraded = semantic_status != "ready" && seeds.is_empty();
    build_pack(
        req,
        ctx,
        &seeds,
        &neighbors,
        budget,
        "question",
        question,
        &no_symbol_stubs,
        degraded,
        callgraph_unavailable,
        &project_root,
    )
}

fn handle_gather_symbol(
    req: &RawRequest,
    ctx: &AppContext,
    symbol: &str,
    file_path_str: &str,
    budget: usize,
    include_tests: bool,
) -> Response {
    let file_path = match ctx.validate_path(&req.id, Path::new(file_path_str)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let store = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => store,
        CallgraphStoreAccess::Building => return building_response(&req.id, "gather"),
        CallgraphStoreAccess::Unavailable => {
            return unavailable_response(&req.id, "gather", ctx.is_worktree_bridge())
        }
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "gather", error)
        }
    };

    let impact = match impact_result(store.as_ref(), &file_path, symbol, 1, include_tests) {
        Ok(result) => result,
        Err(error) => {
            return store_error_response(&req.id, "gather", error);
        }
    };

    let project_root = grep_executor::project_root(ctx);
    let file_display = file_path.display().to_string();
    let seeds = vec![PackCandidate {
        file: normalize_file_path(&file_display, &project_root),
        name: symbol.to_string(),
        start_line: 0, // will be found by resolve
        provenance: "seed (impact target)".to_string(),
        score: 1.0,
        seed_ordinal: Some(0),
        hop_distance: 0,
    }];

    let mut neighbors: Vec<PackCandidate> = Vec::new();
    for caller in &impact.callers {
        neighbors.push(PackCandidate {
            file: normalize_file_path(&caller.caller_file, &project_root),
            name: caller.caller_symbol.clone(),
            start_line: caller.line,
            provenance: format!("caller-of-{}", symbol),
            score: 0.0,
            seed_ordinal: Some(0),
            hop_distance: 1,
        });
    }

    let callee_neighbors = collect_callees_for_seed(
        ctx,
        store.as_ref(),
        &file_path,
        symbol,
        0,
        &project_root,
        include_tests,
    );
    neighbors.extend(callee_neighbors);

    let mode_desc = format!("impact({}:{})", file_display, symbol);
    build_pack(
        req,
        ctx,
        &seeds,
        &neighbors,
        budget,
        &mode_desc,
        "",
        &[],
        false,
        false,
        &project_root,
    )
}

/// Collect 1-hop callgraph neighbors (callers + callees) for each seed.
///
/// Both the caller query below and `collect_callees_for_seed` must forward
/// `include_tests`. Passing the literal `false` to either one reads as correct
/// (`false` is the default) while silently making the flag inert for that half
/// of the neighbor set — the exact drift a partial conversion produces.
fn collect_callgraph_neighbors(
    ctx: &AppContext,
    seeds: &[PackCandidate],
    project_root: &Path,
    include_tests: bool,
) -> Vec<PackCandidate> {
    let store = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => store,
        _ => return Vec::new(),
    };

    let mut neighbors = Vec::new();
    for (seed_idx, seed) in seeds.iter().enumerate() {
        let seed_path = Path::new(&seed.file);
        let seed_name = &seed.name;

        if let Ok(result) = impact_result(store.as_ref(), seed_path, seed_name, 1, include_tests) {
            for caller in &result.callers {
                neighbors.push(PackCandidate {
                    file: normalize_file_path(&caller.caller_file, project_root),
                    name: caller.caller_symbol.clone(),
                    start_line: caller.line,
                    provenance: format!("caller-of-{}", seed_name),
                    score: 0.0,
                    seed_ordinal: Some(seed_idx),
                    hop_distance: 1,
                });
            }
        }

        neighbors.extend(collect_callees_for_seed(
            ctx,
            store.as_ref(),
            seed_path,
            seed_name,
            seed_idx,
            project_root,
            include_tests,
        ));
    }

    neighbors
}

/// Collect direct callees for a single seed symbol.
fn collect_callees_for_seed(
    _ctx: &AppContext,
    store: &impl crate::callgraph_store::CallGraphRead,
    file_path: &Path,
    symbol: &str,
    seed_idx: usize,
    project_root: &Path,
    include_tests: bool,
) -> Vec<PackCandidate> {
    let mut callees = Vec::new();
    if let Ok(tree) = call_tree_result(store, file_path, symbol, 1, include_tests) {
        for child in &tree.children {
            callees.push(PackCandidate {
                file: normalize_file_path(&child.file, project_root),
                name: child.name.clone(),
                start_line: child.line,
                provenance: format!("{}{}", CALLEE_PROVENANCE_PREFIX, symbol),
                score: 0.0,
                seed_ordinal: Some(seed_idx),
                hop_distance: 1,
            });
        }
    }
    callees
}

/// Assemble the final pack text.
/// `pre_stubs` are no-containing-symbol hits from grep-fallback results
/// that must appear as visible stubs (nothing-silently-dropped contract).
fn build_pack(
    req: &RawRequest,
    ctx: &AppContext,
    seeds: &[PackCandidate],
    neighbors: &[PackCandidate],
    budget: usize,
    mode: &str,
    question: &str,
    pre_stubs: &[String],
    degraded: bool,
    callgraph_unavailable: bool,
    project_root: &Path,
) -> Response {
    // Deduplicate by (file, name). Seeds win over neighbors — they carry
    // relevance (search score / impact target). Seeds are emitted first,
    // then neighbors interleaved per seed. This ordering is intentional:
    // under a budget cut, every seed must be included before any neighbor
    // consumes lines, because seeds are the primary evidence the agent
    // asked for.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut ordered: Vec<&PackCandidate> = Vec::new();

    for seed in seeds {
        let key = (seed.file.clone(), seed.name.clone());
        if seen.insert(key) {
            ordered.push(seed);
        }
    }

    for seed in seeds {
        let seed_idx = seed.seed_ordinal;
        for neighbor in neighbors {
            if neighbor.seed_ordinal == seed_idx {
                let key = (neighbor.file.clone(), neighbor.name.clone());
                if seen.insert(key) {
                    ordered.push(neighbor);
                }
            }
        }
    }

    for neighbor in neighbors {
        if neighbor.seed_ordinal.is_none() {
            let key = (neighbor.file.clone(), neighbor.name.clone());
            if seen.insert(key) {
                ordered.push(neighbor);
            }
        }
    }

    let mut lines_used: usize = 0;
    let mut body = String::new();

    let mut stubs: Vec<String> = Vec::new();
    let mut unresolved_count: usize = 0;

    // Per-symbol budget: remaining budget / remaining candidates (at least 10).
    let total_candidates = ordered.len();

    for (i, candidate) in ordered.iter().enumerate() {
        let remaining = total_candidates - i;
        // .max(10) guarantees per_symbol_budget ≥ 10, so the only
        // budget-exhausted shortfall is caught by `lines_used >= budget`.
        let per_symbol_budget = (budget.saturating_sub(lines_used))
            .saturating_div(remaining)
            .max(10)
            .min(150); // cap per symbol at 150 lines

        if lines_used >= budget {
            stubs.push(format!(
                "{}:{} {} — {}",
                candidate.file,
                candidate.start_line.max(1),
                candidate.name,
                candidate.provenance
            ));
            continue;
        }

        match render_symbol_section(ctx, candidate, per_symbol_budget, project_root) {
            Ok(section) => {
                let section_lines = section.lines().count();
                if lines_used + section_lines + 1 > budget {
                    // Would exceed budget — stub it instead.
                    stubs.push(format!(
                        "{}:{} {} — {}",
                        candidate.file,
                        candidate.start_line.max(1),
                        candidate.name,
                        candidate.provenance
                    ));
                    continue;
                }
                body.push_str(&section);
                body.push('\n');
                lines_used += section_lines + 1; // section + trailing newline
            }
            Err(stub) => {
                // Suppress unresolved EXTERNAL callees — stdlib/prelude symbols
                // (e.g. `readFileSync`, `parse`, `sorted`) are never renderable
                // and bury genuine expandable stubs. Unresolved seeds and
                // callers remain as visible individual stubs so nothing is
                // silently dropped.
                if stub.contains(UNRESOLVED_MARKER)
                    && candidate.provenance.starts_with(CALLEE_PROVENANCE_PREFIX)
                {
                    unresolved_count += 1;
                } else {
                    stubs.push(stub);
                }
            }
        }
    }

    // Build header after rendering so used=N is exact and the replacement
    // cannot collide with query text (e.g. a query containing "used=").
    let degraded_notice = if degraded {
        " | degraded=semantic-index-building (partial results — retry when index ready)"
    } else {
        ""
    };
    let callgraph_notice = if callgraph_unavailable {
        " | neighbors=skipped(callgraph-unavailable)"
    } else {
        ""
    };

    let total_lines = lines_used + 1; // +1 for the header line itself
    let header = if question.is_empty() {
        format!(
            "## gather pack | mode={}{}{} | seeds={} | neighbors={} | budget={} used={}",
            mode,
            degraded_notice,
            callgraph_notice,
            seeds.len(),
            neighbors.len(),
            budget,
            total_lines,
        )
    } else {
        format!(
            "## gather pack | mode={}{}{} | query=\"{}\" | seeds={} | neighbors={} | budget={} used={}",
            mode,
            degraded_notice,
            callgraph_notice,
            truncate_str(question, 80),
            seeds.len(),
            neighbors.len(),
            budget,
            total_lines,
        )
    };

    let mut output = header;
    output.push('\n');
    output.push_str(&body);

    if !stubs.is_empty() || !pre_stubs.is_empty() {
        output.push_str("\n## Beyond budget (zoom to expand)\n");
        for stub in pre_stubs {
            output.push_str(stub);
            output.push('\n');
        }
        for stub in &stubs {
            output.push_str(stub);
            output.push('\n');
        }
        if unresolved_count > 0 {
            output.push_str(&format!(
                "({} unresolved external calls omitted)\n",
                unresolved_count
            ));
        }
    } else if unresolved_count > 0 {
        output.push_str(&format!(
            "\n## Beyond budget (zoom to expand)\n({} unresolved external calls omitted)\n",
            unresolved_count
        ));
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "text": output,
        }),
    )
}

/// Select the best match when multiple same-name symbols exist in one file.
/// If the candidate carries a non-zero `start_line` (1-based, from search results),
/// prefer the match whose range contains (or starts nearest to) that line.
/// Falls back to matches[0] when no line hint is available.
fn select_symbol_match<'a>(
    matches: &'a [crate::symbols::SymbolMatch],
    start_line: u32,
) -> &'a crate::symbols::Symbol {
    if start_line == 0 || matches.len() <= 1 {
        return &matches[0].symbol;
    }
    // start_line is 1-based (serialized Range convention); Symbol range fields
    // are 0-indexed internally — subtract 1 for comparison.
    let hint_line_0b = (start_line.saturating_sub(1)) as u32;
    matches
        .iter()
        .min_by_key(|m| {
            let r = &m.symbol.range;
            if r.start_line <= hint_line_0b && hint_line_0b <= r.end_line {
                0 // exact containment
            } else if r.start_line > hint_line_0b {
                (r.start_line - hint_line_0b) as u64
            } else {
                (hint_line_0b - r.end_line) as u64 + (u32::MAX as u64)
            }
        })
        .map(|m| &m.symbol)
        .unwrap_or(&matches[0].symbol)
}

/// Render a single symbol as a markdown section for the pack.
/// Returns Ok(section_text) on success, or Err(stub_line) if the symbol can't be resolved.
fn render_symbol_section(
    ctx: &AppContext,
    candidate: &PackCandidate,
    per_symbol_budget: usize,
    project_root: &Path,
) -> Result<String, String> {
    let file_path = if Path::new(&candidate.file).is_absolute() {
        PathBuf::from(&candidate.file)
    } else {
        project_root.join(&candidate.file)
    };
    if !file_path.exists() {
        return Err(format!(
            "{}:{} {} — {} (file not found)",
            candidate.file,
            candidate.start_line.max(1),
            candidate.name,
            candidate.provenance
        ));
    }

    let source = match std::fs::read_to_string(&file_path) {
        Ok(s) => s,
        Err(e) => {
            return Err(format!(
                "{}:{} {} — {} (read error: {})",
                candidate.file,
                candidate.start_line.max(1),
                candidate.name,
                candidate.provenance,
                e
            ));
        }
    };
    let lines: Vec<String> = source.lines().map(|l| l.to_string()).collect();

    let matches = match ctx.provider().resolve_symbol(&file_path, &candidate.name) {
        Ok(m) => m,
        Err(_) => {
            return Err(format!(
                "{}:{} {} — {} {}",
                candidate.file,
                candidate.start_line.max(1),
                candidate.name,
                candidate.provenance,
                UNRESOLVED_MARKER,
            ));
        }
    };

    if matches.is_empty() {
        return Err(format!(
            "{}:{} {} — {} (symbol not found)",
            candidate.file,
            candidate.start_line.max(1),
            candidate.name,
            candidate.provenance
        ));
    }

    let target_symbol = select_symbol_match(&matches, candidate.start_line);

    let lang = detect_language(&file_path);
    let kind_str = symbol_kind_string(&target_symbol.kind);

    let rendered =
        render_symbol_within_budget(target_symbol, &lines, lang, None, per_symbol_budget);
    let body = rendered.content.trim().to_string();
    if body.is_empty() {
        return Err(format!(
            "{}:{} {} — {} (empty body)",
            candidate.file,
            target_symbol.range.start_line + 1, // 1-based for display
            candidate.name,
            candidate.provenance
        ));
    }

    let header = format!(
        "## {}:{} {} {}",
        candidate.file,
        target_symbol.range.start_line + 1, // 1-based for display
        kind_str,
        candidate.name,
    );

    let truncated_note = match rendered.status {
        BudgetedSymbolRenderStatus::Complete => String::new(),
        BudgetedSymbolRenderStatus::Truncated => {
            format!(" [truncated — zoom {} for full body]", candidate.name)
        }
        BudgetedSymbolRenderStatus::Menu => {
            format!(" [member menu — zoom {} for bodies]", candidate.name)
        }
    };

    Ok(format!("{}\n{}\n{}", header, body, truncated_note,))
}

/// Truncate `s` to at most `max_len` bytes on a char boundary, appending `…`.
/// Byte-slicing `&s[..max_len]` panics when max_len lands mid-codepoint;
/// this walks `char_indices` to find the last valid boundary ≤ max_len.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = 0;
        for (i, c) in s.char_indices() {
            let char_end = i + c.len_utf8();
            if char_end > max_len {
                break;
            }
            end = char_end;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate_str("hello world this is long", 10), "hello worl…");
    }

    #[test]
    fn truncate_mid_codepoint_regression() {
        // "éx": é spans bytes 0-1, x at byte 2. max_len=1 lands inside é.
        // Old byte-slice code `&s[..1]` panics with "end byte index 1 is not
        // a char boundary; it is inside 'é'". The char_indices walk must not
        // panic and must truncate at the last valid boundary ≤ 1 (byte 0 → "").
        let result = truncate_str("éx", 1);
        assert_eq!(result, "…"); // empty content + ellipsis
    }

    #[test]
    fn truncate_mid_codepoint_in_longer_string() {
        // 79 'a' + "éxxx" = 83 bytes. max_len=80 lands in second byte of é
        // (bytes 79-80 = é). Old byte-slice panics.
        let s = format!("{}éxxx", "a".repeat(79));
        let result = truncate_str(&s, 80);
        assert!(result.ends_with('…'));
        assert!(result.starts_with("aaaa"));
        // Should be 79 'a' (all on char boundaries) + ellipsis
        assert_eq!(result, format!("{}…", "a".repeat(79)));
    }

    #[test]
    fn select_symbol_match_by_containment() {
        use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};

        let first = Symbol {
            name: "foo".into(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 10,
                start_col: 0,
                end_line: 15,
                end_col: 0,
            },
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        };
        let second = Symbol {
            name: "foo".into(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 50,
                start_col: 0,
                end_line: 55,
                end_col: 0,
            },
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        };
        let matches = vec![
            SymbolMatch {
                symbol: first,
                file: "a.rs".into(),
            },
            SymbolMatch {
                symbol: second,
                file: "a.rs".into(),
            },
        ];

        // start_line=52 (1-based) → 51 (0-based) falls inside second's range 50-55.
        let selected = select_symbol_match(&matches, 52);
        assert_eq!(selected.range.start_line, 50);
    }

    #[test]
    fn select_symbol_match_falls_back_to_first_when_no_line_hint() {
        use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};

        let first = Symbol {
            name: "bar".into(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 10,
                start_col: 0,
                end_line: 15,
                end_col: 0,
            },
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        };
        let second = Symbol {
            name: "bar".into(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 50,
                start_col: 0,
                end_line: 55,
                end_col: 0,
            },
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        };
        let matches = vec![
            SymbolMatch {
                symbol: first,
                file: "a.rs".into(),
            },
            SymbolMatch {
                symbol: second,
                file: "a.rs".into(),
            },
        ];

        // start_line=0 means "no line hint" → returns matches[0].
        let selected = select_symbol_match(&matches, 0);
        assert_eq!(selected.range.start_line, 10);
    }

    #[test]
    fn dedup_by_file_and_name() {
        let seeds = vec![PackCandidate {
            file: "a.rs".into(),
            name: "foo".into(),
            start_line: 1,
            provenance: "seed".into(),
            score: 1.0,
            seed_ordinal: Some(0),
            hop_distance: 0,
        }];
        let neighbors = vec![PackCandidate {
            file: "a.rs".into(),
            name: "foo".into(),
            start_line: 1,
            provenance: "caller-of-x".into(),
            score: 0.0,
            seed_ordinal: Some(0),
            hop_distance: 1,
        }];
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut ordered: Vec<&PackCandidate> = Vec::new();
        for seed in &seeds {
            let key = (seed.file.clone(), seed.name.clone());
            if seen.insert(key) {
                ordered.push(seed);
            }
        }
        for neighbor in &neighbors {
            let key = (neighbor.file.clone(), neighbor.name.clone());
            if seen.insert(key) {
                ordered.push(neighbor);
            }
        }
        // Only the seed should be included — neighbor is a duplicate.
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].provenance, "seed");
    }

    #[test]
    fn mode_validation_neither_mode() {
        let req = RawRequest {
            id: "1".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };
        assert!(req.params.get("question").is_none() && req.params.get("symbol").is_none());
    }

    #[test]
    fn mode_validation_both_modes() {
        let req = RawRequest {
            id: "1".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({
                "question": "how does foo work",
                "symbol": "bar",
                "filePath": "bar.rs",
            }),
        };
        let has_question = req
            .params
            .get("question")
            .and_then(|v| v.as_str())
            .is_some();
        let has_symbol = req.params.get("symbol").and_then(|v| v.as_str()).is_some()
            && req
                .params
                .get("filePath")
                .and_then(|v| v.as_str())
                .is_some();
        assert!(has_question && has_symbol);
    }

    #[test]
    fn budget_parsing_defaults_and_caps() {
        // Default budget
        let req = RawRequest {
            id: "1".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };
        let budget = req
            .params
            .get("budget")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_BUDGET as u64)
            .min(MAX_BUDGET as u64) as usize;
        assert_eq!(budget, DEFAULT_BUDGET);

        // Explicit budget
        let req2 = RawRequest {
            id: "2".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({"budget": 200}),
        };
        let budget2 = req2
            .params
            .get("budget")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_BUDGET as u64)
            .min(MAX_BUDGET as u64) as usize;
        assert_eq!(budget2, 200);

        // Budget capped at MAX_BUDGET
        let req3 = RawRequest {
            id: "3".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({"budget": 2000}),
        };
        let budget3 = req3
            .params
            .get("budget")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_BUDGET as u64)
            .min(MAX_BUDGET as u64) as usize;
        assert_eq!(budget3, MAX_BUDGET);
    }

    #[test]
    fn pack_candidate_ranking_order() {
        let mut seeds = vec![
            PackCandidate {
                file: "a.rs".into(),
                name: "high_score".into(),
                start_line: 10,
                provenance: "search score=0.900".into(),
                score: 0.9,
                seed_ordinal: None,
                hop_distance: 0,
            },
            PackCandidate {
                file: "b.rs".into(),
                name: "low_score".into(),
                start_line: 5,
                provenance: "search score=0.300".into(),
                score: 0.3,
                seed_ordinal: None,
                hop_distance: 0,
            },
        ];
        seeds.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        assert_eq!(seeds[0].name, "high_score");
        assert_eq!(seeds[1].name, "low_score");
    }

    #[test]
    fn render_symbol_section_header_format() {
        // Just verify the header format string works.
        let header = format!(
            "## {}:{} {} {}",
            "src/main.rs", 42, "function", "handle_request"
        );
        assert_eq!(header, "## src/main.rs:42 function handle_request");
    }

    #[test]
    fn stub_format_includes_provenance() {
        let stub = format!(
            "{}:{} {} — {}",
            "src/lib.rs", 15, "helper_fn", "callee-of-main"
        );
        assert_eq!(stub, "src/lib.rs:15 helper_fn — callee-of-main");
    }

    // ── regression tests for live-use fixes ──

    #[test]
    fn normalize_abs_path_strips_project_root() {
        // (c) absolute and relative forms of the same file normalize to one key.
        #[cfg(windows)]
        let (root, abs) = (
            Path::new(r"C:\Users\x\project"),
            r"C:\Users\x\project\skills\council\scripts\score.py",
        );
        #[cfg(not(windows))]
        let (root, abs) = (
            Path::new("/home/user/project"),
            "/home/user/project/skills/council/scripts/score.py",
        );
        let rel = "skills/council/scripts/score.py";
        let dot_prefix = "./skills/council/scripts/score.py";

        let from_abs = normalize_file_path(abs, root);
        let from_rel = normalize_file_path(rel, root);
        let from_dot = normalize_file_path(dot_prefix, root);

        assert_eq!(from_abs, "skills/council/scripts/score.py");
        assert_eq!(from_rel, "skills/council/scripts/score.py");
        assert_eq!(from_dot, "skills/council/scripts/score.py");
    }

    #[test]
    fn normalize_rel_path_preserved_for_non_root_abs() {
        let root = Path::new("/home/user/project");
        // An absolute path outside the project root stays absolute.
        let outside = "/other/repo/src/main.rs";
        assert_eq!(normalize_file_path(outside, root), outside);
    }

    #[test]
    fn dedupe_normalized_keys_merge_abs_and_rel() {
        // (c) two candidates with the same (file, name) after normalization
        // merge into one section.
        #[cfg(windows)]
        let (root, abs) = (
            Path::new(r"C:\Users\x\project"),
            r"C:\Users\x\project\skills\score.py",
        );
        #[cfg(not(windows))]
        let (root, abs) = (
            Path::new("/home/user/project"),
            "/home/user/project/skills/score.py",
        );
        let seeds = vec![PackCandidate {
            file: normalize_file_path(abs, root),
            name: "run_executor_only".into(),
            start_line: 243,
            provenance: "search score=0.900".into(),
            score: 0.9,
            seed_ordinal: Some(0),
            hop_distance: 0,
        }];
        let neighbors = vec![PackCandidate {
            file: normalize_file_path("skills/score.py", root),
            name: "run_executor_only".into(),
            start_line: 243,
            provenance: "caller-of-x".into(),
            score: 0.0,
            seed_ordinal: Some(0),
            hop_distance: 1,
        }];
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut ordered: Vec<&PackCandidate> = Vec::new();
        for seed in &seeds {
            let key = (seed.file.clone(), seed.name.clone());
            if seen.insert(key) {
                ordered.push(seed);
            }
        }
        for neighbor in &neighbors {
            let key = (neighbor.file.clone(), neighbor.name.clone());
            if seen.insert(key) {
                ordered.push(neighbor);
            }
        }
        // Only the seed should be included — neighbor is the same symbol.
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].provenance, "search score=0.900");
    }

    #[test]
    fn normalize_backslash_absolute_and_forward_slash_relative_to_same_key() {
        let root = Path::new("/home/user/project");
        let abs_backslash = r"\home\user\project\skills\council\scripts\score.py";
        let rel_forward_slash = "skills/council/scripts/score.py";

        assert_eq!(
            normalize_file_path(abs_backslash, root),
            normalize_file_path(rel_forward_slash, root)
        );
        assert_eq!(
            normalize_file_path(abs_backslash, root),
            "skills/council/scripts/score.py"
        );
    }

    #[test]
    fn resolve_containing_symbol_finds_inner_function() {
        // (a) resolve_containing_symbol must find the symbol whose range
        // contains the hit line — not just the first match.
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        // Write a temp file with known structure.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.py");
        std::fs::write(
            &file_path,
            "#!/usr/bin/env python3\n\ndef outer():\n    pass\n\ndef target_func(arg):\n    return arg\n\ndef bottom():\n    pass\n",
        )
        .unwrap();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        // Line 7 (1-based) = `    return arg` — contained by target_func
        // which starts at line 6 (1-based).
        let result = resolve_containing_symbol(&file_path, 7, &ctx);
        assert!(
            result.is_some(),
            "must resolve containing symbol for line 7"
        );
        let (name, start) = result.unwrap();
        assert_eq!(name, "target_func");
        assert_eq!(start, 6);

        // Line 2 (blank/comment) — no containing symbol.
        let no_sym = resolve_containing_symbol(&file_path, 2, &ctx);
        assert!(
            no_sym.is_none(),
            "blank line should have no containing symbol"
        );
    }

    #[test]
    fn suppression_scopes_callee_only_in_real_pack() {
        // SHOULD: drive the PRODUCTION build_pack path — candidates that hit
        // render errors must be scoped: callee-only suppression, seed+caller
        // stay visible in the Beyond-budget stub list.
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.py");
        std::fs::write(&file_path, "def foo():\n    pass\n").unwrap();
        let file_display = file_path.display().to_string();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        // Seeds: one real (renders), one bogus (unresolved seed → visible stub).
        let seeds = vec![
            PackCandidate {
                file: file_display.clone(),
                name: "foo".into(),
                start_line: 1,
                provenance: "search score=1.000".into(),
                score: 1.0,
                seed_ordinal: Some(0),
                hop_distance: 0,
            },
            PackCandidate {
                file: file_display.clone(),
                name: "ghost_seed".into(),
                start_line: 0,
                provenance: "search score=0.500".into(),
                score: 0.5,
                seed_ordinal: Some(1),
                hop_distance: 0,
            },
        ];

        // Neighbors: bogus names in the same file — all produce
        // "(symbol not resolved)".
        let neighbors = vec![
            PackCandidate {
                file: file_display.clone(),
                name: "bogus_callee".into(),
                start_line: 0,
                provenance: "callee-of-foo".into(),
                score: 0.0,
                seed_ordinal: Some(0),
                hop_distance: 1,
            },
            PackCandidate {
                file: file_display.clone(),
                name: "bogus_caller".into(),
                start_line: 0,
                provenance: "caller-of-foo".into(),
                score: 0.0,
                seed_ordinal: Some(0),
                hop_distance: 1,
            },
        ];

        let req = RawRequest {
            id: "test".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };

        let budget = 500;
        let resp = build_pack(
            &req,
            &ctx,
            &seeds,
            &neighbors,
            budget,
            "question",
            "test query",
            &[],
            false,
            false,
            dir.path(),
        );
        let text = resp.data.get("text").and_then(|v| v.as_str()).unwrap_or("");

        // Callee-neighbor must NOT appear as a visible stub — suppressed.
        assert!(
            !text.contains("bogus_callee — callee-of-foo (symbol not resolved)"),
            "unresolved callee must be suppressed, not visible stub"
        );

        // Caller-neighbor MUST appear as a visible stub.
        assert!(
            text.contains("bogus_caller — caller-of-foo (symbol not resolved)"),
            "unresolved caller must remain visible stub"
        );

        // Unresolved seed MUST appear as a visible stub.
        assert!(
            text.contains("ghost_seed — search score=0.500 (symbol not resolved)"),
            "unresolved seed must remain visible stub"
        );

        // Unresolved count line must mention callee omission.
        assert!(
            text.contains("(1 unresolved external calls omitted)"),
            "must report exactly one suppressed callee"
        );
    }

    #[test]
    fn no_containing_symbol_hits_produce_visible_stubs() {
        // MUST: grep-fallback hits with no containing symbol (comment,
        // import, blank line) must produce visible stubs — not silent drops.
        // The degenerate "all stubs" case must produce a pack, not
        // "no results found".
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("comments.py");
        // A file where line 2 is a comment (no containing symbol)
        // and line 4 is a blank line.
        std::fs::write(&file_path, "# top-level comment\n\n# another comment\n\n").unwrap();
        let file_display = file_path.display().to_string();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        let req = RawRequest {
            id: "test".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };

        // Simulate three grep-fallback hits: two comment lines, one nonexistent file.
        let pre_stubs: Vec<String> = vec![
            format!(
                "{}:2 — search score=0.800 (no containing symbol)",
                file_display
            ),
            format!(
                "{}:4 — search score=0.600 (no containing symbol)",
                file_display
            ),
            "nonexistent.rs:10 — search score=0.400 (no containing symbol)".into(),
        ];

        let resp = build_pack(
            &req,
            &ctx,
            &[],
            &[],
            400,
            "question",
            "test query",
            &pre_stubs,
            true,
            false,
            dir.path(),
        );

        let text = resp.data.get("text").and_then(|v| v.as_str()).unwrap_or("");

        // Must have a pack header (even though no seeds).
        assert!(
            text.contains("## gather pack"),
            "empty-seed pack must still have header"
        );

        // All three stubs must be visible in the Beyond budget section.
        assert!(
            text.contains("(no containing symbol)"),
            "no-containing-symbol stubs must be visible"
        );
        assert!(
            text.contains("nonexistent.rs:10"),
            "nonexistent file stub must be visible"
        );

        // Must NOT say "no results found" — that was the old silent-drop.
        assert!(
            !text.contains("no results found"),
            "must not silently drop no-containing-symbol hits"
        );
    }

    #[test]
    fn fully_degraded_pack_flags_semantic_index_building() {
        // (a) When semantic_status != "ready" and all hits are no-symbol
        // stubs, the header carries a degradation notice.
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("blank.py");
        std::fs::write(&file_path, "# comment\n").unwrap();
        let file_display = file_path.display().to_string();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        let req = RawRequest {
            id: "test".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };

        let pre_stubs = vec![format!(
            "{}:1 — search score=0.250 (no containing symbol)",
            file_display
        )];

        // degraded=true → header should carry the flag.
        let resp = build_pack(
            &req,
            &ctx,
            &[],
            &[],
            400,
            "question",
            "test query",
            &pre_stubs,
            true,
            false,
            dir.path(),
        );
        let text = resp.data.get("text").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            text.contains("degraded=semantic-index-building"),
            "degraded pack must carry semantic-index-building flag in header"
        );
    }

    #[test]
    fn normal_pack_has_no_degradation_flag() {
        // (b) Normal results (seeds resolved) must NOT carry the degradation flag.
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("normal.py");
        std::fs::write(&file_path, "def foo():\n    pass\n").unwrap();
        let file_display = file_path.display().to_string();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        let seeds = vec![PackCandidate {
            file: file_display,
            name: "foo".into(),
            start_line: 1,
            provenance: "search score=1.000".into(),
            score: 1.0,
            seed_ordinal: Some(0),
            hop_distance: 0,
        }];

        let req = RawRequest {
            id: "test".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };

        // degraded=false — normal case.
        let resp = build_pack(
            &req,
            &ctx,
            &seeds,
            &[],
            400,
            "question",
            "test query",
            &[],
            false,
            false,
            dir.path(),
        );
        let text = resp.data.get("text").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !text.contains("degraded=semantic-index-building"),
            "normal pack must not carry degradation flag"
        );
        assert!(
            text.contains("def foo"),
            "normal pack must contain seed body"
        );
    }

    #[test]
    fn mixed_results_with_one_real_seed_no_degradation_flag() {
        // (c) Even when degraded=true, if at least one symbol-level seed
        // resolved, the pack has usable content — no flag.
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("mixed.py");
        std::fs::write(&file_path, "def foo():\n    pass\n").unwrap();
        let file_display = file_path.display().to_string();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        // One real seed (resolves) + some no-symbol stubs.
        let seeds = vec![PackCandidate {
            file: file_display.clone(),
            name: "foo".into(),
            start_line: 1,
            provenance: "search score=1.000".into(),
            score: 1.0,
            seed_ordinal: Some(0),
            hop_distance: 0,
        }];

        let pre_stubs = vec![format!(
            "{}:1 — search score=0.250 (no containing symbol)",
            file_display
        )];

        let req = RawRequest {
            id: "test".into(),
            command: "gather".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({}),
        };

        // Production path: handle_gather_question sets degraded=false when
        // seeds is non-empty (even one resolved seed → not degraded).
        let resp = build_pack(
            &req,
            &ctx,
            &seeds,
            &[],
            400,
            "question",
            "test query",
            &pre_stubs,
            false,
            false,
            dir.path(),
        );
        let text = resp.data.get("text").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !text.contains("degraded=semantic-index-building"),
            "mixed pack with one real seed must not carry degradation flag"
        );
        assert!(
            text.contains("def foo"),
            "mixed pack must contain seed body"
        );
    }

    #[test]
    fn render_resolves_repo_relative_path_against_project_root() {
        // (f) render_symbol_section must resolve repo-relative candidate.file
        // against project_root, not process CWD. A unique temp dir ensures
        // the relative path cannot exist relative to CWD — only project_root
        // join produces a valid path.
        use crate::config::Config;
        use crate::parser::TreeSitterProvider;

        let project_root = tempfile::tempdir().unwrap();
        let file_name = "test_cwd.py";
        let file_path = project_root.path().join(file_name);
        std::fs::write(&file_path, "def answer():\n    return 42\n").unwrap();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        let candidate = PackCandidate {
            file: file_name.to_string(),
            name: "answer".into(),
            start_line: 1,
            provenance: "test".into(),
            score: 1.0,
            seed_ordinal: Some(0),
            hop_distance: 0,
        };

        let result = render_symbol_section(&ctx, &candidate, 50, project_root.path());
        assert!(
            result.is_ok(),
            "repo-relative path must resolve via project_root; got: {:?}",
            result
        );
        let section = result.unwrap();
        assert!(
            section.contains("def answer"),
            "rendered section must contain symbol body; got: {}",
            section
        );
    }
}
