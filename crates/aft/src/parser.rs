pub use aft_parse::parser::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::cache_freshness::{self};
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph::resolve_module_path;
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph_store::join::syntax_index::{Direct, Nav};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::callgraph_store::join::Parser;
#[cfg(test)]
#[allow(unused_imports)]
use crate::error::AftError;
#[cfg(test)]
#[allow(unused_imports)]
use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};
#[cfg(test)]
#[allow(unused_imports)]
use std::cell::RefCell;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, LazyLock, Mutex, RwLock};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::SystemTime;
#[cfg(test)]
#[allow(unused_imports)]
use streaming_iterator::StreamingIterator;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(not(test))]
use tree_sitter::Parser;
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::{Language, Node, Query, QueryCursor, Tree};

#[cfg(test)]
// Some tests here are `cfg(debug_assertions)` (they lean on debug-only seams);
// helpers only they use read as dead in release-profile test builds.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
mod tests {
    /// The reason a too deeply nested Markdown file is not parsed reaches
    /// the coding agents that call AFT's tools, in the error for that file.
    /// Its wording is a reviewed interface, so it is pinned here, and it
    /// must name the limit the guard applies.
    #[test]
    fn the_markdown_too_deep_reason_is_pinned() {
        assert_eq!(
            super::MARKDOWN_TOO_DEEP,
            "markdown is nested too deeply to parse safely (more than 128 open blocks); not parsed"
        );
        assert!(super::MARKDOWN_TOO_DEEP.contains(&format!(
            "more than {} open blocks",
            super::MARKDOWN_MAX_OPEN_BLOCKS
        )));
    }

    /// Markdown one block past the limit is refused with the pinned reason
    /// before it reaches the grammar, which would abort the whole process
    /// at 255 open blocks; one block fewer parses. Block quotes on one line
    /// and lists nested by indentation across lines are both counted.
    #[test]
    fn markdown_just_over_the_nesting_limit_is_refused_and_just_under_parses() {
        use super::{markdown_too_deep_to_parse, LangId, MARKDOWN_MAX_OPEN_BLOCKS};
        let path = std::path::Path::new("deep.md");
        // A line of n quote markers opens n blocks.
        let quotes = |n: usize| format!("# Title\n\n{}x\n", "> ".repeat(n));
        let over = quotes(MARKDOWN_MAX_OPEN_BLOCKS);
        match super::parse_source_with_parser(path, &over, LangId::Markdown) {
            Err(crate::error::AftError::ParseError { message }) => {
                assert_eq!(message, format!("deep.md: {}", super::MARKDOWN_TOO_DEEP));
            }
            other => panic!("expected the nesting refusal, got {other:?}"),
        }
        let under = quotes(MARKDOWN_MAX_OPEN_BLOCKS - 1);
        assert!(super::parse_source_with_parser(path, &under, LangId::Markdown).is_ok());

        // Line k nests k + 1 list items.
        let list = |n: usize| {
            (0..n)
                .map(|k| format!("{}- x\n", "  ".repeat(k)))
                .collect::<String>()
        };
        assert!(markdown_too_deep_to_parse(
            LangId::Markdown,
            &list(MARKDOWN_MAX_OPEN_BLOCKS)
        ));
        assert!(!markdown_too_deep_to_parse(
            LangId::Markdown,
            &list(MARKDOWN_MAX_OPEN_BLOCKS - 1)
        ));
        assert!(super::parse_source_with_parser(
            path,
            &list(MARKDOWN_MAX_OPEN_BLOCKS - 1),
            LangId::Markdown
        )
        .is_ok());
        // Only Markdown goes through the Markdown grammar.
        assert!(!markdown_too_deep_to_parse(LangId::Python, &over));
    }

