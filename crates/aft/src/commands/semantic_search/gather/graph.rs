use super::{
    Direction, GatherGap, GatherNode, GraphIdentity, NeighborKey, NeighborLocation,
    DEFAULT_BODY_BYTE_BUDGET, DEFAULT_BODY_LINE_BUDGET, MAX_FRONTIER,
};
use crate::callgraph_store::{CallGraphStore, StoreNode};
use crate::commands::symbol_render::{render_symbol_within_budget, symbol_kind_string};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::grep_executor::truncate_at_char_boundary;
use crate::parser::{detect_language, LangId};
use crate::symbols::{Symbol, SymbolKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

const CACHE_ENTRY_LIMIT: usize = 512;

struct GatherFile {
    source: String,
    tree: tree_sitter::Tree,
    symbols: Vec<Symbol>,
    lines: Vec<String>,
    lang: LangId,
}

#[derive(Default)]
pub(in crate::commands::semantic_search) struct GatherSourceCache {
    files: HashMap<PathBuf, Option<GatherFile>>,
    access: Option<(String, String)>,
    #[cfg(test)]
    pub(super) loads: usize,
}

impl GatherSourceCache {
    pub(super) fn for_request(req: Option<&crate::protocol::RawRequest>) -> Self {
        Self {
            access: req.map(|req| (req.id.clone(), req.session().to_owned())),
            ..Self::default()
        }
    }

    fn allowed(&self, ctx: &AppContext, path: &Path) -> bool {
        let (id, session) = self
            .access
            .as_ref()
            .map_or(("", ""), |(id, session)| (id.as_str(), session.as_str()));
        ctx.validate_search_path(
            id,
            session,
            path,
            crate::context::WorkerSearchTool::AftSearch,
        )
        .is_ok()
    }

    fn load(&mut self, path: &Path) -> Option<&GatherFile> {
        if !self.files.contains_key(path) {
            #[cfg(test)]
            {
                self.loads += 1;
            }
            let parsed = (|| {
                let lang = detect_language(path)?;
                let source = crate::parser::read_source(path).ok()?;
                let tree =
                    crate::parser::parse_source_with_cached_parser(path, &source, lang).ok()?;
                let symbols =
                    crate::parser::extract_symbols_from_tree(&source, &tree, lang).ok()?;
                let lines = source.lines().map(str::to_owned).collect();
                Some(GatherFile {
                    source,
                    tree,
                    symbols,
                    lines,
                    lang,
                })
            })();
            self.files.insert(path.to_owned(), parsed);
        }
        self.files.get(path)?.as_ref()
    }
}

#[derive(Default)]
pub(in crate::commands::semantic_search) struct GatherGraph {
    pub nodes: Vec<GatherNode>,
    pub gaps: Vec<GatherGap>,
}

pub(in crate::commands::semantic_search) fn with_graph_snapshot<T>(
    ctx: &AppContext,
    root: &Path,
    read: impl FnOnce(&CallGraphStore) -> Result<T, String>,
) -> Result<T, String> {
    let store = match ctx.callgraph_store_for_query_without_wait() {
        CallgraphStoreAccess::Ready(store) => store,
        CallgraphStoreAccess::Building => return Err("building".into()),
        CallgraphStoreAccess::Suspended(_) => return Err("suspended".into()),
        CallgraphStoreAccess::Off => return Err("off".into()),
        CallgraphStoreAccess::Unavailable => return Err("unavailable".into()),
        CallgraphStoreAccess::Error(_) => return Err("error".into()),
    };
    let root = root.canonicalize().map_err(|_| "root_unavailable")?;
    if store.project_root() != root {
        return Err("root_mismatch".into());
    }
    let snapshot = store.read_snapshot().map_err(|_| "snapshot_unavailable")?;
    // The first read fixes the deferred transaction before any edge query.
    let _generation = snapshot.projection_generation().map(str::to_owned);
    let _revision = snapshot
        .projection_write_revision()
        .map_err(|_| "snapshot_unavailable")?;
    let result = read(&snapshot);
    drop(snapshot);
    drop(store);
    result
}

fn containing_symbol(file: &GatherFile, line: u32) -> Option<&Symbol> {
    let line = line.checked_sub(1)?;
    file.symbols
        .iter()
        .filter(|symbol| {
            !matches!(symbol.kind, SymbolKind::Heading | SymbolKind::FileSummary)
                && symbol.range.start_line <= line
                && line <= symbol.range.end_line
        })
        .min_by_key(|symbol| {
            (
                symbol
                    .range
                    .end_line
                    .saturating_sub(symbol.range.start_line),
                symbol.range.end_col.saturating_sub(symbol.range.start_col),
            )
        })
}

#[cfg(test)]
pub(in crate::commands::semantic_search) fn resolve_containing_symbol(
    file: &Path,
    line: u32,
    ctx: &AppContext,
) -> Option<(String, u32)> {
    let mut sources = GatherSourceCache::default();
    if !sources.allowed(ctx, file) {
        return None;
    }
    let file = sources.load(file)?;
    let symbol = containing_symbol(file, line)?;
    let declaration = declaration_node(&file.tree, &file.source, symbol);
    Some((
        symbol.name.clone(),
        declaration
            .map_or(symbol.range.start_line, |node| {
                node.start_position().row as u32
            })
            .saturating_add(1),
    ))
}

fn declaration_node<'a>(
    tree: &'a tree_sitter::Tree,
    source: &str,
    symbol: &Symbol,
) -> Option<tree_sitter::Node<'a>> {
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.end_position().row < symbol.range.start_line as usize
            || node.start_position().row > symbol.range.end_line as usize
        {
            continue;
        }
        if node.end_position().row == symbol.range.end_line as usize
            && node
                .child_by_field_name("name")
                .is_some_and(|name| source.get(name.byte_range()) == Some(symbol.name.as_str()))
        {
            return Some(node);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    None
}

