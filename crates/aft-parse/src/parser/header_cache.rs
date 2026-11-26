use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use tree_sitter::Tree;

use super::{
    cache_freshness, is_h_header, parse_source_with_parser, parse_source_with_parser_deadline,
    AftError, LangId, HEADER_FALLBACK_PARSE_DEADLINE,
};

#[cfg(any(test, feature = "test-support"))]
use std::cell::RefCell;

const MAX_ENTRIES: usize = 128;
const TREE_BUDGET: usize = 16 * 1024 * 1024;

struct HeaderParse {
    lang: LangId,
    tree: Option<Tree>,
    bytes: usize,
}

#[derive(Default)]
struct HeaderParseCache {
    entries: HashMap<blake3::Hash, HeaderParse>,
    order: VecDeque<blake3::Hash>,
    bytes: usize,
}

impl HeaderParseCache {
    fn insert(&mut self, hash: blake3::Hash, tree: &Tree, lang: LangId, source_bytes: usize) {
        // Tree-sitter does not expose allocated bytes. Charge both input bytes
        // and a conservative 128 bytes per syntax node, visible or anonymous.
        let estimate =
            source_bytes.saturating_add(tree.root_node().descendant_count().saturating_mul(128));
        let bytes = if estimate <= TREE_BUDGET { estimate } else { 0 };
        while self.entries.len() >= MAX_ENTRIES || self.bytes + bytes > TREE_BUDGET {
            let oldest = self
                .order
                .pop_front()
                .expect("header cache order matches entries");
            self.bytes -= self
                .entries
                .remove(&oldest)
                .expect("queued header is cached")
                .bytes;
        }
        self.bytes += bytes;
        self.order.push_back(hash);
        self.entries.insert(
            hash,
            HeaderParse {
                lang,
                tree: (bytes != 0).then(|| tree.clone()),
                bytes,
            },
        );
    }
}

// FileParser instances are request-local. Share header trees by content so a
// language-only consumer and a later symbol/graph consumer use the same verdict
// without repeating the fallback parses. Eviction affects reuse, not results.
static HEADER_PARSE_CACHE: LazyLock<Mutex<HeaderParseCache>> =
    LazyLock::new(|| Mutex::new(HeaderParseCache::default()));

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static HEADER_PARSE_ON_MISS: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

/// Reuse a resolved header tree in path-free extraction without changing the
/// caller's already-resolved language.
pub fn cached_header_tree(source: &str, lang: LangId) -> Option<Tree> {
    if !matches!(lang, LangId::C | LangId::Cpp | LangId::ObjC) {
        return None;
    }
    HEADER_PARSE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .get(&cache_freshness::hash_bytes(source.as_bytes()))
        .filter(|entry| entry.lang == lang)
        .and_then(|entry| entry.tree.clone())
}

pub fn parse_resolved_source(
    path: &Path,
    source: &str,
    lang: LangId,
) -> Result<(Tree, LangId), AftError> {
    if !is_h_header(path) {
        return parse_source_with_parser(path, source, lang).map(|tree| (tree, lang));
    }
    let hash = cache_freshness::hash_bytes(source.as_bytes());
    let cached = HEADER_PARSE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .get(&hash)
        .map(|entry| (entry.tree.clone(), entry.lang));
    if let Some((tree, lang)) = cached {
        // A large header can retain its language verdict without its tree.
        return match tree {
            Some(tree) => Ok((tree, lang)),
            None => parse_source_with_parser(path, source, lang).map(|tree| (tree, lang)),
        };
    }

    #[cfg(any(test, feature = "test-support"))]
    HEADER_PARSE_ON_MISS.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });

    let c_tree = parse_source_with_parser(path, source, LangId::C)?;
    let mut resolved = (c_tree.clone(), LangId::C);
    if c_tree.root_node().has_error() {
        for candidate in [LangId::Cpp, LangId::ObjC] {
            let Some(tree) = parse_source_with_parser_deadline(
                path,
                source,
                candidate,
                Some(HEADER_FALLBACK_PARSE_DEADLINE),
            )?
            else {
                continue;
            };
            // has_error includes both ERROR nodes and inserted MISSING nodes.
            if !tree.root_node().has_error() {
                resolved = (tree, candidate);
                break;
            }
        }
    }
    // Parsing must not serialize unrelated headers behind this cache lock.
    // Racing misses may both parse; return the first inserted verdict to both
    // callers, including when one of their fallback attempts was cancelled.
    // A fallback close to the deadline can finish in only one racing thread.
    // Returning the stored verdict keeps both results consistent.
    let mut cache = HEADER_PARSE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parsed) = cache.entries.get(&hash) {
        if let Some(tree) = &parsed.tree {
            return Ok((tree.clone(), parsed.lang));
        }
        let lang = parsed.lang;
        drop(cache);
        return if resolved.1 == lang {
            Ok(resolved)
        } else {
            parse_source_with_parser(path, source, lang).map(|tree| (tree, lang))
        };
    }
    cache.insert(hash, &resolved.0, resolved.1, source.len());
    Ok(resolved)
}