    /// The limit leaves headroom: the Markdown grammar itself still parses
    /// 254 open block quotes, one short of the 255 that abort it, while the
    /// guard refuses such a file long before.
    #[test]
    fn the_markdown_grammar_parses_254_open_blocks_which_the_guard_refuses() {
        use super::LangId;
        let source = format!("{}x\n", "> ".repeat(254));
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::grammar_for(LangId::Markdown).unwrap())
            .unwrap();
        assert!(parser.parse(&source, None).is_some());
        assert!(super::markdown_too_deep_to_parse(LangId::Markdown, &source));
    }

    #[test]
    fn grammar_builtin_only_parser_rejects_dynamic_shared_cache_hits() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            FileParser::new().extract_symbols_shared(&directory.path().join("missing.luau")),
            Err(AftError::FileNotFound { .. })
        ));
        let path = directory.path().join("cached.luau");
        let source = "local function f() end";
        std::fs::write(&path, source).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut parser = FileParser::new();
        parser.symbol_cache.write().unwrap().insert(
            path.clone(),
            mtime,
            source.len() as u64,
            blake3::hash(source.as_bytes()),
            Vec::new(),
        );
        assert!(matches!(
            parser.extract_symbols_shared(&path),
            Err(AftError::InvalidRequest { .. })
        ));
        assert!(matches!(
            parser.parse(&path),
            Err(AftError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn captured_source_keeps_the_owned_read_buffer() {
        let source = "// padding\n".repeat(10_000);
        let pointer = source.as_bytes().as_ptr();
        let captured = captured_source(source);
        assert_eq!(
            captured.as_bytes().as_ptr(),
            pointer,
            "capturing a completed read must not copy its bytes"
        );
        assert_eq!(captured.len(), 110_000);
    }

    #[test]
    fn only_snapshot_consumers_retain_source_with_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("layout.rs");
        std::fs::write(&path, "pub mod api { pub fn run() {} }\n").unwrap();
        let mut parser = FileParser::new();
        parser.extract_symbols_shared(&path).unwrap();
        assert!(parser.cache[&path].source.is_none());
        assert!(parser.symbol_cache.read().unwrap().entries[&path]
            .modules
            .is_some());
        parser.evict_parse_tree(&path);
        let captured = parser.parse_snapshot(&path).unwrap();
        assert_eq!(
            captured.source.as_ref(),
            "pub mod api { pub fn run() {} }\n"
        );
        assert!(parser.cache[&path].source.is_some());
        parser.evict_parse_tree(&path);
        assert_eq!(parser.parse_tree_cache_len(), 0);
    }

    #[test]
    fn cached_symbol_count_hashing_releases_cache_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("counts.ts");
        std::fs::write(&path, "export function alpha() {}\n").unwrap();
        let mut parser = super::FileParser::new();
        parser.extract_symbols_shared(&path).unwrap();
        let cache = parser.symbol_cache();
        let meta = std::fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap() + std::time::Duration::from_secs(2);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
        let arrived = std::sync::Arc::new(std::sync::Barrier::new(2));
        let released = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker = {
            let (cache, path, arrived, released) = (
                cache.clone(),
                path.clone(),
                arrived.clone(),
                released.clone(),
            );
            std::thread::spawn(move || {
                super::FRESHNESS_TEST_BARRIERS
                    .with(|slot| *slot.borrow_mut() = Some((arrived, released)));
                super::cached_symbol_count_shared(&cache, &path, mtime, meta.len())
            })
        };
        arrived.wait();
        let writer_progress = cache.try_write().is_ok();
        released.wait();
        assert_eq!(worker.join().unwrap(), Some(1));
        assert!(
            writer_progress,
            "fallback hashing retained the shared cache read lock"
        );
    }

    use super::*;
    use crate::language::LanguageProvider;
    use std::path::{Path, PathBuf};

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(crate::test_support::manifest_dir())
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn symbols_from_source(source: &str, lang: LangId) -> Vec<Symbol> {
        let grammar = grammar_for(lang).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_symbols_from_tree(source, &tree, lang).unwrap()
    }

    #[test]
    fn h_header_pathological_objc_fallback_keeps_original_c_tree() {
        let source = include_str!("../tests/fixtures/header-fallback/is_valid.h");
        let path = Path::new("is_valid.h");
        let c_tree = parse_source_with_parser(path, source, LangId::C).unwrap();
        assert!(c_tree.root_node().has_error());
        let start = std::time::Instant::now();
        let (tree, language) = parse_resolved_source(path, source, LangId::C).unwrap();
        assert_eq!(language, LangId::C);
        assert_eq!(tree.root_node().to_sexp(), c_tree.root_node().to_sexp());
        assert!(
            start.elapsed()
                < HEADER_FALLBACK_PARSE_DEADLINE * 2 + std::time::Duration::from_secs(5)
        );
        REUSABLE_PARSERS.with(|parsers| {
            assert!(!parsers.borrow().contains_key(&LangId::ObjC));
        });
        let chunks = crate::semantic_index::chunk_view_file(
            Path::new("."),
            path,
            source.as_bytes(),
            crate::semantic_index::EmbedTextCaps::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(chunks.len(), 2);
        // A fresh parser must still accept a normal Objective-C header after
        // cancellation, rather than resuming the pathological header's state.
        let ordinary = "@interface AfterCancellation\n- (void)run;\n@end\n";
        let (tree, language) =
            parse_resolved_source(Path::new("ordinary.h"), ordinary, LangId::C).unwrap();
        assert_eq!(language, LangId::ObjC);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn h_header_cancelled_fallback_discards_partial_parser() {
        let source = "@interface Cancelled\n- (void)run;\n@end\n".repeat(1000);
        for language in [LangId::Cpp, LangId::ObjC] {
            let result = parse_source_with_parser_deadline(
                Path::new("cancelled.h"),
                &source,
                language,
                Some(std::time::Duration::ZERO),
            )
            .unwrap();
            assert!(result.is_none());
            REUSABLE_PARSERS.with(|parsers| {
                assert!(!parsers.borrow().contains_key(&language));
            });
        }
    }

    #[test]
    fn h_header_concurrent_resolution_agrees() {
        let temp = tempfile::tempdir().unwrap();
        let source = Arc::new(format!(
            "// {}\n@interface ConcurrentHeader\n- (void)run;\n@end\n",
            temp.path().display()
        ));
        let (ready, started) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let source = source.clone();
                let ready = ready.clone();
                let (release, wait) = std::sync::mpsc::channel();
                releases.push(release);
                std::thread::spawn(move || {
                    HEADER_PARSE_ON_MISS.with(|hook| {
                        *hook.borrow_mut() = Some(Box::new(move || {
                            ready.send(()).unwrap();
                            wait.recv_timeout(std::time::Duration::from_secs(10))
                                .unwrap();
                        }));
                    });
                    let (tree, language) =
                        parse_resolved_source(Path::new("concurrent.h"), &source, LangId::C)
                            .unwrap();
                    assert!(!tree.root_node().has_error());
                    (tree.root_node().to_sexp(), language)
                })
            })
            .collect();
        // Both cold misses must reach this rendezvous before either parses. A
        // retained cache lock would prevent the second worker from getting here.
        let unlocked: Vec<_> = (0..2)
            .map(|_| started.recv_timeout(std::time::Duration::from_secs(5)))
            .collect();
        for release in releases {
            let _ = release.send(());
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(unlocked.into_iter().all(|result| result == Ok(())));
        assert_eq!(results[0], results[1]);
        assert_eq!(results[0].1, LangId::ObjC);
    }

    #[test]
    fn h_header_cpp_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("widget.h");
        std::fs::write(
            &path,
            "namespace api { class Widget { public: int value() { return 42; } }; }\n",
        )
        .unwrap();
        let mut parser = FileParser::new();
        let (tree, lang) = parser.parse(&path).unwrap();
        assert_eq!(lang, LangId::Cpp);
        assert!(!tree.root_node().has_error());
        assert_eq!(parser.parse(&path).unwrap().1, LangId::Cpp);
        let symbols = parser.extract_symbols(&path).unwrap();
        assert!(symbols.iter().any(|symbol| symbol.name == "Widget"));
        assert!(symbols.iter().any(|symbol| symbol.name == "value"));
    }

    #[test]
    fn h_header_fallback_budget_and_cross_consumer_cache() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("budget.h");
        for (body, expected, parses) in [
            ("int plain(void);", LangId::C, 1),
            (
                "namespace api { template <typename T> class Widget { public: T run(T value) { return value; } }; }",
                LangId::Cpp,
                2,
            ),
            (
                "@interface Greeter\n@property int value;\n- (void)run;\n@end",
                LangId::ObjC,
                3,
            ),
            ("int broken( {", LangId::C, 3),
        ] {
            let source = format!("// {}\n{body}\n", path.display());
            std::fs::write(&path, &source).unwrap();
            let before = work_counters::cached_parser_parses();
            let (tree, lang) = parse_resolved_source(&path, &source, LangId::C).unwrap();
            assert_eq!(lang, expected, "{body}");
            assert_eq!(
                work_counters::cached_parser_parses() - before,
                parses,
                "{body}"
            );
            assert_eq!(detect_language(&path), Some(expected));
            let mut parser = FileParser::new();
            assert_eq!(parser.parse(&path).unwrap().1, expected);
            parser.extract_symbols(&path).unwrap();
            let label = format!("{expected:?}").to_lowercase();
            crate::callgraph_store::join::CallgraphBlob::extract(&source, &label, "header-test")
                .unwrap();
            let ast_lang = match expected {
                LangId::C => crate::ast_grep_lang::AstGrepLang::C,
                LangId::Cpp => crate::ast_grep_lang::AstGrepLang::Cpp,
                LangId::ObjC => crate::ast_grep_lang::AstGrepLang::ObjC,
                _ => unreachable!(),
            };
            assert!(ast_lang.matches_path(&path));
            assert_eq!(
                crate::lsp::manager::language_id_for_path(&path, Some(&source)),
                match expected {
                    LangId::ObjC => "objective-c",
                    LangId::Cpp => "cpp",
                    _ => "c",
                }
            );
            let path_for_thread = path.clone();
            let source_for_thread = source.clone();
            std::thread::spawn(move || {
                let before = work_counters::cached_parser_parses();
                assert_eq!(
                    detect_language_from_source(&path_for_thread, &source_for_thread),
                    Some(expected)
                );
                assert_eq!(work_counters::cached_parser_parses(), before);
            })
            .join()
            .unwrap();
            assert_eq!(work_counters::cached_parser_parses() - before, parses);
            if expected == LangId::C && parses == 3 {
                let old_c = parse_source_with_parser(&path, &source, LangId::C).unwrap();
                assert_eq!(tree.root_node().to_sexp(), old_c.root_node().to_sexp());
                assert!(tree.root_node().has_error());
            }
        }
    }

    #[test]
    fn h_header_missing_nodes_count_as_errors_and_keep_c_when_all_fail() {
        let source = "int missing_semicolon(void)";
        let old_c = parse_source_with_parser(Path::new("missing.c"), source, LangId::C).unwrap();
        assert!(old_c.root_node().has_error());
        assert!(old_c.root_node().to_sexp().contains("MISSING"));
        let (tree, lang) =
            parse_resolved_source(Path::new("missing.h"), source, LangId::C).unwrap();
        assert_eq!(lang, LangId::C);
        assert_eq!(tree.root_node().to_sexp(), old_c.root_node().to_sexp());
    }

    #[test]
    fn h_header_clean_c_tree_wins_for_ambiguous_declarations() {
        let source = "class Widget { public: void run(); };\n";
        let c_tree = parse_source_with_parser(Path::new("ambiguous.c"), source, LangId::C).unwrap();
        assert!(!c_tree.root_node().has_error());
        let (tree, language) =
            parse_resolved_source(Path::new("ambiguous.h"), source, LangId::C).unwrap();
        assert_eq!(language, LangId::C);
        assert_eq!(tree.root_node().to_sexp(), c_tree.root_node().to_sexp());
    }

    #[test]
    fn h_header_source_snapshot_overrides_disk_language() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot.h");
        std::fs::write(&path, "int c_function(void);\n").unwrap();
        let source = "class Snapshot { public: int value() { return 7; } };\n";
        let mut parser = FileParser::new();
        let hash = content_hash_for_source(source);
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let (tree, lang) = parser
            .parse_with_source(&path, source, mtime, source.len() as u64, hash)
            .unwrap();
        assert_eq!(lang, LangId::Cpp);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn variable_name_membership_is_linear_and_byte_identical() {
        for (lang, count) in [
            (LangId::TypeScript, 4000),
            (LangId::JavaScript, 400),
            (LangId::Zig, 400),
        ] {
            let mut source = String::new();
            for i in 0..count {
                source.push_str(&format!("const generated_{i} = {i};\n"));
            }
            if lang != LangId::Zig {
                source.push_str("function local() {\n");
                for i in 0..count {
                    source.push_str(&format!("const local_{i} = {i};\n"));
                }
                source.push_str("}\nconst callback = () => 1;\nclass C { generated_0() {} }\n");
            }
            LEGACY_VARIABLE_NAMES.with(|legacy| legacy.set(true));
            VARIABLE_NAME_WORK.with(|work| work.set(0));
            let reference = symbols_from_source(&source, lang);
            let before = VARIABLE_NAME_WORK.with(|work| work.get());
            LEGACY_VARIABLE_NAMES.with(|legacy| legacy.set(false));
            VARIABLE_NAME_WORK.with(|work| work.set(0));
            let actual = symbols_from_source(&source, lang);
            let after = VARIABLE_NAME_WORK.with(|work| work.get());
            assert_eq!(
                serde_json::to_vec(&actual).unwrap(),
                serde_json::to_vec(&reference).unwrap()
            );
            assert!(actual.len() >= count);
            eprintln!("{lang:?}: variable membership work {before} -> {after}");
            assert!(
                after <= 2 * actual.len(),
                "{after} name operations for {} symbols",
                actual.len()
            );
        }
    }

    #[test]
    fn shared_symbol_hits_and_resolution_do_not_copy_the_table() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("generated.ts");
        let source: String = (0..1000)
            .map(|i| format!("export const c{i} = {i};\n"))
            .collect();
        std::fs::write(&file, source).unwrap();
        let provider = TreeSitterProvider::new();
        let original = list_symbols_shared(&provider, &file, false).unwrap();
        let serialized = serde_json::to_vec(original.as_ref()).unwrap();
        let before = work_counters::CACHED_SYMBOL_COPIES.with(|count| count.get());
        for _ in 0..10 {
            let hit = list_symbols_shared(&provider, &file, false).unwrap();
            assert!(
                Arc::ptr_eq(&original, &hit),
                "a cache hit must share the table allocation"
            );
            assert_eq!(serde_json::to_vec(hit.as_ref()).unwrap(), serialized);
            let resolved = provider.resolve_symbol(&file, "c500").unwrap();
            assert_eq!(resolved[0].symbol, original[500]);
        }
        assert_eq!(
            work_counters::CACHED_SYMBOL_COPIES.with(|count| count.get()) - before,
            0
        );
        // The owned compatibility API still returns the exact same symbols.
        assert_eq!(
            serde_json::to_vec(&provider.list_symbols(&file).unwrap()).unwrap(),
            serialized
        );
    }

    #[test]
    fn non_js_symbol_miss_does_not_read_or_parse_again() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.rs");
        std::fs::write(&file, "pub fn present() {}\n").unwrap();
        let provider = TreeSitterProvider::new();
        provider.list_symbols(&file).unwrap();
        let reads = work_counters::PARSED_FILE_READS.with(|count| count.get());
        let parses = work_counters::tree_parses();
        assert!(matches!(
            provider.resolve_symbol(&file, "missing"),
            Err(AftError::SymbolNotFound { .. })
        ));
        assert_eq!(
            work_counters::PARSED_FILE_READS.with(|count| count.get()) - reads,
            0
        );
        assert_eq!(work_counters::tree_parses() - parses, 0);
    }

    /// Export detection reads export statements from one tree walk and finds
    /// the containing statement by binary search. It must agree, for every
    /// node, with the old method: a full query pass for `@export.stmt`
    /// captures and a scan of all of them.
    #[test]
    fn export_index_agrees_with_query_ranges_for_every_node() {
        let mut sources: Vec<(String, LangId)> = [
            ("sample.ts", LangId::TypeScript),
            ("structure_ts.ts", LangId::TypeScript),
            ("member_ts.ts", LangId::TypeScript),
            ("imports_ts.ts", LangId::TypeScript),
            ("sample.tsx", LangId::Tsx),
            ("sample.js", LangId::JavaScript),
            ("imports_js.js", LangId::JavaScript),
        ]
        .into_iter()
        .map(|(name, lang)| (std::fs::read_to_string(fixture_path(name)).unwrap(), lang))
        .collect();
        sources.push((
            "export namespace Outer {\n  export function inner() {}\n  function hidden() {}\n  export namespace Deep { export const x = 1; }\n}\nfunction after() {}\nexport { after };\nexport default class {}\nexport function (\n".to_string(),
            LangId::TypeScript,
        ));
        for (source, lang) in &sources {
            let mut parser = Parser::new();
            parser.set_language(&grammar_for(*lang).unwrap()).unwrap();
            let tree = parser.parse(source, None).unwrap();
            let root = tree.root_node();
            let query = cached_query_for(*lang).unwrap().unwrap();
            let export_idx = query
                .capture_names()
                .iter()
                .position(|name| *name == "export.stmt")
                .unwrap() as u32;
            let mut reference = Vec::new();
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(query, root, source.as_bytes());
            while let Some(m) = {
                matches.advance();
                matches.get()
            } {
                for capture in m.captures {
                    if capture.index == export_idx {
                        reference.push(capture.node.byte_range());
                    }
                }
            }
            let index = collect_exports(source, &root, Direct);
            assert_eq!(index.names, collect_exported_symbol_names(source, &root));
            let mut walk = root.walk();
            let mut checked = 0usize;
            loop {
                let node = walk.node();
                let range = node.byte_range();
                let expected = reference
                    .iter()
                    .any(|er| er.start <= range.start && range.end <= er.end);
                assert_eq!(
                    is_exported(&node, &index.ranges),
                    expected,
                    "{} at {:?} in {source:?}",
                    node.kind(),
                    range
                );
                checked += 1;
                if walk.goto_first_child() {
                    continue;
                }
                while !walk.goto_next_sibling() {
                    if !walk.goto_parent() {
                        break;
                    }
                }
                if walk.node() == root {
                    break;
                }
            }
            assert!(checked > 10);
        }
    }

    #[test]
    fn export_detection_costs_one_check_per_symbol() {
        let mut source = String::new();
        for index in 0..300 {
            source.push_str(&format!("export function f{index}() {{}}\n"));
        }
        let before = EXPORT_RANGE_COMPARISONS.with(|count| count.get());
        let symbols = symbols_from_source(&source, LangId::TypeScript);
        let comparisons = EXPORT_RANGE_COMPARISONS.with(|count| count.get()) - before;
        assert_eq!(symbols.len(), 300);
        assert!(symbols.iter().all(|symbol| symbol.exported));
        assert!(
            comparisons <= symbols.len(),
            "{comparisons} export-range comparisons for {} symbols",
            symbols.len()
        );
    }

    /// A symbol-cache lookup snapshots the entry under the read lock. The
    /// snapshot must not copy the symbol list: it is copied once, after the
    /// lock is released, and only when the entry is still fresh. A stale
    /// entry is re-extracted without ever copying the old list.
    #[test]
    fn symbol_cache_lookup_copies_symbols_only_for_fresh_hits() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a.ts");
        let mut source = String::new();
        for index in 0..50 {
            source.push_str(&format!("export function f{index}() {{}}\n"));
        }
        std::fs::write(&path, &source).unwrap();
        let mut parser = FileParser::new();
        parser.extract_symbols(&path).unwrap();
        parser.evict_parse_tree(&path);

        let copies = || work_counters::CACHED_SYMBOL_COPIES.with(|count| count.get());
        let before = copies();
        let fresh = parser.extract_symbols(&path).unwrap();
        assert_eq!(fresh.len(), 50);
        assert_eq!(copies() - before, 1, "a fresh hit copies the list once");

        // A different size proves staleness without hashing.
        std::fs::write(&path, "export function only() {}\n").unwrap();
        let before = copies();
        let reparsed = parser.extract_symbols(&path).unwrap();
        assert_eq!(reparsed.len(), 1);
        assert_eq!(copies() - before, 0, "a stale entry must not be copied");
    }

    /// Resolving a name through re-exports reads each visited file's source
    /// once for both the default-export lookup and the re-export scan; each
    /// of them used to read and hash the file again.
    #[test]
    fn default_resolution_reads_each_visited_file_once() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.ts");
        std::fs::write(&a, "export * from './b';\n").unwrap();
        std::fs::write(
            temp.path().join("b.ts"),
            "export default function target() {}\n",
        )
        .unwrap();
        let provider = TreeSitterProvider::new();
        let before = work_counters::PARSED_FILE_READS.with(|count| count.get());
        let matches = provider.resolve_symbol(&a, "default").unwrap();
        let reads = work_counters::PARSED_FILE_READS.with(|count| count.get()) - before;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].symbol.name, "target");
        assert_eq!(reads, 2, "one read per visited file");
    }

    /// The language provider builds a `FileParser` per call. Those parsers
    /// must share this thread's retained tree-sitter parser instead of each
    /// allocating a parser and loading the grammar again.
    #[test]
    fn provider_calls_reuse_the_thread_parser() {
        let temp = tempfile::tempdir().unwrap();
        let provider = TreeSitterProvider::new();
        // Warm this thread's parser so the count below isolates the calls.
        let warm = temp.path().join("warm.ts");
        std::fs::write(&warm, "export const warm = 1;\n").unwrap();
        provider.list_symbols(&warm).unwrap();

        let loads_before = work_counters::grammar_loads();
        let parses_before = work_counters::tree_parses();
        for index in 0..10 {
            let path = temp.path().join(format!("f{index}.ts"));
            std::fs::write(&path, format!("export function f{index}() {{}}\n")).unwrap();
            let symbols = provider.list_symbols(&path).unwrap();
            assert_eq!(symbols[0].name, format!("f{index}"));
        }
        assert_eq!(work_counters::grammar_loads() - loads_before, 0);
        assert_eq!(work_counters::tree_parses() - parses_before, 10);
    }

    /// The blank-line check between a doc comment and its symbol must only
    /// look at the bytes between them. It used to split the whole file into
    /// lines once per documented symbol, so a file with N documented symbols
    /// cost N full passes.
    #[test]
    fn adjacent_line_check_scans_only_the_gap_and_keeps_doc_attachment() {
        let mut source = String::new();
        for index in 0..200 {
            source.push_str(&format!(
                "/** doc {index} */\nexport function f{index}() {{}}\n\n"
            ));
            source.push_str(&format!(
                "/** detached {index} */\n\nexport function g{index}() {{}}\n\n"
            ));
        }
        let scanned_before = ADJACENT_LINE_BYTES_SCANNED.with(|bytes| bytes.get());
        let symbols = symbols_from_source(&source, LangId::TypeScript);
        let scanned = ADJACENT_LINE_BYTES_SCANNED.with(|bytes| bytes.get()) - scanned_before;
        assert!(
            scanned <= source.len(),
            "the check scanned {scanned} bytes of a {} byte file",
            source.len()
        );
        assert_eq!(symbols.len(), 400);
        for symbol in &symbols {
            let doc_row = if symbol.name.starts_with('f') {
                // An adjacent JSDoc block belongs to the symbol.
                symbol.range.start_line + 1
            } else {
                // A blank line detaches the JSDoc block.
                symbol.range.start_line
            };
            let declaration_row = source
                .lines()
                .position(|line| line.contains(&format!("function {}()", symbol.name)))
                .unwrap() as u32;
            assert_eq!(doc_row, declaration_row, "{}", symbol.name);
        }
    }

    #[test]
    fn adjacent_line_check_handles_crlf_and_whitespace_only_rows() {
        let cases = [
            ("/** a */\r\nfunction f() {}\r\n", 0),
            ("/** a */\r\n\r\nfunction f() {}\r\n", 2),
            ("/** a */\n   \t\nfunction f() {}\n", 2),
            ("/** a\n * b\n */\nfunction f() {}\n", 0),
            ("/** a */ function f() {}\n", 0),
        ];
        for (source, start_line) in cases {
            let symbols = symbols_from_source(source, LangId::TypeScript);
            assert_eq!(symbols[0].range.start_line, start_line, "{source:?}");
        }
    }

    /// The export walk must not spend stack per tree depth. This runs it on a
    /// deliberately small thread over a tree nested far deeper than the old
    /// recursive walk survived at that size; a return to recursion overflows
    /// here (the abort is deterministic, not a timing flake) instead of in the
    /// daemon, where a URL that parsed as TypeScript did exactly that.
    #[test]
    fn export_walk_survives_deep_trees_on_a_small_stack() {
        const DEPTH: usize = 3000;
        // The export clause sits after the deep expression: the walk is
        // pre-order over the whole tree, so it must come through the nesting
        // to find it, and finding it proves the traversal completed.
        let mut source = String::from("const deep = ");
        source.push_str(&"(".repeat(DEPTH));
        source.push('1');
        source.push_str(&")".repeat(DEPTH));
        source.push_str(";\nfunction shallow() {}\nexport { deep, shallow };\n");

        let exported = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let grammar = grammar_for(LangId::TypeScript).unwrap();
                let mut parser = Parser::new();
                parser.set_language(&grammar).unwrap();
                let tree = parser.parse(&source, None).unwrap();
                collect_exported_symbol_names(&source, &tree.root_node())
            })
            .unwrap()
            .join()
            .expect("export walk must not overflow a 256 KiB stack");

        assert!(exported.contains("deep"), "exports: {exported:?}");
        assert!(exported.contains("shallow"), "exports: {exported:?}");
    }

    /// Function-type detection must not spend stack per nested type node. A
    /// property signature may legally contain thousands of nested generic types,
    /// and extraction runs on executor workers with bounded stacks.
    #[test]
    fn interface_function_type_scan_survives_deep_types_on_a_small_stack() {
        const DEPTH: usize = 3000;
        let mut source = String::from("interface Deep {\n  handler: ");
        source.push_str(&"Array<".repeat(DEPTH));
        source.push_str("() => void");
        source.push_str(&">".repeat(DEPTH));
        source.push_str(";\n}\n");

        let symbols = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || symbols_from_source(&source, LangId::TypeScript))
            .unwrap()
            .join()
            .expect("interface member extraction must not overflow a 256 KiB stack");

        assert!(
            symbols.iter().any(|symbol| {
                symbol.name == "handler" && symbol.parent.as_deref() == Some("Deep")
            }),
            "deep function-valued property should still be extracted: {symbols:?}"
        );
    }

    #[test]
    #[ignore = "manual single-file parser/query phase benchmark"]
    fn profile_rust_parser_and_query_phases() {
        let source = "pub fn tiny() -> bool {\n    true\n}\n";

        let grammar_started = std::time::Instant::now();
        let grammar = grammar_for(LangId::Rust).unwrap();
        let grammar_elapsed = grammar_started.elapsed();

        let parser_init_started = std::time::Instant::now();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let parser_init_elapsed = parser_init_started.elapsed();

        let parse_started = std::time::Instant::now();
        let tree = parser.parse(source, None).unwrap();
        let parse_elapsed = parse_started.elapsed();

        let query_compile_started = std::time::Instant::now();
        let query = Query::new(&grammar, RS_QUERY).unwrap();
        let query_compile_elapsed = query_compile_started.elapsed();

        let query_execute_started = std::time::Instant::now();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut query_matches = 0;
        while matches.next().is_some() {
            query_matches += 1;
        }
        let query_execute_elapsed = query_execute_started.elapsed();

        let direct_extract_started = std::time::Instant::now();
        let symbols = extract_rs_symbols(source, &tree.root_node(), Direct).unwrap();
        let direct_extract_elapsed = direct_extract_started.elapsed();

        eprintln!(
            "rust parser/query phases: grammar={grammar_elapsed:?} parser_init={parser_init_elapsed:?} parse_execute={parse_elapsed:?} query_compile={query_compile_elapsed:?} query_execute={query_execute_elapsed:?} direct_extract={direct_extract_elapsed:?} symbols={}",
            symbols.len()
        );
        assert!(query_matches > 0);
        assert_eq!(symbols.len(), 1);
    }

    fn extract_rs_symbols_with_query_oracle(source: &str, root: Node<'_>) -> Vec<Symbol> {
        let grammar = grammar_for(LangId::Rust).unwrap();
        let query = Query::new(&grammar, RS_QUERY).expect("compile Rust parity query");
        let capture_names = query.capture_names();
        let is_pub = |node: &Node<'_>| {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if cursor.node().kind() == "visibility_modifier" {
                        return true;
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            false
        };

        let mut symbols = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source.as_bytes());
        while let Some(query_match) = {
            matches.advance();
            matches.get()
        } {
            let mut function_name = None;
            let mut function_definition = None;
            let mut item_name = None;
            let mut item_definition = None;
            let mut item_kind = None;
            let mut impl_node = None;
            for capture in query_match.captures {
                let Some(name) = capture_names.get(capture.index as usize) else {
                    continue;
                };
                match *name {
                    "fn.name" => function_name = Some(capture.node),
                    "fn.def" => function_definition = Some(capture.node),
                    "struct.name" => {
                        item_name = Some(capture.node);
                        item_kind = Some(SymbolKind::Struct);
                    }
                    "struct.def" => item_definition = Some(capture.node),
                    "enum.name" => {
                        item_name = Some(capture.node);
                        item_kind = Some(SymbolKind::Enum);
                    }
                    "enum.def" => item_definition = Some(capture.node),
                    "trait.name" => {
                        item_name = Some(capture.node);
                        item_kind = Some(SymbolKind::Interface);
                    }
                    "trait.def" => item_definition = Some(capture.node),
                    "impl.def" => impl_node = Some(capture.node),
                    _ => {}
                }
            }

            if let (Some(name_node), Some(definition)) = (function_name, function_definition) {
                let owner = rust_function_declaration_list_owner(&definition, Direct);
                if owner
                    .as_ref()
                    .is_none_or(|owner| owner.kind() == "mod_item")
                {
                    let scope_chain = rust_mod_scope_chain(&definition, source, Direct);
                    symbols.push(Symbol {
                        name: node_text(source, &name_node).to_string(),
                        kind: SymbolKind::Function,
                        range: node_range_with_decorators(&definition, source, LangId::Rust),
                        signature: Some(extract_signature(source, &definition)),
                        scope_chain: scope_chain.clone(),
                        exported: is_pub(&definition),
                        parent: scope_chain.last().cloned(),
                    });
                }
            }

            if let (Some(name_node), Some(definition), Some(kind)) =
                (item_name, item_definition, item_kind)
            {
                symbols.push(Symbol {
                    name: node_text(source, &name_node).to_string(),
                    kind,
                    range: node_range_with_decorators(&definition, source, LangId::Rust),
                    signature: Some(extract_signature(source, &definition)),
                    scope_chain: Vec::new(),
                    exported: is_pub(&definition),
                    parent: None,
                });
            }

            if let Some(impl_node) = impl_node {
                let scope_name = rust_impl_scope_name(&impl_node, source);
                let parent_name =
                    rust_impl_parent_name(scope_name.rsplit(" for ").next().unwrap_or_default());
                let mut impl_cursor = impl_node.walk();
                if impl_cursor.goto_first_child() {
                    loop {
                        let child = impl_cursor.node();
                        if child.kind() == "declaration_list" {
                            let mut method_cursor = child.walk();
                            if method_cursor.goto_first_child() {
                                loop {
                                    let method = method_cursor.node();
                                    if method.kind() == "function_item" {
                                        if let Some(name_node) = method.child_by_field_name("name")
                                        {
                                            symbols.push(Symbol {
                                                name: node_text(source, &name_node).to_string(),
                                                kind: SymbolKind::Method,
                                                range: node_range_with_decorators(
                                                    &method,
                                                    source,
                                                    LangId::Rust,
                                                ),
                                                signature: Some(extract_signature(source, &method)),
                                                scope_chain: (!scope_name.is_empty())
                                                    .then(|| vec![scope_name.clone()])
                                                    .unwrap_or_default(),
                                                exported: is_pub(&method),
                                                parent: (!parent_name.is_empty())
                                                    .then(|| parent_name.clone()),
                                            });
                                        }
                                    }
                                    if !method_cursor.goto_next_sibling() {
                                        break;
                                    }
                                }
                            }
                        }
                        if !impl_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
        }

        dedup_symbols(&mut symbols);
        symbols
    }

    fn rust_symbol_fingerprint(symbols: &[Symbol]) -> Vec<String> {
        symbols
            .iter()
            .map(|symbol| {
                format!(
                    "{}|{:?}|{:?}|{:?}|{:?}|{}|{:?}",
                    symbol.name,
                    symbol.kind,
                    symbol.range,
                    symbol.signature,
                    symbol.scope_chain,
                    symbol.exported,
                    symbol.parent
                )
            })
            .collect()
    }

    #[test]
    fn rust_walk_matches_query_oracle_for_adversarial_shapes() {
        let source = r#"
#[cfg(any())]
pub(crate) mod outer {
    pub mod inner {
        #[doc = "keeps decorator range"]
        pub(crate) fn nested<T>() {}

        pub(crate) struct Generic<T>(T);
        pub enum State { Ready }
        pub trait Service<T> {
            fn declaration(&self);
        }

        impl<T> Service<T> for Generic<T> {
            #[inline]
            pub(crate) fn attributed_method(&self) {}
            fn private_method(&self) {}
        }
    }
}
"#;
        let grammar = grammar_for(LangId::Rust).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();

        let query_symbols = extract_rs_symbols_with_query_oracle(source, tree.root_node());
        let walk_symbols = extract_rs_symbols(source, &tree.root_node(), Direct).unwrap();
        assert_eq!(
            rust_symbol_fingerprint(&walk_symbols),
            rust_symbol_fingerprint(&query_symbols)
        );
        assert!(walk_symbols.iter().any(|symbol| {
            symbol.name == "nested" && symbol.exported && symbol.scope_chain == ["outer", "inner"]
        }));
        assert!(walk_symbols.iter().any(|symbol| {
            symbol.name == "attributed_method"
                && symbol.kind == SymbolKind::Method
                && symbol.scope_chain == ["Service<T> for Generic<T>"]
                && symbol.exported
        }));
        assert!(
            walk_symbols
                .iter()
                .all(|symbol| symbol.name != "declaration"),
            "trait declarations are not implementation methods"
        );
    }

    #[test]
    fn source_parser_is_reused_on_the_current_worker_thread() {
        let first = "pub fn first() {}\n";
        let second = "pub struct Second;\n";
        let path = Path::new("worker.rs");

        let before = REUSABLE_PARSERS.with(|parsers| parsers.borrow().len());
        let first_tree =
            parse_source_with_cached_parser(path, first, LangId::Rust).expect("parse first source");
        let after_first = REUSABLE_PARSERS.with(|parsers| parsers.borrow().len());
        let second_tree = parse_source_with_cached_parser(path, second, LangId::Rust)
            .expect("parse second source");
        let after_second = REUSABLE_PARSERS.with(|parsers| parsers.borrow().len());

        assert!(!first_tree.root_node().has_error());
        assert!(!second_tree.root_node().has_error());
        assert!(after_first == before || after_first == before + 1);
        assert_eq!(after_second, after_first);
    }

    fn test_symbol(name: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 10,
            },
            signature: Some(format!("fn {name}()")),
            scope_chain: Vec::new(),
            exported: true,
            parent: None,
        }
    }

    #[test]
    fn symbol_memory_estimate_is_zero_when_empty_and_nonzero_when_populated() {
        let mut cache = SymbolCache::new();
        assert_eq!(cache.estimated_memory().estimated_bytes, Some(0));
        cache.insert(
            PathBuf::from("src/lib.rs"),
            SystemTime::UNIX_EPOCH,
            10,
            cache_freshness::zero_hash(),
            vec![test_symbol("memory_estimate")],
        );
        let estimate = cache.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert_eq!(estimate.counts["entries"], 1);
        assert_eq!(estimate.counts["symbols"], 1);
    }

    fn assert_symbol_memory_recount(cache: &SymbolCache, step: &str) {
        assert_eq!(
            cache.estimated_memory(),
            cache.recounted_memory_for_test(),
            "{step}"
        );
    }

    #[test]
    fn symbol_memory_running_totals_include_outline_facts() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("lib.rs");
        let cache = Arc::new(RwLock::new(SymbolCache::new()));
        let ctx = crate::context::AppContext::new(
            Box::new(TreeSitterProvider::with_symbol_cache(Arc::clone(&cache))),
            crate::config::Config::default(),
        );
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "memory-facts", "command": "outline", "files": [&file],
        }))
        .unwrap();
        let mut parser = FileParser::with_symbol_cache(Arc::clone(&cache));
        for source in [
            "pub trait Api { fn get(&self); }\n#[cfg(test)] mod tests { #[test] fn works() {} }\n",
            "pub fn small() {}\n",
        ] {
            std::fs::write(&file, source).unwrap();
            let response = crate::commands::outline::handle_outline(&request, &ctx);
            assert!(response.success, "{response:?}");
            assert!(parser.cached_outline(&file).is_some());
            assert_symbol_memory_recount(&cache.read().unwrap(), "attach facts through outline");
            let parsed = parser.parse_snapshot(&file).unwrap();
            let facts = parser.cached_outline(&file).unwrap().facts;
            parser.store_outline_facts(&file, &parsed, facts);
            assert_symbol_memory_recount(
                &cache.read().unwrap(),
                "replace facts without double counting",
            );
        }
        cache.write().unwrap().invalidate(&file);
        assert_symbol_memory_recount(&cache.read().unwrap(), "evict entry with facts");
    }

    #[test]
    #[ignore = "offline symbol-memory before/after timing probe"]
    fn symbol_memory_many_projects_timing_probe() {
        let caches: Vec<_> = (0..190)
            .map(|project| {
                let mut cache = SymbolCache::new();
                for file in 0..500 {
                    cache.insert(
                        PathBuf::from(format!("/project-{project}/src/file-{file}.rs")),
                        SystemTime::UNIX_EPOCH,
                        10,
                        cache_freshness::zero_hash(),
                        (0..20)
                            .map(|symbol| test_symbol(&format!("symbol_{symbol}")))
                            .collect(),
                    );
                }
                assert_symbol_memory_recount(&cache, "timing fixture");
                cache
            })
            .collect();
        let measure = |recount: bool| {
            let start = std::time::Instant::now();
            let mut bytes = 0u64;
            for _ in 0..10 {
                for cache in &caches {
                    let cache = std::hint::black_box(cache);
                    let estimate = if recount {
                        cache.recounted_memory_for_test()
                    } else {
                        cache.estimated_memory()
                    };
                    bytes += std::hint::black_box(estimate).estimated_bytes.unwrap();
                }
            }
            (start.elapsed() / 10, bytes)
        };
        let (before, before_bytes) = measure(true);
        let (after, after_bytes) = measure(false);
        assert_eq!(before_bytes, after_bytes);
        eprintln!("symbol memory tick: projects=190 files_per_project=500 symbols_per_file=20 before={before:?} after={after:?} bytes_per_tick={}", after_bytes / 10);
    }

    #[test]
    fn inc_and_scss_extensions_are_detected() {
        assert_eq!(
            detect_language(Path::new("template.inc")),
            Some(LangId::Php)
        );
        assert_eq!(
            detect_language(Path::new("styles.scss")),
            Some(LangId::Scss)
        );
        assert_eq!(detect_language(Path::new("main.pas")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.pp")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.dpr")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.dpk")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.lpr")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("template.tpl")), None);
    }

    #[test]
    fn inc_files_parse_with_php_grammar() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let file = tmp.path().join("partial.inc");
        std::fs::write(&file, "<?php\nfunction render_partial() { return 1; }\n")
            .expect("write inc file");

        let mut parser = FileParser::new();
        let symbols = parser
            .extract_symbols(&file)
            .expect("extract php inc symbols");
        let function = symbols
            .iter()
            .find(|symbol| symbol.name == "render_partial")
            .expect("find PHP function in .inc");
        assert_eq!(function.kind, SymbolKind::Function);
    }

    #[test]
    fn scss_symbols_include_mixin_variable_function_and_rule() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let file = tmp.path().join("styles.scss");
        std::fs::write(
            &file,
            r#"$brand-color: #336699;

@mixin button-base($padding) {
  padding: $padding;
}

@function double($value) {
  @return $value * 2;
}

.card, .panel {
  color: $brand-color;
}
"#,
        )
        .expect("write scss file");

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).expect("extract scss symbols");
        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("button-base").kind, SymbolKind::Function);
        assert_eq!(get("double").kind, SymbolKind::Function);
        assert_eq!(get("$brand-color").kind, SymbolKind::Variable);
        assert_eq!(get(".card, .panel").kind, SymbolKind::Class);
    }

    // --- Language detection ---

    #[test]
    fn detect_ts() {
        assert_eq!(
            detect_language(Path::new("foo.ts")),
            Some(LangId::TypeScript)
        );
    }

    #[test]
    fn detect_tsx() {
        assert_eq!(detect_language(Path::new("foo.tsx")), Some(LangId::Tsx));
    }

    #[test]
    fn detect_js() {
        assert_eq!(
            detect_language(Path::new("foo.js")),
            Some(LangId::JavaScript)
        );
    }

    #[test]
    fn detect_jsx() {
        assert_eq!(
            detect_language(Path::new("foo.jsx")),
            Some(LangId::JavaScript)
        );
    }

    #[test]
    fn detect_py() {
        assert_eq!(detect_language(Path::new("foo.py")), Some(LangId::Python));
    }

    #[test]
    fn detect_rs() {
        assert_eq!(detect_language(Path::new("foo.rs")), Some(LangId::Rust));
    }

    #[test]
    fn detect_go() {
        assert_eq!(detect_language(Path::new("foo.go")), Some(LangId::Go));
    }

    #[test]
    fn detect_c() {
        assert_eq!(detect_language(Path::new("foo.c")), Some(LangId::C));
    }

    #[test]
    fn detect_h() {
        assert_eq!(detect_language(Path::new("foo.h")), Some(LangId::C));
    }

    #[test]
    fn detect_cc() {
        assert_eq!(detect_language(Path::new("foo.cc")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_cpp() {
        assert_eq!(detect_language(Path::new("foo.cpp")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_cxx() {
        assert_eq!(detect_language(Path::new("foo.cxx")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_hpp() {
        assert_eq!(detect_language(Path::new("foo.hpp")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_hh() {
        assert_eq!(detect_language(Path::new("foo.hh")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_zig() {
        assert_eq!(detect_language(Path::new("foo.zig")), Some(LangId::Zig));
    }

    #[test]
    fn detect_cs() {
        assert_eq!(detect_language(Path::new("foo.cs")), Some(LangId::CSharp));
    }

    #[test]
    fn detect_unknown_returns_none() {
        assert_eq!(detect_language(Path::new("foo.txt")), None);
    }

    #[test]
    fn groovy_language_constant_loads_under_workspace_tree_sitter() {
        let mut parser = Parser::new();
        let language: Language = dekobon_tree_sitter_groovy::LANGUAGE.into();
        parser
            .set_language(&language)
            .expect("load Groovy grammar under workspace tree-sitter");
        let tree = parser
            .parse("class Greeter { def greet() { 'hi' } }", None)
            .expect("parse Groovy sample");
        assert!(
            !tree.root_node().has_error(),
            "Groovy sample should parse cleanly"
        );
    }

    #[test]
    fn detect_groovy_extensions_and_jenkinsfile() {
        assert_eq!(
            detect_language(Path::new("script.groovy")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("script.gvy")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("script.gy")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("shell.gsh")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("build.gradle")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("build.gradle.kts")),
            Some(LangId::Kotlin)
        );
        assert_eq!(
            detect_language(Path::new("Jenkinsfile")),
            Some(LangId::Groovy)
        );
    }

    // --- Unsupported extension error ---

    #[test]
    fn unsupported_extension_returns_invalid_request() {
        // Use a file that exists but has an unsupported extension
        let path = fixture_path("sample.ts");
        let bad_path = path.with_extension("txt");
        // Create a dummy file so the error comes from language detection, not I/O
        std::fs::write(&bad_path, "hello").unwrap();
        let provider = TreeSitterProvider::new();
        let result = provider.list_symbols(&bad_path);
        std::fs::remove_file(&bad_path).ok();
        match result {
            Err(AftError::InvalidRequest { message }) => {
                assert!(
                    message.contains("unsupported file extension"),
                    "msg: {}",
                    message
                );
                assert!(message.contains("txt"), "msg: {}", message);
            }
            other => panic!("expected InvalidRequest, got {:?}", other),
        }
    }

    // --- TypeScript extraction ---

    #[test]
    fn ts_extracts_all_symbol_kinds() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"greet"),
            "missing function greet: {:?}",
            names
        );
        assert!(names.contains(&"add"), "missing arrow fn add: {:?}", names);
        assert!(
            names.contains(&"UserService"),
            "missing class UserService: {:?}",
            names
        );
        assert!(
            names.contains(&"Config"),
            "missing interface Config: {:?}",
            names
        );
        assert!(
            names.contains(&"Status"),
            "missing enum Status: {:?}",
            names
        );
        assert!(
            names.contains(&"UserId"),
            "missing type alias UserId: {:?}",
            names
        );
        assert!(
            names.contains(&"internalHelper"),
            "missing non-exported fn: {:?}",
            names
        );

        // At least 6 unique symbols as required
        assert!(
            symbols.len() >= 6,
            "expected ≥6 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn ts_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("greet").kind, SymbolKind::Function);
        assert_eq!(find("add").kind, SymbolKind::Function); // arrow fn → Function
        assert_eq!(find("UserService").kind, SymbolKind::Class);
        assert_eq!(find("Config").kind, SymbolKind::Interface);
        assert_eq!(find("Status").kind, SymbolKind::Enum);
        assert_eq!(find("UserId").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn ts_export_detection() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert!(find("greet").exported, "greet should be exported");
        assert!(find("add").exported, "add should be exported");
        assert!(
            find("UserService").exported,
            "UserService should be exported"
        );
        assert!(find("Config").exported, "Config should be exported");
        assert!(find("Status").exported, "Status should be exported");
        assert!(
            !find("internalHelper").exported,
            "internalHelper should not be exported"
        );
    }

    #[test]
    fn ts_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let methods: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(!methods.is_empty(), "should have at least one method");

        for method in &methods {
            assert_eq!(
                method.scope_chain,
                vec!["UserService"],
                "method {} should have UserService in scope chain",
                method.name
            );
            assert_eq!(method.parent.as_deref(), Some("UserService"));
        }
    }

    #[test]
    fn ts_extracts_interface_callable_signatures_without_bodies() {
        let symbols = symbols_from_source(
            r#"export interface Shape {
  area(): number;
  scale(f: number): Shape;
  transform: (point: number) => number;
  label: string;
}
"#,
            LangId::TypeScript,
        );

        let members = symbols
            .iter()
            .filter(|symbol| symbol.parent.as_deref() == Some("Shape"))
            .collect::<Vec<_>>();
        assert_eq!(
            members
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["area", "scale", "transform"]
        );
        for member in members {
            assert_eq!(member.kind, SymbolKind::Method);
            assert_eq!(member.scope_chain, vec!["Shape"]);
            assert_eq!(member.range.start_line, member.range.end_line);
            assert!(
                !member.signature.as_deref().unwrap().contains('{'),
                "interface signature must not imply an implementation: {member:?}"
            );
        }
        assert!(symbols.iter().all(|symbol| symbol.name != "label"));
    }

    #[test]
    fn ts_extracts_object_property_callables_and_named_alias() {
        let symbols = symbols_from_source(
            r#"export const helper = {
  compute: function doCompute(x: number) { return x * 2; },
  arrow: (y: number) => y + 1,
  shorthand(z: number) { return z - 1; },
};
class Calculator {
  multiply = (x: number) => x * 2;
}
"#,
            LangId::TypeScript,
        );

        for name in ["compute", "doCompute", "arrow", "shorthand"] {
            let symbol = symbols.iter().find(|symbol| symbol.name == name).unwrap();
            assert_eq!(symbol.parent.as_deref(), Some("helper"), "{name}");
            assert_eq!(symbol.scope_chain, vec!["helper"], "{name}");
        }
        let compute = symbols
            .iter()
            .find(|symbol| symbol.name == "compute")
            .unwrap();
        assert_eq!(compute.kind, SymbolKind::Method);
        assert!(compute
            .signature
            .as_deref()
            .unwrap()
            .starts_with("compute:"));
        let alias = symbols
            .iter()
            .find(|symbol| symbol.name == "doCompute")
            .unwrap();
        assert_eq!(alias.kind, SymbolKind::Function);
        assert!(alias
            .signature
            .as_deref()
            .unwrap()
            .starts_with("function doCompute"));

        let class_field = symbols
            .iter()
            .find(|symbol| symbol.name == "multiply")
            .unwrap();
        assert_eq!(class_field.kind, SymbolKind::Method);
        assert_eq!(class_field.parent.as_deref(), Some("Calculator"));
    }

    #[test]
    fn tsx_extracts_interface_and_object_property_callables() {
        let symbols = symbols_from_source(
            r#"interface Props {
  render(): JSX.Element;
  onSelect: (value: number) => void;
}
const helpers = {
  render: () => <div />,
};
"#,
            LangId::Tsx,
        );

        assert!(symbols.iter().any(|symbol| {
            symbol.name == "onSelect" && symbol.parent.as_deref() == Some("Props")
        }));
        assert!(symbols.iter().any(|symbol| {
            symbol.name == "render" && symbol.parent.as_deref() == Some("Props")
        }));
        assert!(symbols.iter().any(|symbol| {
            symbol.name == "render" && symbol.parent.as_deref() == Some("helpers")
        }));
    }

    #[test]
    fn ts_signatures_present() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        let greet_sig = find("greet").signature.as_ref().unwrap();
        assert!(
            greet_sig.contains("greet"),
            "signature should contain function name: {}",
            greet_sig
        );
    }

    #[test]
    fn ts_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- JavaScript extraction ---

    #[test]
    fn js_extracts_core_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.js")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"multiply"),
            "missing function multiply: {:?}",
            names
        );
        assert!(
            names.contains(&"divide"),
            "missing arrow fn divide: {:?}",
            names
        );
        assert!(
            names.contains(&"EventEmitter"),
            "missing class EventEmitter: {:?}",
            names
        );
        assert!(
            names.contains(&"main"),
            "missing default export fn main: {:?}",
            names
        );

        assert!(
            symbols.len() >= 4,
            "expected ≥4 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn js_arrow_fn_correctly_named() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.js")).unwrap();

        let divide = symbols.iter().find(|s| s.name == "divide").unwrap();
        assert_eq!(divide.kind, SymbolKind::Function);
        assert!(divide.exported, "divide should be exported");

        let internal = symbols.iter().find(|s| s.name == "internalUtil").unwrap();
        assert_eq!(internal.kind, SymbolKind::Function);
        assert!(!internal.exported, "internalUtil should not be exported");
    }

    #[test]
    fn js_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.js")).unwrap();

        let methods: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();

        for method in &methods {
            assert_eq!(
                method.scope_chain,
                vec!["EventEmitter"],
                "method {} should have EventEmitter in scope chain",
                method.name
            );
        }
    }

    // --- TSX extraction ---

    #[test]
    fn tsx_extracts_react_component() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.tsx")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Button"),
            "missing React component Button: {:?}",
            names
        );
        assert!(
            names.contains(&"Counter"),
            "missing class Counter: {:?}",
            names
        );
        assert!(
            names.contains(&"formatLabel"),
            "missing function formatLabel: {:?}",
            names
        );

        assert!(
            symbols.len() >= 2,
            "expected ≥2 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn tsx_jsx_doesnt_break_parser() {
        // Main assertion: TSX grammar handles JSX without errors
        let provider = TreeSitterProvider::new();
        let result = provider.list_symbols(&fixture_path("sample.tsx"));
        assert!(
            result.is_ok(),
            "TSX parsing should succeed: {:?}",
            result.err()
        );
    }

    // --- resolve_symbol ---

    #[test]
    fn resolve_symbol_finds_match() {
        let provider = TreeSitterProvider::new();
        let matches = provider
            .resolve_symbol(&fixture_path("sample.ts"), "greet")
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].symbol.name, "greet");
        assert_eq!(matches[0].symbol.kind, SymbolKind::Function);
    }

    #[test]
    fn resolve_symbol_not_found() {
        let provider = TreeSitterProvider::new();
        let result = provider.resolve_symbol(&fixture_path("sample.ts"), "nonexistent");
        assert!(matches!(result, Err(AftError::SymbolNotFound { .. })));
    }

    #[test]
    fn resolve_symbol_follows_reexport_chains() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.ts");
        let barrel1 = dir.path().join("barrel1.ts");
        let barrel2 = dir.path().join("barrel2.ts");
        let barrel3 = dir.path().join("barrel3.ts");
        let index = dir.path().join("index.ts");

        std::fs::write(
            &config,
            "export class Config {}\nexport default class DefaultConfig {}\n",
        )
        .unwrap();
        std::fs::write(
            &barrel1,
            "export { Config } from './config';\nexport { default as NamedDefault } from './config';\n",
        )
        .unwrap();
        std::fs::write(
            &barrel2,
            "export { Config as RenamedConfig } from './barrel1';\n",
        )
        .unwrap();
        std::fs::write(
            &barrel3,
            "export * from './barrel2';\nexport * from './barrel1';\n",
        )
        .unwrap();
        std::fs::write(
            &index,
            "export class Config {}\nexport { RenamedConfig as FinalConfig } from './barrel3';\nexport * from './barrel3';\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let config_canon = std::fs::canonicalize(&config).unwrap();

        let direct = provider.resolve_symbol(&barrel1, "Config").unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].symbol.name, "Config");
        assert_eq!(direct[0].file, config_canon.display().to_string());

        let renamed = provider.resolve_symbol(&barrel2, "RenamedConfig").unwrap();
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].symbol.name, "Config");
        assert_eq!(renamed[0].file, config_canon.display().to_string());

        let wildcard_chain = provider.resolve_symbol(&index, "FinalConfig").unwrap();
        assert_eq!(wildcard_chain.len(), 1);
        assert_eq!(wildcard_chain[0].symbol.name, "Config");
        assert_eq!(wildcard_chain[0].file, config_canon.display().to_string());

        let wildcard_default = provider.resolve_symbol(&index, "NamedDefault").unwrap();
        assert_eq!(wildcard_default.len(), 1);
        assert_eq!(wildcard_default[0].symbol.name, "DefaultConfig");
        assert_eq!(wildcard_default[0].file, config_canon.display().to_string());

        let local = provider.resolve_symbol(&index, "Config").unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].symbol.name, "Config");
        assert_eq!(local[0].file, index.display().to_string());
    }

    // --- Parse tree caching ---

    #[test]
    fn symbol_range_includes_rust_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_attrs.rs");
        std::fs::write(
            &path,
            "/// This is a doc comment\n#[test]\n#[cfg(test)]\nfn my_test_fn() {\n    assert!(true);\n}\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let matches = provider.resolve_symbol(&path, "my_test_fn").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].symbol.range.start_line, 0,
            "symbol range should include preceding /// doc comment, got start_line={}",
            matches[0].symbol.range.start_line
        );
    }

    #[test]
    fn symbol_range_includes_go_doc_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_doc.go");
        std::fs::write(
            &path,
            "package main\n\n// MyFunc does something useful.\n// It has a multi-line doc.\nfunc MyFunc() {\n}\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let matches = provider.resolve_symbol(&path, "MyFunc").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].symbol.range.start_line, 2,
            "symbol range should include preceding doc comments, got start_line={}",
            matches[0].symbol.range.start_line
        );
    }

    #[test]
    fn symbol_range_skips_unrelated_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_gap.go");
        std::fs::write(
            &path,
            "package main\n\n// This is a standalone comment\n\nfunc Standalone() {\n}\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let matches = provider.resolve_symbol(&path, "Standalone").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].symbol.range.start_line, 4,
            "symbol range should NOT include comment separated by blank line, got start_line={}",
            matches[0].symbol.range.start_line
        );
    }

    #[test]
    fn parse_cache_returns_same_tree() {
        let mut parser = FileParser::new();
        let path = fixture_path("sample.ts");

        let (tree1, _) = parser.parse(&path).unwrap();
        let tree1_root = tree1.root_node().byte_range();

        let (tree2, _) = parser.parse(&path).unwrap();
        let tree2_root = tree2.root_node().byte_range();

        // Same tree (cache hit) should return identical root node range
        assert_eq!(tree1_root, tree2_root);
    }

    #[test]
    fn extract_symbols_from_tree_matches_list_symbols() {
        let path = fixture_path("sample.rs");
        let source = std::fs::read_to_string(&path).unwrap();

        let provider = TreeSitterProvider::new();
        let listed = provider.list_symbols(&path).unwrap();

        let mut parser = FileParser::new();
        let (tree, lang) = parser.parse(&path).unwrap();
        let mut extracted = extract_symbols_from_tree(&source, tree, lang).unwrap();
        // Navigation adds Rust trait members after the tree walk, and the
        // stored records keep the walk's table, so complete it the same way.
        complete_rust_trait_members(
            &mut extracted,
            crate::callgraph::rust_trait_members(&source, tree.root_node(), Direct),
        );

        assert_eq!(symbols_as_debug(&extracted), symbols_as_debug(&listed));
    }

    fn symbols_as_debug(symbols: &[Symbol]) -> Vec<String> {
        symbols
            .iter()
            .map(|symbol| {
                format!(
                    "{}|{:?}|{}:{}-{}:{}|{:?}|{:?}|{}|{:?}",
                    symbol.name,
                    symbol.kind,
                    symbol.range.start_line,
                    symbol.range.start_col,
                    symbol.range.end_line,
                    symbol.range.end_col,
                    symbol.signature,
                    symbol.scope_chain,
                    symbol.exported,
                    symbol.parent,
                )
            })
            .collect()
    }

    // --- Python extraction ---

    #[test]
    fn py_extracts_all_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"top_level_function"),
            "missing top_level_function: {:?}",
            names
        );
        assert!(names.contains(&"MyClass"), "missing MyClass: {:?}", names);
        assert!(
            names.contains(&"instance_method"),
            "missing method instance_method: {:?}",
            names
        );
        assert!(
            names.contains(&"decorated_function"),
            "missing decorated_function: {:?}",
            names
        );

        // Plan requires ≥4 symbols
        assert!(
            symbols.len() >= 4,
            "expected ≥4 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn py_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("top_level_function").kind, SymbolKind::Function);
        assert_eq!(find("MyClass").kind, SymbolKind::Class);
        assert_eq!(find("instance_method").kind, SymbolKind::Method);
        assert_eq!(find("decorated_function").kind, SymbolKind::Function);
        assert_eq!(find("OuterClass").kind, SymbolKind::Class);
        assert_eq!(find("InnerClass").kind, SymbolKind::Class);
        assert_eq!(find("inner_method").kind, SymbolKind::Method);
        assert_eq!(find("outer_method").kind, SymbolKind::Method);
    }

    #[test]
    fn py_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        // Method inside MyClass
        assert_eq!(
            find("instance_method").scope_chain,
            vec!["MyClass"],
            "instance_method should have MyClass in scope chain"
        );
        assert_eq!(find("instance_method").parent.as_deref(), Some("MyClass"));

        // Method inside OuterClass > InnerClass
        assert_eq!(
            find("inner_method").scope_chain,
            vec!["OuterClass", "InnerClass"],
            "inner_method should have nested scope chain"
        );

        // InnerClass itself should have OuterClass in scope
        assert_eq!(
            find("InnerClass").scope_chain,
            vec!["OuterClass"],
            "InnerClass should have OuterClass in scope"
        );

        // Top-level function has empty scope
        assert!(
            find("top_level_function").scope_chain.is_empty(),
            "top-level function should have empty scope chain"
        );
    }

    #[test]
    fn py_decorated_function_signature() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        let sig = find("decorated_function").signature.as_ref().unwrap();
        assert!(
            sig.contains("@staticmethod"),
            "decorated function signature should include decorator: {}",
            sig
        );
        assert!(
            sig.contains("def decorated_function"),
            "signature should include function def: {}",
            sig
        );
    }

    #[test]
    fn py_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- Rust extraction ---

    #[test]
    fn rs_extracts_all_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"public_function"),
            "missing public_function: {:?}",
            names
        );
        assert!(
            names.contains(&"private_function"),
            "missing private_function: {:?}",
            names
        );
        assert!(names.contains(&"MyStruct"), "missing MyStruct: {:?}", names);
        assert!(names.contains(&"Color"), "missing enum Color: {:?}", names);
        assert!(
            names.contains(&"Drawable"),
            "missing trait Drawable: {:?}",
            names
        );
        // impl methods
        assert!(
            names.contains(&"new"),
            "missing impl method new: {:?}",
            names
        );

        // Plan requires ≥6 symbols
        assert!(
            symbols.len() >= 6,
            "expected ≥6 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn rs_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("public_function").kind, SymbolKind::Function);
        assert_eq!(find("private_function").kind, SymbolKind::Function);
        assert_eq!(find("MyStruct").kind, SymbolKind::Struct);
        assert_eq!(find("Color").kind, SymbolKind::Enum);
        assert_eq!(find("Drawable").kind, SymbolKind::Interface); // trait → Interface
        assert_eq!(find("new").kind, SymbolKind::Method);
    }

    #[test]
    fn rs_pub_export_detection() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert!(
            find("public_function").exported,
            "pub fn should be exported"
        );
        assert!(
            !find("private_function").exported,
            "non-pub fn should not be exported"
        );
        assert!(find("MyStruct").exported, "pub struct should be exported");
        assert!(find("Color").exported, "pub enum should be exported");
        assert!(find("Drawable").exported, "pub trait should be exported");
        assert!(
            find("new").exported,
            "pub fn inside impl should be exported"
        );
        assert!(
            !find("helper").exported,
            "non-pub fn inside impl should not be exported"
        );
    }

    #[test]
    fn rs_impl_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        // `impl MyStruct { fn new() }` → scope chain = ["MyStruct"]
        assert_eq!(
            find("new").scope_chain,
            vec!["MyStruct"],
            "impl method should have type in scope chain"
        );
        assert_eq!(find("new").parent.as_deref(), Some("MyStruct"));

        // Free function has empty scope chain
        assert!(
            find("public_function").scope_chain.is_empty(),
            "free function should have empty scope chain"
        );
    }

    #[test]
    fn rs_trait_impl_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        // `impl Drawable for MyStruct { fn draw() }` → scope = ["Drawable for MyStruct"]
        let draw = symbols.iter().find(|s| s.name == "draw").unwrap();
        assert_eq!(
            draw.scope_chain,
            vec!["Drawable for MyStruct"],
            "trait impl method should have 'Trait for Type' scope"
        );
        assert_eq!(draw.parent.as_deref(), Some("MyStruct"));
    }

    #[test]
    fn rs_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    /// Source text of the whole lines a symbol's range covers.
    fn symbol_lines<'a>(source: &'a str, symbol: &Symbol) -> Vec<&'a str> {
        source
            .lines()
            .skip(symbol.range.start_line as usize)
            .take((symbol.range.end_line - symbol.range.start_line + 1) as usize)
            .collect()
    }

    fn only_symbol<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        let matches: Vec<&Symbol> = symbols.iter().filter(|s| s.name == name).collect();
        assert_eq!(matches.len(), 1, "expected one `{name}` in {symbols:#?}");
        matches[0]
    }

    /// Shaped like a runtime split across `macro_rules!` definitions whose
    /// single `() => { ... }` arm holds concrete methods and items, expanded
    /// elsewhere by an empty invocation.
    const RUST_MACRO_RUNTIME_SOURCE: &str = r#"//! Runtime declarations owned by one domain.