fn cfg_requires_test(predicate: &str) -> bool {
    if predicate == "test" {
        return true;
    }
    let Some((operator, rest)) = predicate.split_once('(') else {
        return false;
    };
    let Some(inner) = rest.strip_suffix(')') else {
        return false;
    };
    let mut depth = 0usize;
    let mut start = 0;
    let mut parts = Vec::new();
    for (index, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < inner.len() {
        parts.push(&inner[start..]);
    }
    match operator {
        "all" => parts.into_iter().any(cfg_requires_test),
        "any" => !parts.is_empty() && parts.into_iter().all(cfg_requires_test),
        _ => false,
    }
}

fn rust_test_only(source: &str, mut node: tree_sitter::Node<'_>) -> bool {
    // The store adapters filter file names, not cfg-gated items in product files.
    loop {
        let mut previous = node.prev_sibling();
        while let Some(attribute) = previous {
            match attribute.kind() {
                "attribute_item" => {
                    let compact: String = source[attribute.byte_range()]
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .collect();
                    if compact == "#[test]"
                        || compact
                            .strip_prefix("#[cfg(")
                            .and_then(|inner| inner.strip_suffix(")]"))
                            .is_some_and(cfg_requires_test)
                    {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            previous = attribute.prev_sibling();
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

fn code_path(root: &Path, file: &Path) -> Option<PathBuf> {
    let file = if file.is_absolute() {
        file.to_owned()
    } else {
        root.join(file)
    };
    let file = file.canonicalize().ok()?;
    if !file.starts_with(root) {
        return None;
    }
    match detect_language(&file)? {
        LangId::Markdown | LangId::Json | LangId::Yaml | LangId::Toml => None,
        _ => Some(file),
    }
}

#[cfg(test)]
pub(in crate::commands::semantic_search) fn resolve_seeds(
    ctx: &AppContext,
    root: &Path,
    hits: &[(PathBuf, u32)],
    include_tests: bool,
) -> Vec<GatherNode> {
    resolve_seeds_for_request(ctx, root, hits, include_tests, None)
}

pub(super) fn resolve_seeds_for_request(
    ctx: &AppContext,
    root: &Path,
    hits: &[(PathBuf, u32)],
    include_tests: bool,
    request: Option<&crate::protocol::RawRequest>,
) -> Vec<GatherNode> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut sources = GatherSourceCache::for_request(request);
    hits.iter()
        .enumerate()
        .filter_map(|(ordinal, (path, line))| {
            code_path(&root, path).map(|path| (ordinal, path, *line))
        })
        .take(5)
        .filter(|(_, path, _)| {
            include_tests
                || !crate::inspect::job::is_test_file(
                    &path.strip_prefix(&root).unwrap_or(path).to_string_lossy(),
                )
        })
        .filter_map(|(ordinal, path, line)| {
            let mut node = render_cached(
                ctx,
                &root,
                &path,
                line,
                DEFAULT_BODY_LINE_BUDGET,
                include_tests,
                &mut sources,
            )?;
            node.seed_ordinal = ordinal;
            node.edge = "seed".into();
            seen.insert(node.id.clone()).then_some(node)
        })
        .collect()
}

#[cfg(test)]
pub(in crate::commands::semantic_search) fn render_node(
    ctx: &AppContext,
    root: &Path,
    path: &Path,
    line: u32,
    line_budget: usize,
    include_tests: bool,
) -> Option<GatherNode> {
    render_cached(
        ctx,
        root,
        path,
        line,
        line_budget,
        include_tests,
        &mut GatherSourceCache::default(),
    )
}

fn render_cached(
    ctx: &AppContext,
    root: &Path,
    path: &Path,
    line: u32,
    line_budget: usize,
    include_tests: bool,
    sources: &mut GatherSourceCache,
) -> Option<GatherNode> {
    let path = code_path(root, path)?;
    if !sources.allowed(ctx, &path) {
        return None;
    }
    let file = sources.load(&path)?;
    let symbol = containing_symbol(file, line)?;
    let source = &file.source;
    let lang = file.lang;
    let declaration = declaration_node(&file.tree, source, symbol);
    if !include_tests
        && (crate::inspect::job::is_test_file(&path.strip_prefix(root).ok()?.to_string_lossy())
            || lang == LangId::Rust
                && declaration.is_some_and(|node| rust_test_only(&source, node)))
    {
        return None;
    }
    let lines = &file.lines;
    let start = declaration.map_or(symbol.range.start_line as usize, |node| {
        node.start_position().row
    });
    let docs = lines[symbol.range.start_line as usize
        ..start.max(symbol.range.start_line as usize).min(lines.len())]
        .iter()
        .filter_map(|line| {
            let line = line.trim();
            if line == "*/" {
                return None;
            }
            let text = line
                .strip_prefix("///")
                .or_else(|| line.strip_prefix("//!"))
                .or_else(|| line.strip_prefix("/**"))
                .or_else(|| line.strip_prefix('*'))?;
            let text = text.strip_suffix("*/").unwrap_or(text).trim();
            (!text.is_empty()).then(|| truncate_at_char_boundary(text, 512).to_owned())
        })
        .take(3)
        .collect();
    let render = render_symbol_within_budget(
        &symbol,
        &lines,
        detect_language(&path),
        None,
        line_budget,
        false,
    );
    let relative = path.strip_prefix(root).ok()?.to_owned();
    let line = (start as u32).saturating_add(1);
    Some(GatherNode {
        id: format!(
            "{}:{line}:{}",
            relative.to_string_lossy().replace('\\', "/"),
            symbol.name
        ),
        path: relative,
        line,
        name: symbol.name.clone(),
        kind: symbol_kind_string(&symbol.kind),
        signature: truncate_at_char_boundary(
            symbol.signature.as_deref().unwrap_or(&symbol.name),
            1024,
        )
        .to_owned(),
        docs,
        body: truncate_at_char_boundary(&render.content, DEFAULT_BODY_BYTE_BUDGET).to_owned(),
        edge: String::new(),
        seed_ordinal: 0,
        hop_distance: 0,
        rerank_score: None,
    })
}

fn graph_identity(snapshot: &CallGraphStore) -> Result<Option<GraphIdentity>, String> {
    let generation = snapshot.projection_generation().map(str::to_owned);
    let revision = snapshot
        .projection_write_revision()
        .map_err(|_| "snapshot_unavailable")?;
    Ok(generation
        .zip(revision)
        .map(|(generation, revision)| GraphIdentity {
            root: snapshot.project_root().to_owned(),
            generation,
            revision,
        }))
}

fn query_neighbors(
    snapshot: &CallGraphStore,
    target: &StoreNode,
    direction: Direction,
    include_tests: bool,
) -> Result<Vec<NeighborLocation>, String> {
    let mut locations: Vec<_> = match direction {
        Direction::Caller => {
            let mut locations: Vec<_> = snapshot
                .direct_callers_of(Path::new(&target.file), &target.symbol)
                .map_err(|_| "graph_query_failed")?
                .into_iter()
                .filter(|site| {
                    include_tests || !crate::inspect::job::is_test_file(&site.caller.file)
                })
                .map(|site| NeighborLocation {
                    path: PathBuf::from(site.caller.file),
                    line: site.line,
                })
                .collect();
            let templates = if detect_language(Path::new(&target.file)) == Some(LangId::Rust) {
                snapshot
                    .ref_sites_named(
                        crate::callgraph_store::MACRO_BODY_CALL_REF_KIND,
                        &target.name,
                        MAX_FRONTIER,
                    )
                    .map_err(|_| "graph_query_failed")?
            } else {
                Vec::new()
            };
            // Macro templates have no direct edges. Their calls are ambiguous when target names repeat.
            if !templates.is_empty()
                && !snapshot
                    .nodes_matching(&target.name)
                    .map_err(|_| "graph_query_failed")?
                    .iter()
                    .any(|node| {
                        node.name == target.name
                            && (&node.file, &node.symbol, node.line, node.end_line)
                                != (&target.file, &target.symbol, target.line, target.end_line)
                    })
            {
                let mut macros = Vec::new();
                for site in templates {
                    let Some(name) = site.local_name else {
                        continue;
                    };
                    if locations.len() < MAX_FRONTIER
                        && (include_tests || !crate::inspect::job::is_test_file(&site.file))
                    {
                        locations.push(NeighborLocation {
                            path: PathBuf::from(site.file),
                            line: site.line,
                        });
                    }
                    macros.push(name);
                }
                macros.sort();
                macros.dedup();
                for name in macros {
                    let remaining = MAX_FRONTIER.saturating_sub(locations.len());
                    if remaining == 0 {
                        break;
                    }
                    for site in snapshot
                        .ref_sites_named("call", &name, remaining)
                        .map_err(|_| "graph_query_failed")?
                    {
                        if include_tests || !crate::inspect::job::is_test_file(&site.file) {
                            locations.push(NeighborLocation {
                                path: PathBuf::from(site.file),
                                line: site.line,
                            });
                        }
                    }
                }
            }
            locations
        }
        Direction::Callee => {
            let mut locations = Vec::new();
            for site in snapshot
                .outgoing_calls_of(target)
                .map_err(|_| "graph_query_failed")?
            {
                let node = match site.target {
                    Some(node) => Some(node),
                    None => snapshot
                        .nodes_for_stored(&site.target_file, &site.target_symbol)
                        .map_err(|_| "graph_query_failed")?
                        .into_iter()
                        .filter(|node| node.symbol == site.target_symbol)
                        .min_by_key(|node| node.line),
                };
                if let Some(node) = node {
                    if include_tests || !crate::inspect::job::is_test_file(&node.file) {
                        locations.push(NeighborLocation {
                            path: PathBuf::from(node.file),
                            line: node.line,
                        });
                    }
                }
            }
            locations
        }
    };
    locations.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    locations.dedup_by(|left, right| left.path == right.path && left.line == right.line);
    locations.truncate(MAX_FRONTIER);
    Ok(locations)
}

pub(in crate::commands::semantic_search) fn collect_callgraph_neighbors(
    ctx: &AppContext,
    snapshot: &CallGraphStore,
    seeds: &[GatherNode],
    include_tests: bool,
    sources: &mut GatherSourceCache,
) -> Result<GatherGraph, String> {
    let identity = graph_identity(snapshot)?;
    let runtime = ctx.gather_runtime();
    {
        let mut cache = runtime
            .neighbors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.identity != identity || identity.is_none() {
            cache.entries.clear();
            cache.identity = identity.clone();
        }
    }
    let mut seen: HashSet<_> = seeds.iter().map(|seed| seed.id.clone()).collect();
    let mut result = GatherGraph::default();
    let mut queues = Vec::new();
    for seed in seeds {
        let candidates = match snapshot.nodes_for(&seed.path, &seed.name) {
            Ok(nodes) => nodes,
            Err(_) => {
                result.gaps.push(GatherGap {
                    seed_id: seed.id.clone(),
                    state: "graph_query_failed",
                });
                queues.push(VecDeque::new());
                continue;
            }
        };
        let target = candidates
            .into_iter()
            // Store ranges select a name, not the full source declaration.
            .find(|candidate| {
                render_cached(ctx, snapshot.project_root(), Path::new(&candidate.file), candidate.line,
                    DEFAULT_BODY_LINE_BUDGET, include_tests, sources)
                    .is_some_and(|node| node.id == seed.id)
            });
        let Some(target) = target else {
            result.gaps.push(GatherGap {
                seed_id: seed.id.clone(),
                state: "seed_unresolved",
            });
            queues.push(VecDeque::new());
            continue;
        };
        let mut queue = VecDeque::new();
        for direction in [Direction::Caller, Direction::Callee] {
            let key = identity.clone().map(|identity| NeighborKey {
                identity,
                node_id: seed.id.clone(),
                direction,
                include_tests,
            });
            let cached = key.as_ref().and_then(|key| {
                runtime
                    .neighbors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entries
                    .get(key)
                    .cloned()
            });
            let locations = match cached {
                Some(locations) => locations,
                None => {
                    // Graph queries must not hold the cache lock.
                    let locations =
                        match query_neighbors(snapshot, &target, direction, include_tests) {
                            Ok(locations) => locations,
                            Err(_) => {
                                result.gaps.push(GatherGap {
                                    seed_id: seed.id.clone(),
                                    state: "graph_query_failed",
                                });
                                continue;
                            }
                        };
                    if let Some(key) = key {
                        let mut cache = runtime
                            .neighbors
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if cache.identity.as_ref() == Some(&key.identity) {
                            if cache.entries.len() >= CACHE_ENTRY_LIMIT {
                                cache.entries.clear();
                            }
                            cache.entries.insert(key, locations.clone());
                        }
                    }
                    locations
                }
            };
            queue.extend(locations.into_iter().map(|location| (direction, location)));
        }
        queues.push(queue);
    }
    loop {
        let mut progressed = false;
        for (seed, queue) in seeds.iter().zip(&mut queues) {
            while let Some((direction, location)) = queue.pop_front() {
                let path = snapshot.project_root().join(&location.path);
                if !sources.allowed(ctx, &path) {
                    result.gaps.push(GatherGap {
                        seed_id: seed.id.clone(),
                        state: "path_denied",
                    });
                    continue;
                }
                let Some(mut node) = render_cached(
                    ctx,
                    snapshot.project_root(),
                    &location.path,
                    location.line,
                    DEFAULT_BODY_LINE_BUDGET,
                    include_tests,
                    sources,
                ) else {
                    continue;
                };
                if !seen.insert(node.id.clone()) {
                    continue;
                }
                node.seed_ordinal = seed.seed_ordinal;
                node.hop_distance = seed.hop_distance.saturating_add(1);
                node.edge = format!(
                    "{}-of-{}",
                    match direction {
                        Direction::Caller => "caller",
                        Direction::Callee => "callee",
                    },
                    seed.id
                );
                result.nodes.push(node);
                progressed = true;
                if result.nodes.len() >= MAX_FRONTIER {
                    return Ok(result);
                }
                break;
            }
        }
        if !progressed {
            break;
        }
    }
    Ok(result)
}
