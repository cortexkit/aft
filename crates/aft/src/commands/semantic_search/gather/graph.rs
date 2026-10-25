use super::{Direction, GatherNode, GraphIdentity, NeighborKey, NeighborLocation};
use crate::callgraph_store::CallGraphStore;
use crate::commands::callgraph_store_adapter::{call_tree_result, callers_result};
use crate::commands::symbol_render::{render_symbol_within_budget, symbol_kind_string};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::grep_executor::truncate_at_char_boundary;
use crate::parser::{detect_language, LangId};
use crate::symbols::{Symbol, SymbolKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(in crate::commands::semantic_search) const DEFAULT_BODY_LINE_BUDGET: usize = 80;
pub(in crate::commands::semantic_search) const DEFAULT_BODY_BYTE_BUDGET: usize = 4096;
const DEFAULT_FRONTIER_LIMIT: usize = 40;
const CACHE_ENTRY_LIMIT: usize = 512;

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

fn containing_symbol(file: &Path, line: u32, ctx: &AppContext) -> Option<Symbol> {
    let line = line.checked_sub(1)?;
    ctx.provider()
        .list_symbols(file)
        .ok()?
        .into_iter()
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

pub(in crate::commands::semantic_search) fn resolve_containing_symbol(
    file: &Path,
    line: u32,
    ctx: &AppContext,
) -> Option<(String, u32)> {
    let symbol = containing_symbol(file, line, ctx)?;
    let source = crate::parser::read_source(file).ok()?;
    let tree =
        crate::parser::parse_source_with_cached_parser(file, &source, detect_language(file)?)
            .ok()?;
    let declaration = declaration_node(&tree, &source, &symbol);
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

pub(in crate::commands::semantic_search) fn resolve_seeds(
    ctx: &AppContext,
    root: &Path,
    hits: &[(PathBuf, u32)],
    include_tests: bool,
) -> Vec<GatherNode> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
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
            let mut node = render_node(
                ctx,
                &root,
                &path,
                line,
                DEFAULT_BODY_LINE_BUDGET,
                include_tests,
            )?;
            node.seed_ordinal = ordinal;
            node.edge = "seed".into();
            seen.insert(node.id.clone()).then_some(node)
        })
        .collect()
}

pub(in crate::commands::semantic_search) fn render_node(
    ctx: &AppContext,
    root: &Path,
    path: &Path,
    line: u32,
    line_budget: usize,
    include_tests: bool,
) -> Option<GatherNode> {
    let path = code_path(root, path)?;
    let symbol = containing_symbol(&path, line, ctx)?;
    let source = crate::parser::read_source(&path).ok()?;
    let lang = detect_language(&path)?;
    let tree = crate::parser::parse_source_with_cached_parser(&path, &source, lang).ok()?;
    let declaration = declaration_node(&tree, &source, &symbol);
    if !include_tests
        && (crate::inspect::job::is_test_file(&path.strip_prefix(root).ok()?.to_string_lossy())
            || lang == LangId::Rust
                && declaration.is_some_and(|node| rust_test_only(&source, node)))
    {
        return None;
    }
    let lines: Vec<String> = source.lines().map(str::to_owned).collect();
    let start = declaration.map_or(symbol.range.start_line as usize, |node| {
        node.start_position().row
    });
    let mut docs: Vec<String> = lines[..start.min(lines.len())]
        .iter()
        .rev()
        .take_while(|line| {
            let line = line.trim();
            line.starts_with("///")
                || line.starts_with("//!")
                || line.starts_with("/**")
                || line.starts_with('*')
        })
        .take(3)
        .map(|line| truncate_at_char_boundary(line.trim(), 512).to_owned())
        .collect();
    docs.reverse();
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
    seed: &GatherNode,
    direction: Direction,
    include_tests: bool,
) -> Result<Vec<NeighborLocation>, String> {
    let candidates = snapshot
        .nodes_for(&seed.path, &seed.name)
        .map_err(|_| "graph_query_failed")?;
    let Some(target) = candidates
        .iter()
        .filter(|node| node.line <= seed.line && seed.line <= node.end_line)
        .min_by_key(|node| node.end_line.saturating_sub(node.line))
    else {
        return Ok(Vec::new());
    };
    let locations = match direction {
        Direction::Caller => callers_result(snapshot, &seed.path, &target.symbol, 1, include_tests)
            .map_err(|_| "graph_query_failed")?
            .callers
            .into_iter()
            .flat_map(|group| {
                group
                    .callers
                    .into_iter()
                    .map(move |caller| NeighborLocation {
                        path: PathBuf::from(&group.file),
                        line: caller.line,
                    })
            })
            .take(DEFAULT_FRONTIER_LIMIT)
            .collect(),
        Direction::Callee => {
            call_tree_result(snapshot, &seed.path, &target.symbol, 1, include_tests)
                .map_err(|_| "graph_query_failed")?
                .children
                .into_iter()
                .filter(|node| node.resolved)
                .take(DEFAULT_FRONTIER_LIMIT)
                .map(|node| NeighborLocation {
                    path: PathBuf::from(node.file),
                    line: node.line,
                })
                .collect()
        }
    };
    Ok(locations)
}

pub(in crate::commands::semantic_search) fn collect_callgraph_neighbors(
    ctx: &AppContext,
    snapshot: &CallGraphStore,
    seeds: &[GatherNode],
    include_tests: bool,
) -> Result<Vec<GatherNode>, String> {
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
    let mut result = Vec::new();
    for seed in seeds {
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
                    let locations = query_neighbors(snapshot, seed, direction, include_tests)?;
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
            for location in locations {
                let Some(mut node) = render_node(
                    ctx,
                    snapshot.project_root(),
                    &location.path,
                    location.line,
                    DEFAULT_BODY_LINE_BUDGET,
                    include_tests,
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
                result.push(node);
                if result.len() >= DEFAULT_FRONTIER_LIMIT {
                    return Ok(result);
                }
            }
        }
    }
    Ok(result)
}