macro_rules! domain_runtime_methods {
    () => {
/// Stable ingress port.
    pub fn domain_ingress(
        &self,
        session_id: &str,
    ) -> Result<(), String> {
        self.domain_record(session_id)
    }

    fn domain_record(&self, session_id: &str) -> Result<(), String> {
        let _ = session_id;
        Ok(())
    }
    };
}

macro_rules! domain_runtime_items {
    () => {
const DOMAIN_LIMIT: u32 = 40;

#[derive(Debug)]
pub enum DomainTerminal {
    Completed,
}

pub fn domain_limit() -> u32 { DOMAIN_LIMIT }
    };
}

pub fn outside_macros() {}
"#;

    #[test]
    fn rs_macro_rules_definition_bodies_contribute_their_items() {
        let source = RUST_MACRO_RUNTIME_SOURCE;
        let symbols = symbols_from_source(source, LangId::Rust);

        let ingress = only_symbol(&symbols, "domain_ingress");
        assert_eq!(ingress.kind, SymbolKind::Function);
        assert!(ingress.exported);
        assert_eq!(
            symbol_lines(source, ingress),
            [
                "/// Stable ingress port.",
                "    pub fn domain_ingress(",
                "        &self,",
                "        session_id: &str,",
                "    ) -> Result<(), String> {",
                "        self.domain_record(session_id)",
                "    }",
            ]
        );

        let record = only_symbol(&symbols, "domain_record");
        assert!(!record.exported);
        assert_eq!((record.range.start_line, record.range.end_line), (12, 15));

        let terminal = only_symbol(&symbols, "DomainTerminal");
        assert_eq!(terminal.kind, SymbolKind::Enum);
        assert_eq!(
            (terminal.range.start_line, terminal.range.end_line),
            (23, 26),
            "the derive attribute belongs to the enum's range"
        );
        assert_eq!(
            symbol_lines(source, only_symbol(&symbols, "domain_limit")),
            ["pub fn domain_limit() -> u32 { DOMAIN_LIMIT }"]
        );
        only_symbol(&symbols, "outside_macros");

        let order: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            order,
            [
                "domain_ingress",
                "domain_record",
                "DomainTerminal",
                "domain_limit",
                "outside_macros"
            ],
            "symbols stay in source order"
        );
    }

    #[test]
    fn rs_item_position_macro_invocation_contributes_its_items() {
        let source = r#"declare_handlers! {
    pub fn handle_open(path: &str) -> bool {
        !path.is_empty()
    }

    pub struct HandlerState;
}

impl Service {
    service_methods! { fn service_tick(&self) {} }
}

fn body_macros_are_not_items() {
    helper! { fn not_an_item() {} }
}
"#;
        let symbols = symbols_from_source(source, LangId::Rust);

        assert_eq!(
            symbol_lines(source, only_symbol(&symbols, "handle_open")),
            [
                "    pub fn handle_open(path: &str) -> bool {",
                "        !path.is_empty()",
                "    }",
            ]
        );
        assert_eq!(
            only_symbol(&symbols, "HandlerState").kind,
            SymbolKind::Struct
        );
        let tick = only_symbol(&symbols, "service_tick");
        assert_eq!(tick.range.start_line, 9);
        assert_eq!(
            tick.range.start_col, 23,
            "columns map back to the original line"
        );
        assert!(
            symbols.iter().all(|s| s.name != "not_an_item"),
            "macros inside function bodies are not item positions: {symbols:#?}"
        );
    }

    #[test]
    fn rs_macro_template_items_named_by_metavariables_are_skipped() {
        let source = r#"macro_rules! make_getter {
    ($name:ident, $ty:ty) => {
        pub fn $name(&self) -> $ty { self.value }

        pub fn fixed_name_helper() -> $ty { Default::default() }
    };
}
"#;
        let symbols = symbols_from_source(source, LangId::Rust);

        assert!(
            symbols.iter().all(|s| !s.name.starts_with('$')),
            "metavariable names are placeholders, not symbols: {symbols:#?}"
        );
        assert_eq!(
            symbol_lines(source, only_symbol(&symbols, "fixed_name_helper")),
            ["        pub fn fixed_name_helper() -> $ty { Default::default() }"]
        );
    }

    #[test]
    fn rs_unparseable_macro_body_leaves_outline_unchanged() {
        let base = "pub struct Kept;\n\npub fn kept_function() {}\n";
        let with_bad_macros = format!(
            "{base}\nweird! {{ fn hidden_good() {{}} fn broken( ) -> }}\n\nmacro_rules! repeated {{\n    ($($name:ident),*) => {{ $(fn $name() {{}})* }};\n}}\n"
        );

        let base_symbols = symbols_from_source(base, LangId::Rust);
        let symbols = symbols_from_source(&with_bad_macros, LangId::Rust);
        assert_eq!(
            rust_symbol_fingerprint(&symbols),
            rust_symbol_fingerprint(&base_symbols),
            "a body that does not parse contributes nothing, not even its good items"
        );
    }

    #[test]
    fn rs_nested_macro_bodies_survive_deep_nesting_on_a_small_stack() {
        const DEPTH: usize = 300;
        let mut source = String::new();
        for level in 0..DEPTH {
            source.push_str(&format!("wrap{level}! {{\nfn level_{level}() {{}}\n"));
        }
        for _ in 0..DEPTH {
            source.push_str("}\n");
        }

        let symbols = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || symbols_from_source(&source, LangId::Rust))
            .unwrap()
            .join()
            .expect("nested macro bodies must not overflow the stack");

        assert_eq!(symbols.len(), DEPTH);
        for (level, symbol) in symbols.iter().enumerate() {
            assert_eq!(symbol.name, format!("level_{level}"));
            assert_eq!(symbol.range.start_line as usize, level * 2 + 1);
        }
    }

    // --- Go extraction ---

    #[test]
    fn go_package_variables_and_constants_are_symbols() {
        let source = "package main\nvar Boot = Register()\nvar (\n A, b = 1, 2\n)\nconst Answer = 42\nfunc Register() int { var local = 1; return local }\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vars.go");
        std::fs::write(&path, source).unwrap();
        let symbols = TreeSitterProvider::new().list_symbols(&path).unwrap();
        for name in ["Boot", "A", "b", "Answer"] {
            let symbol = symbols.iter().find(|s| s.name == name).expect(name);
            assert_eq!(symbol.kind, SymbolKind::Variable);
            assert_eq!(symbol.exported, name != "b");
        }
        assert!(!symbols.iter().any(|s| s.name == "local"));
    }

    #[test]
    fn go_extracts_all_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"ExportedFunction"),
            "missing ExportedFunction: {:?}",
            names
        );
        assert!(
            names.contains(&"unexportedFunction"),
            "missing unexportedFunction: {:?}",
            names
        );
        assert!(
            names.contains(&"MyStruct"),
            "missing struct MyStruct: {:?}",
            names
        );
        assert!(
            names.contains(&"Reader"),
            "missing interface Reader: {:?}",
            names
        );
        // receiver method
        assert!(
            names.contains(&"String"),
            "missing receiver method String: {:?}",
            names
        );

        // Plan requires ≥4 symbols
        assert!(
            symbols.len() >= 4,
            "expected ≥4 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn go_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("ExportedFunction").kind, SymbolKind::Function);
        assert_eq!(find("unexportedFunction").kind, SymbolKind::Function);
        assert_eq!(find("MyStruct").kind, SymbolKind::Struct);
        assert_eq!(find("Reader").kind, SymbolKind::Interface);
        assert_eq!(find("String").kind, SymbolKind::Method);
        assert_eq!(find("helper").kind, SymbolKind::Method);
    }

    #[test]
    fn go_uppercase_export_detection() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert!(
            find("ExportedFunction").exported,
            "ExportedFunction (uppercase) should be exported"
        );
        assert!(
            !find("unexportedFunction").exported,
            "unexportedFunction (lowercase) should not be exported"
        );
        assert!(
            find("MyStruct").exported,
            "MyStruct (uppercase) should be exported"
        );
        assert!(
            find("Reader").exported,
            "Reader (uppercase) should be exported"
        );
        assert!(
            find("String").exported,
            "String method (uppercase) should be exported"
        );
        assert!(
            !find("helper").exported,
            "helper method (lowercase) should not be exported"
        );
    }

    #[test]
    fn go_receiver_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        // `func (m *MyStruct) String()` → scope chain = ["MyStruct"]
        assert_eq!(
            find("String").scope_chain,
            vec!["MyStruct"],
            "receiver method should have type in scope chain"
        );
        assert_eq!(find("String").parent.as_deref(), Some("MyStruct"));

        // Regular function has empty scope chain
        assert!(
            find("ExportedFunction").scope_chain.is_empty(),
            "regular function should have empty scope chain"
        );
    }

    #[test]
    fn go_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- Cross-language ---

    #[test]
    fn cross_language_all_six_produce_symbols() {
        let provider = TreeSitterProvider::new();

        let fixtures = [
            ("sample.ts", "TypeScript"),
            ("sample.tsx", "TSX"),
            ("sample.js", "JavaScript"),
            ("sample.py", "Python"),
            ("sample.rs", "Rust"),
            ("sample.go", "Go"),
        ];

        for (fixture, lang) in &fixtures {
            let symbols = provider
                .list_symbols(&fixture_path(fixture))
                .unwrap_or_else(|e| panic!("{} ({}) failed: {:?}", lang, fixture, e));
            assert!(
                symbols.len() >= 2,
                "{} should produce ≥2 symbols, got {}: {:?}",
                lang,
                symbols.len(),
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }
    }

    // --- Symbol cache tests ---

    fn cached_freshness_fixture(
        content: &str,
    ) -> (tempfile::TempDir, PathBuf, SystemTime, u64, blake3::Hash) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, content).unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let metadata = std::fs::metadata(&file).unwrap();
        let mtime = metadata.modified().unwrap();
        let size = metadata.len();
        let hash = cache_freshness::hash_bytes(content.as_bytes());
        (dir, file, mtime, size, hash)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_unchanged_metadata_skips_hashing() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn hello() {}\n");

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 0);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_changed_mtime_with_identical_content_hashes_fresh() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn hello() {}\n");
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_changed_mtime_and_same_size_content_hashes_stale() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn one() {}\n");
        std::fs::write(&file, "pub fn two() {}\n").unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().len(), size);

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(!cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_changed_size_skips_hashing() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn hello() {}\n");
        std::fs::write(&file, "pub fn hello() {}\n// longer\n").unwrap();

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(!cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 0);
    }

    #[test]
    fn heading_columns_index_many_sections_with_mixed_endings() {
        let source = (0..5_000)
            .map(|i| format!("# Heading {i}\r\nbody é {i}\r"))
            .collect::<Vec<_>>()
            .join("\n");
        HEADING_COLUMN_LINE_VISITS.with(|visits| visits.set(0));
        let cols = source_line_end_cols(&source);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headings.md");
        std::fs::write(&path, &source).unwrap();
        let mut parser = FileParser::new();
        let headings = parser.extract_symbols(&path).unwrap();
        assert_eq!(headings.len(), 5_000);
        for (i, heading) in headings.iter().enumerate() {
            let line = heading.range.end_line as usize;
            let expected_start = i * 2;
            let expected_end = if i + 1 == headings.len() {
                source.lines().count() - 1
            } else {
                expected_start + 1
            };
            assert_eq!(heading.range.start_line as usize, expected_start);
            assert_eq!(line, expected_end);
            assert_eq!(heading.range.start_col, 0);
            assert_eq!(
                heading.range.end_col,
                source.lines().nth(expected_end).unwrap().len() as u32
            );
        }
        assert_eq!(cols.len(), source.lines().count());
        for (i, line) in source.lines().enumerate() {
            assert_eq!(cols[i], line.len() as u32);
        }
        let old_line_visits: usize = (0..5_000)
            .map(|i| source.lines().take(i * 2 + 2).count())
            .sum();
        let indexed_visits = HEADING_COLUMN_LINE_VISITS.with(|visits| visits.get());
        assert_eq!(indexed_visits, cols.len() * 2);
        assert!(old_line_visits > indexed_visits * 1_000);
        eprintln!("heading line visits: baseline {old_line_visits}, indexed {indexed_visits}");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn touched_symbol_cache_rehashes_only_first_query() {
        let (_dir, file, _, _, _) = cached_freshness_fixture("pub fn hello() {}\n");
        let mut parser = FileParser::new();
        parser.extract_symbols(&file).unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();
        cache_freshness::reset_hash_file_if_small_count_for_debug();
        for _ in 0..100 {
            assert_eq!(parser.extract_symbols(&file).unwrap()[0].name, "hello");
        }
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 1);
    }

    #[test]
    fn touched_cache_readers_do_not_hold_lock_and_observe_replacement() {
        use std::sync::{Arc, Barrier};
        let (_dir, file, _, _, _) = cached_freshness_fixture("pub fn one() {}\n");
        let mut initial = FileParser::new();
        initial.extract_symbols(&file).unwrap();
        let cache = initial.symbol_cache();
        let arrived = Arc::new(Barrier::new(3));
        let released = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let (file, cache, arrived, released) = (
                    file.clone(),
                    cache.clone(),
                    arrived.clone(),
                    released.clone(),
                );
                std::thread::spawn(move || {
                    FRESHNESS_TEST_BARRIERS
                        .with(|slot| *slot.borrow_mut() = Some((arrived, released)));
                    FileParser::with_symbol_cache(cache)
                        .extract_symbols(&file)
                        .unwrap()[0]
                        .name
                        .clone()
                })
            })
            .collect::<Vec<_>>();
        arrived.wait();
        let unlocked = cache.try_write().is_ok();
        if !unlocked {
            released.wait();
            for worker in workers {
                worker.join().unwrap();
            }
            panic!("disk freshness checks must not hold the cache lock");
        }
        std::fs::write(&file, "pub fn two() {}\n").unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(3, 0)).unwrap();
        let mut replacement = FileParser::with_symbol_cache(cache);
        assert_eq!(replacement.extract_symbols(&file).unwrap()[0].name, "two");
        released.wait();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), "two");
        }
    }

    #[test]
    fn touched_cache_refresh_retries_after_concurrent_replacement() {
        use std::sync::{Arc, Barrier};
        let (_dir, file, _, _, _) = cached_freshness_fixture("pub fn one() {}\n");
        let mut initial = FileParser::new();
        initial.extract_symbols(&file).unwrap();
        let cache = initial.symbol_cache();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();
        let arrived = Arc::new(Barrier::new(3));
        let released = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let (file, cache, arrived, released) = (
                    file.clone(),
                    cache.clone(),
                    arrived.clone(),
                    released.clone(),
                );
                std::thread::spawn(move || {
                    CACHE_REFRESH_TEST_BARRIERS
                        .with(|slot| *slot.borrow_mut() = Some((arrived, released)));
                    FileParser::with_symbol_cache(cache)
                        .extract_symbols(&file)
                        .unwrap()[0]
                        .name
                        .clone()
                })
            })
            .collect::<Vec<_>>();
        arrived.wait();
        let unlocked = cache.try_write().is_ok();
        if !unlocked {
            released.wait();
            for worker in workers {
                worker.join().unwrap();
            }
            panic!("hashing must not hold the cache lock");
        }
        std::fs::write(&file, "pub fn two() {}\n").unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(3, 0)).unwrap();
        assert_eq!(
            FileParser::with_symbol_cache(cache)
                .extract_symbols(&file)
                .unwrap()[0]
                .name,
            "two"
        );
        released.wait();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), "two");
        }
    }

    #[test]
    fn symbol_cache_returns_cached_results_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "pub fn hello() {}\npub fn world() {}").unwrap();

        let mut parser = FileParser::new();

        let symbols1 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols1.len(), 2);

        // Second call should return cached result
        let symbols2 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols2.len(), 2);
        assert_eq!(symbols1[0].name, symbols2[0].name);

        // Verify cache is populated
        assert!(parser.symbol_cache.read().unwrap().contains_key(&file));
    }

    #[test]
    fn symbol_cache_invalidates_on_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "pub fn hello() {}").unwrap();

        let mut parser = FileParser::new();

        let symbols1 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols1.len(), 1);
        assert_eq!(symbols1[0].name, "hello");

        // Wait to ensure mtime changes (filesystem resolution can be 1s on some OS)
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Modify file — add a second function
        std::fs::write(&file, "pub fn hello() {}\npub fn goodbye() {}").unwrap();

        // Should detect mtime change and re-extract
        let symbols2 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols2.len(), 2);
        assert!(symbols2.iter().any(|s| s.name == "goodbye"));
    }

    #[test]
    fn symbol_cache_invalidate_method_clears_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "pub fn hello() {}").unwrap();

        let mut parser = FileParser::new();
        parser.extract_symbols(&file).unwrap();
        assert!(parser.symbol_cache.read().unwrap().contains_key(&file));

        parser.invalidate_symbols(&file);
        assert!(!parser.symbol_cache.read().unwrap().contains_key(&file));
        // Parse tree cache should also be cleared
        assert!(!parser.cache.contains_key(&file));
    }

    #[test]
    fn symbol_cache_works_for_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        let rs_file = dir.path().join("lib.rs");
        let ts_file = dir.path().join("app.ts");
        let py_file = dir.path().join("main.py");

        std::fs::write(&rs_file, "pub fn rust_fn() {}").unwrap();
        std::fs::write(&ts_file, "export function tsFn() {}").unwrap();
        std::fs::write(&py_file, "def py_fn():\n    pass").unwrap();

        let mut parser = FileParser::new();

        let rs_syms = parser.extract_symbols(&rs_file).unwrap();
        let ts_syms = parser.extract_symbols(&ts_file).unwrap();
        let py_syms = parser.extract_symbols(&py_file).unwrap();

        assert!(rs_syms.iter().any(|s| s.name == "rust_fn"));
        assert!(ts_syms.iter().any(|s| s.name == "tsFn"));
        assert!(py_syms.iter().any(|s| s.name == "py_fn"));

        // All should be cached now
        assert_eq!(parser.symbol_cache.read().unwrap().len(), 3);

        // Re-extract should return same results from cache
        let rs_syms2 = parser.extract_symbols(&rs_file).unwrap();
        assert_eq!(rs_syms.len(), rs_syms2.len());
    }

    /// Minified bundles put deeply nested functions on one line. A symbol's
    /// signature is the first line of its definition, so on a single-line file
    /// every nested symbol used to carry the whole remainder of the file and the
    /// serialized symbol payload grew with nesting depth times file size. This
    /// fixture is a ~410 KiB file that produced a ~20 MiB payload, larger than
    /// the 16 MiB per-file limit of the on-disk symbol cache.
    #[test]
    fn single_line_nested_functions_keep_symbol_payload_linear() {
        const DEPTH: usize = 100;
        let padding = "x".repeat(4096);
        let mut source = String::new();
        for level in 0..DEPTH {
            source.push_str(&format!("function f{level}(){{var p{level}=\"{padding}\";"));
        }
        for _ in 0..DEPTH {
            source.push('}');
        }
        source.push('\n');

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bundle.min.js");
        std::fs::write(&file, &source).unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();
        let payload = serde_json::to_vec(&symbols).unwrap();
        eprintln!(
            "single-line fixture: file={} bytes symbols={} payload={} bytes",
            source.len(),
            symbols.len(),
            payload.len()
        );

        assert_eq!(
            symbols
                .iter()
                .filter(|symbol| symbol.kind == SymbolKind::Function)
                .count(),
            DEPTH
        );
        assert!(symbols.iter().all(|symbol| symbol
            .signature
            .as_deref()
            .is_none_or(|signature| signature.len() <= MAX_SIGNATURE_BYTES + '…'.len_utf8())));
        assert!(
            payload.len() < source.len(),
            "symbol payload {} bytes should stay below the {} byte source",
            payload.len(),
            source.len()
        );
    }

    #[test]
    fn extract_json_symbols_top_level_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.json");
        std::fs::write(&file, r#"{"name": "x", "version": "1"}"#).unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 2);
        assert!(symbols
            .iter()
            .any(|s| s.name == "name" && s.kind == SymbolKind::Variable));
        assert!(symbols
            .iter()
            .any(|s| s.name == "version" && s.kind == SymbolKind::Variable));
    }

    #[test]
    fn extract_json_symbols_root_array() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("array.json");
        std::fs::write(&file, "[1,2,3]").unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 0);
    }

    #[test]
    fn extract_json_symbols_no_recursion_into_nested() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested.json");
        std::fs::write(&file, r#"{"scripts": {"build": "tsc"}}"#).unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "scripts");
        assert!(!symbols.iter().any(|s| s.name == "build"));
    }

    #[test]
    fn extract_scala_symbols_object_and_method() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Greeter.scala");
        std::fs::write(
            &file,
            "object Greeter {\n  def hello(name: String): String = s\"hi $name\"\n}",
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert!(symbols
            .iter()
            .any(|s| s.name == "Greeter" && s.kind == SymbolKind::Class));
        assert!(symbols.iter().any(|s| s.name == "hello"
            && s.kind == SymbolKind::Method
            && s.scope_chain == vec!["Greeter".to_string()]));
    }

    #[test]
    fn extract_scala_symbols_class_and_trait() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Types.scala");
        std::fs::write(&file, "class Foo\ntrait Bar").unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert!(symbols
            .iter()
            .any(|s| s.name == "Foo" && s.kind == SymbolKind::Class));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Bar" && s.kind == SymbolKind::Interface));
    }

    #[test]
    fn extract_yaml_symbols_k8s_resource() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deployment.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
  namespace: web
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for K8s Deployment");
        let sym = &symbols[0];
        assert_eq!(sym.name, "web/Deployment/nginx");
        assert_eq!(sym.kind, SymbolKind::Class);
        assert!(sym.exported);
    }

    #[test]
    fn extract_yaml_symbols_k8s_no_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("service.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: v1
kind: Service
metadata:
  name: nginx-svc
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for K8s Service");
        let sym = &symbols[0];
        assert_eq!(sym.name, "Service/nginx-svc");
        assert_eq!(sym.kind, SymbolKind::Class);
    }

    #[test]
    fn extract_yaml_symbols_multidoc() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("multidoc.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: a
---
apiVersion: v1
kind: Service
metadata:
  name: b
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 2, "Expected 2 symbols for multi-doc YAML");
        assert!(symbols.iter().any(|s| s.name == "Deployment/a"));
        assert!(symbols.iter().any(|s| s.name == "Service/b"));
    }

    #[test]
    fn extract_yaml_symbols_generic_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("compose.yaml");
        std::fs::write(
            &file,
            r#"version: "3"
services:
  web: {}
volumes:
  data: {}
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        // Should have top-level keys: version, services, volumes
        assert_eq!(symbols.len(), 3, "Expected 3 symbols for generic YAML");
        assert!(symbols
            .iter()
            .any(|s| s.name == "version" && s.kind == SymbolKind::Variable));
        assert!(symbols
            .iter()
            .any(|s| s.name == "services" && s.kind == SymbolKind::Variable));
        assert!(symbols
            .iter()
            .any(|s| s.name == "volumes" && s.kind == SymbolKind::Variable));
    }

    #[test]
    fn extract_yaml_symbols_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.yaml");
        std::fs::write(&file, "").unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 0, "Expected 0 symbols for empty YAML");
    }

    #[test]
    fn extract_yaml_symbols_resource_limits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deployment.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
spec:
  template:
    spec:
      containers:
      - name: main
        image: myapp:1.0
        resources:
          limits:
            cpu: "2"
            memory: 1Gi
          requests:
            cpu: "1"
            memory: 512Mi
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Deployment");
        let sym = &symbols[0];
        assert_eq!(sym.name, "Deployment/app");
        let sig = sym.signature.as_ref().unwrap();
        assert!(sig.contains("cpu="), "Signature should contain cpu= field");
        assert!(
            sig.contains("memory="),
            "Signature should contain memory= field"
        );
    }

    #[test]
    fn extract_yaml_symbols_env_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deployment.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
spec:
  template:
    spec:
      containers:
      - name: main
        image: myapp:1.0
        env:
        - name: FOO
          value: "bar"
        - name: BAR
          value: "baz"
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Deployment");
        let sym = &symbols[0];
        let sig = sym.signature.as_ref().unwrap();
        assert!(
            sig.contains("env=FOO,BAR"),
            "Signature should contain env=FOO,BAR"
        );
    }

    #[test]
    fn extract_yaml_symbols_rbac_rules() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("role.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: reader
rules:
- apiGroups: [""]
  resources: [pods, services]
  verbs: [get, list, watch]
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Role");
        let sym = &symbols[0];
        let sig = sym.signature.as_ref().unwrap();
        assert!(
            sig.contains("verbs=get,list,watch"),
            "Signature should contain verbs=get,list,watch"
        );
        assert!(
            sig.contains("resources=pods,services"),
            "Signature should contain resources=pods,services"
        );
    }

    #[test]
    fn extract_yaml_symbols_argo_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("workflow.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  name: hello-world
spec:
  entrypoint: main
  templates:
  - name: main
    container:
      image: alpine:3.18
      command: [echo]
      args: ["hello"]
  - name: print
    container:
      image: alpine:3.18
      command: [echo]
      args: ["world"]
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Workflow");
        let sym = &symbols[0];
        assert!(
            sym.name.contains("Workflow"),
            "Symbol name should contain Workflow"
        );
        let sig = sym.signature.as_ref().unwrap();
        assert!(
            sig.contains("entrypoint=main"),
            "Signature should contain entrypoint=main"
        );
        assert!(
            sig.contains("templates=main,print"),
            "Signature should contain templates=main,print"
        );
        assert!(
            sig.contains("image=alpine:3.18"),
            "Signature should contain image=alpine:3.18"
        );
        assert!(
            sig.contains("command=echo"),
            "Signature should contain command=echo"
        );
    }

    #[test]
    fn extract_yaml_symbols_generatename_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("workflow.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  generateName: hello-
spec:
  entrypoint: main
  templates:
  - name: main
    container:
      image: alpine:3.18
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Workflow");
        let sym = &symbols[0];
        assert!(
            sym.name.contains("hello-"),
            "Symbol name should contain generateName fallback 'hello-'"
        );
    }

    #[test]
    fn detect_r_extensions_are_case_sensitive() {
        assert_eq!(detect_language(Path::new("analysis.R")), Some(LangId::R));
        assert_eq!(detect_language(Path::new("script.r")), Some(LangId::R));
    }

    #[test]
    fn extract_r_symbols_test() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("analysis.R");
        std::fs::write(
            &file,
            r#"
# summary function
summarise <- function(data, column) {
  total <- sum(data[[column]])
  total
}

normalise = function(x) {
  x / max(x)
}

function(y) {
  y + 1
} -> transform_values

threshold <- 10
"done" -> status

outer <- function(values) {
  local_value <- 1
  local_value
}
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("summarise").kind, SymbolKind::Function);
        assert_eq!(get("normalise").kind, SymbolKind::Function);
        assert_eq!(get("transform_values").kind, SymbolKind::Function);
        assert!(
            symbols
                .iter()
                .all(|symbol| !symbol.name.starts_with("function(")),
            "rightward function assignments should use the RHS identifier: {symbols:?}"
        );
        assert_eq!(get("outer").kind, SymbolKind::Function);
        assert_eq!(get("threshold").kind, SymbolKind::Variable);
        assert_eq!(get("status").kind, SymbolKind::Variable);
        assert!(
            symbols.iter().all(|symbol| symbol.name != "local_value"),
            "nested assignments should not surface as top-level variables: {symbols:?}"
        );
    }

    #[test]
    fn extract_pascal_symbols_test() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("MyUnit.pas");
        std::fs::write(
            &file,
            r#"
unit MyUnit;

interface

uses SysUtils;

type
  TMyClass = class
  private
    FValue: Integer;
  public
    constructor Create;
    procedure DoSomething; virtual;
  end;

  TMyRecord = record
    X, Y: Integer;
  end;

  IMyInterface = interface
    procedure DoSomethingElse;
  end;

  TMyEnum = (Red, Green, Blue);

const
  UNIT_CONST = 42;

var
  UnitVar: string;

implementation

constructor TMyClass.Create;
begin
  FValue := 0;
end;

procedure TMyClass.DoSomething;
begin
end;

procedure StandaloneProc;
begin
end;

end.
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("MyUnit").kind, SymbolKind::Class);
        assert_eq!(get("TMyClass").kind, SymbolKind::Class);
        assert_eq!(get("TMyRecord").kind, SymbolKind::Struct);
        assert_eq!(get("IMyInterface").kind, SymbolKind::Interface);
        assert_eq!(get("TMyEnum").kind, SymbolKind::Enum);
        assert_eq!(get("UNIT_CONST").kind, SymbolKind::Variable);
        assert_eq!(get("UnitVar").kind, SymbolKind::Variable);
        assert_eq!(get("StandaloneProc").kind, SymbolKind::Function);

        // Methods inside TMyClass
        let create_methods: Vec<&Symbol> = symbols.iter().filter(|s| s.name == "Create").collect();
        assert_eq!(create_methods.len(), 2); // one in interface, one in implementation
        for m in create_methods {
            assert_eq!(m.kind, SymbolKind::Method);
            assert_eq!(m.scope_chain, vec!["TMyClass".to_string()]);
        }

        let do_something_methods: Vec<&Symbol> =
            symbols.iter().filter(|s| s.name == "DoSomething").collect();
        assert_eq!(do_something_methods.len(), 2); // one in interface, one in implementation
        for m in do_something_methods {
            assert_eq!(m.kind, SymbolKind::Method);
            assert_eq!(m.scope_chain, vec!["TMyClass".to_string()]);
        }
    }

    #[test]
    fn groovy_fixture_extracts_types_methods_and_properties() {
        let provider = TreeSitterProvider::new();
        let symbols = provider
            .list_symbols(&fixture_path("sample.groovy"))
            .unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("GreeterSupport").kind, SymbolKind::Interface);
        assert_eq!(get("Named").kind, SymbolKind::Interface);
        assert_eq!(get("BuildStatus").kind, SymbolKind::Enum);
        assert_eq!(get("BuildLogic").kind, SymbolKind::Class);
        assert_eq!(get("action").kind, SymbolKind::Variable);
        assert_eq!(get("action").scope_chain, vec!["BuildLogic".to_string()]);
        assert_eq!(get("greet").kind, SymbolKind::Method);
        assert_eq!(get("greet").scope_chain, vec!["BuildLogic".to_string()]);
        assert_eq!(get("topLevelHelper").kind, SymbolKind::Function);
        assert!(get("topLevelHelper").scope_chain.is_empty());

        assert_eq!(get("BuildLogic").range.start_line, 12);
        assert_eq!(get("BuildLogic").range.end_line, 23);
        assert_eq!(get("action").range.start_line, 14);
        assert_eq!(get("action").range.end_line, 14);
        assert_eq!(get("greet").range.start_line, 16);
        assert_eq!(get("greet").range.end_line, 18);
        assert_eq!(get("topLevelHelper").range.start_line, 25);
        assert_eq!(get("topLevelHelper").range.end_line, 27);
    }

    #[test]
    fn groovy_gradle_fixture_extracts_task_declarations() {
        let provider = TreeSitterProvider::new();
        let symbols = provider
            .list_symbols(&fixture_path("build.gradle"))
            .unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("smokeTest").kind, SymbolKind::Function);
        assert_eq!(get("smokeTest").range.start_line, 8);
        assert_eq!(get("smokeTest").range.end_line, 12);
        assert_eq!(get("ciCheck").kind, SymbolKind::Function);
        assert_eq!(get("ciCheck").range.start_line, 14);
        assert_eq!(get("ciCheck").range.end_line, 18);
    }

    #[test]
    fn groovy_jenkinsfile_fixture_extracts_pipeline_block() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("Jenkinsfile")).unwrap();

        let pipeline = symbols
            .iter()
            .find(|symbol| symbol.name == "pipeline")
            .unwrap_or_else(|| panic!("missing pipeline; got {symbols:?}"));
        assert_eq!(pipeline.kind, SymbolKind::Function);
        assert_eq!(pipeline.range.start_line, 0);
        assert_eq!(pipeline.range.end_line, 9);
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn sustained_cache_refresh_contention_falls_back_after_three_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.rs");
        std::fs::write(&file, "pub fn original() {}\n").unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let mut parser = FileParser::new();
        parser.extract_symbols(&file).unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();
        let cache = parser.symbol_cache();
        let key = file.clone();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = calls.clone();
        CACHE_REFRESH_TEST_MUTATOR.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let call = counted.get() + 1;
                counted.set(call);
                if call <= 5 {
                    cache.write().unwrap().entries.get_mut(&key).unwrap().mtime =
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(call + 2);
                }
            }))
        });
        let symbols = parser.extract_symbols(&file).unwrap();
        CACHE_REFRESH_TEST_MUTATOR.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(symbols[0].name, "original");
        assert_eq!(calls.get(), 3);
    }

    /// Outline parse facts share the symbol cache entry's freshness on every
    /// retry attempt, including the last one that skips the cache snapshot.
    /// Contention forces that last attempt; when the file also changes under
    /// it, the fresh symbols must not be paired with the facts of the old
    /// content, and the next outline must show the new file.
    #[test]
    fn final_uncached_attempt_never_pairs_fresh_symbols_with_stale_outline_facts() {
        for rewrite in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("product.rs");
            std::fs::write(
                &file,
                "pub fn product() {}\n#[cfg(test)]\nmod checks {\n    #[test]\n    fn case() {}\n}\n",
            )
            .unwrap();
            filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1, 0)).unwrap();
            let cache: SharedSymbolCache = Arc::new(RwLock::new(SymbolCache::new()));
            let ctx = crate::context::AppContext::new(
                Box::new(TreeSitterProvider::with_symbol_cache(Arc::clone(&cache))),
                crate::config::Config::default(),
            );
            let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "retry-facts", "command": "outline", "files": [&file],
            }))
            .unwrap();
            let outline = || {
                let response = crate::commands::outline::handle_outline(&request, &ctx);
                assert!(response.success, "{response:?}");
                response.data["text"].as_str().unwrap().to_string()
            };
            let first = outline();
            assert!(first.contains("checks:"), "{first}");
            let mut parser = FileParser::with_symbol_cache(Arc::clone(&cache));
            assert!(parser.cached_outline(&file).is_some(), "facts were cached");

            // Same content, newer mtime: the snapshot path refreshes metadata,
            // and the contention below makes every refresh attempt fail.
            filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();
            let entries = Arc::clone(&cache);
            let (key, path) = (file.clone(), file.clone());
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let counted = calls.clone();
            CACHE_REFRESH_TEST_MUTATOR.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    let call = counted.get() + 1;
                    counted.set(call);
                    entries
                        .write()
                        .unwrap()
                        .entries
                        .get_mut(&key)
                        .unwrap()
                        .mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(call + 2);
                    if rewrite && call == 3 {
                        std::fs::write(&path, "pub fn changed() {}\n").unwrap();
                    }
                }))
            });
            let symbols = parser.extract_symbols(&file).unwrap();
            CACHE_REFRESH_TEST_MUTATOR.with(|slot| *slot.borrow_mut() = None);
            assert_eq!(calls.get(), 3, "the last attempt skipped the snapshot");

            let names = symbols
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                parser.cached_has_error(&file),
                Some(false),
                "rewrite={rewrite}"
            );
            if rewrite {
                assert_eq!(names, ["changed"]);
                assert!(
                    parser.cached_outline(&file).is_none(),
                    "facts of the old content survived next to the new symbols"
                );
                let text = outline();
                assert!(
                    text.contains("fn   changed")
                        && !text.contains("checks")
                        && !text.contains("fn   product"),
                    "{text}"
                );
            } else {
                assert!(names.contains(&"product"), "{names:?}");
                assert_eq!(outline(), first);
            }
        }
    }

    fn symbols(source: &str, lang: LangId) -> Vec<Symbol> {
        let tree = parse_source_with_cached_parser(Path::new("fixture"), source, lang).unwrap();
        extract_symbols_from_tree(source, &tree, lang).unwrap()
    }

    fn replaced_bytes(source: &str, lang: LangId, name: &str, replacement: &str) -> Vec<u8> {
        let symbols = symbols(source, lang);
        let range = &symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap()
            .range;
        let start = crate::edit::line_col_to_byte(source, range.start_line, range.start_col);
        let end = crate::edit::line_col_to_byte(source, range.end_line, range.end_col);
        let updated = crate::edit::replace_byte_range(source, start, end, replacement).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.txt");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &updated,
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        std::fs::read(path).unwrap()
    }

    #[test]
    fn go_grouped_types_and_aliases_edit_only_the_selected_spec() {
        let source =
            "\u{feff}package main\r\ntype (\r\n\tFirst struct{}\r\n\tSecond = First\r\n)\r\n";
        assert_eq!(replaced_bytes(source, LangId::Go, "First", "First struct{ X int }"),
            "\u{feff}package main\r\ntype (\r\n\tFirst struct{ X int }\r\n\tSecond = First\r\n)\r\n".as_bytes());
        assert_eq!(
            replaced_bytes(source, LangId::Go, "Second", "Second = int"),
            "\u{feff}package main\r\ntype (\r\n\tFirst struct{}\r\n\tSecond = int\r\n)\r\n"
                .as_bytes()
        );
        assert_eq!(
            replaced_bytes(
                "package main\ntype Single = int",
                LangId::Go,
                "Single",
                "type Single = string"
            ),
            b"package main\ntype Single = string"
        );
    }

    #[test]
    fn node_text_handles_stale_tree_offsets() {
        let tree = parse_source_with_cached_parser(
            Path::new("a.ts"),
            "const longer = 123;",
            LangId::TypeScript,
        )
        .unwrap();
        let node = tree.root_node().named_child(0).unwrap();
        assert_eq!(node_text("x", &node), "");
        assert_eq!(node_text("\u{e9}", &node), "");
        let short =
            parse_source_with_cached_parser(Path::new("a.ts"), "x", LangId::TypeScript).unwrap();
        assert_eq!(node_text("\u{e9}", &short.root_node()), "");
    }

    #[test]
    fn html_same_row_headings_have_noninverted_ranges() {
        let source = "\u{feff}<html>\r\n<h1>A</h1><h1>B</h1>\r\n</html>\r\n";
        let headings = symbols(source, LangId::Html);
        assert_eq!(headings.len(), 2);
        assert!(headings
            .iter()
            .all(|heading| heading.range.end_line >= heading.range.start_line));
        assert_eq!(
            replaced_bytes(source, LangId::Html, "A", "<h1>C</h1>"),
            "\u{feff}<html>\r\n<h1>C</h1><h1>B</h1>\r\n</html>\r\n".as_bytes()
        );
    }
}

#[cfg(test)]
#[test]
fn stage2_search_scoped_parse_source_obeys_whole_read_ceiling() {
    crate::bounded_io::with_skip_disclosure(|| {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.rs");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(crate::large_read_counts::WHOLE_READ_CEILING + 1)
            .unwrap();
        assert!(crate::large_read_counts::is_ceiling_error(
            &read_source(&path).unwrap_err()
        ));
        file.set_len(crate::large_read_counts::WHOLE_READ_CEILING - 1)
            .unwrap();
        assert_eq!(
            read_source(&path).unwrap().len() as u64,
            crate::large_read_counts::WHOLE_READ_CEILING - 1
        );
        crate::protocol::Response::success("parse-ceiling", serde_json::json!({}))
    });
}

#[cfg(test)]
mod large_source_scope_tests {
    use super::*;
    use crate::language::LanguageProvider;
    use std::path::Path;

    fn large_typescript(path: &Path) {
        let mut source = "/*".to_string();
        source.push_str(&"x".repeat(17 << 20));
        source.push_str(
            "*/\n\n\n\n\n\n\n\n\n\n\nexport function large_member(): number { return 7; }\n",
        );
        std::fs::write(path, source).unwrap();
    }

    #[test]
    fn audit1432_round3_supported_large_source_reaches_real_outline_and_zoom() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.ts");
        large_typescript(&path);
        let ctx = crate::context::AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(root.path().join(".store")),
                ..Default::default()
            },
        );
        for command in ["outline", "zoom"] {
            let req: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id":"large-source", "command":command, "file":path, "symbol":"large_member", "context_lines":0
            })).unwrap();
            let response = if command == "outline" {
                crate::commands::outline::handle_outline(&req, &ctx)
            } else {
                crate::commands::zoom::handle_zoom(&req, &ctx)
            };
            assert!(response.success, "{command}: {response:?}");
            let rendered = crate::subc_format::format_response(command, &response, false);
            assert!(rendered.contains("large_member"), "{command}: {response:?}");
            assert!(!response
                .data
                .to_string()
                .contains(crate::large_read_counts::WHOLE_READ_SKIP_REASON));
        }
    }

    #[test]
    fn audit1432_round3_background_parser_does_not_inherit_another_threads_search_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.ts");
        large_typescript(&path);
        crate::bounded_io::with_skip_disclosure(|| {
            assert!(crate::bounded_io::skip_disclosure_active());
            let budget = crate::bounded_io::capture();
            let worker_path = path.clone();
            std::thread::scope(|threads| {
                let background = threads.spawn(|| {
                    assert!(!crate::bounded_io::skip_disclosure_active());
                    assert!(read_source(&path).unwrap().len() > 17 << 20);
                    let symbols = TreeSitterProvider::new().list_symbols(&path).unwrap();
                    assert!(symbols.iter().any(|symbol| symbol.name == "large_member"));
                });
                let search_worker = threads.spawn(move || {
                    crate::bounded_io::with_budget(budget, || {
                        assert!(crate::bounded_io::skip_disclosure_active());
                        assert!(crate::large_read_counts::is_ceiling_error(
                            &read_source(&worker_path).unwrap_err()
                        ));
                    })
                });
                search_worker.join().unwrap();
                background.join().unwrap();
            });
            crate::protocol::Response::success("background-parse", serde_json::json!({}))
        });
    }
}
