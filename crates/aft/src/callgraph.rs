pub use aft_parse::callgraph::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph_store::disk_facts::DiskFacts;
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph_store::facts::{byte_path, EntryKind, FactPaths};
#[cfg(test)]
#[allow(unused_imports)]
use crate::callgraph_store::join::syntax_index::{Direct, Nav, SyntaxIndex};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::callgraph_store::join::Parser;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::calls::{call_node_kinds, extract_callee_name, extract_calls_full, extract_full_callee};
#[cfg(test)]
#[allow(unused_imports)]
use crate::calls::{extract_calls_full_with, extract_rust_value_references_with};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::edit::line_col_to_byte;
#[cfg(test)]
#[allow(unused_imports)]
use crate::error::AftError;
#[cfg(test)]
#[allow(unused_imports)]
use crate::imports::{self, ImportBlock};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::parser::grammar_for;
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::{detect_language, detect_language_from_source, LangId};
#[cfg(test)]
#[allow(unused_imports)]
use crate::symbols::{Range, Symbol, SymbolKind};
#[cfg(test)]
#[allow(unused_imports)]
use globset::{Glob, GlobSet, GlobSetBuilder};
#[cfg(test)]
#[allow(unused_imports)]
use serde::Serialize;
#[cfg(test)]
#[allow(unused_imports)]
use serde_json::Value;
#[cfg(test)]
#[allow(unused_imports)]
use std::cell::RefCell;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, LazyLock, OnceLock, RwLock};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::Node;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callgraph_store::facts::{DirEntry, ProjectFacts};
    use std::cell::Cell;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn call_tree_work_gap_text_names_the_node_budget() {
        assert_eq!(
            CALL_TREE_WORK_GAP,
            "Call tree cut short: examined 1000 call nodes (work budget); additional nodes not examined · narrow: symbol, depth"
        );
        assert!(
            CALL_TREE_WORK_GAP.contains(&format!("examined {CALL_TREE_NODE_BUDGET} call nodes"))
        );
    }

    #[test]
    fn bounded_memo_admits_later_keys_without_exceeding_either_bound() {
        let mut memo = BoundedMemo::<String, i32>::new(2, 30);
        memo.insert("a".to_string(), 1, 3);
        memo.insert("b".to_string(), 2, 3);
        // A full memo evicts its oldest key instead of refusing new ones.
        memo.insert("c".to_string(), 3, 3);
        assert_eq!(memo.get("c"), Some(&3));
        assert_eq!(memo.get("b"), Some(&2));
        assert_eq!(memo.get("a"), None);
        memo.insert("heavy".to_string(), 4, 8);
        assert_eq!(memo.get("heavy"), Some(&4));
        assert!(memo.retained_weight <= 30);
        assert!(memo.entries.len() <= 2);
        // An entry larger than the whole byte bound is never admitted, and
        // refusing it leaves the existing entries in place.
        memo.insert("x".to_string(), 5, 30);
        assert_eq!(memo.get("x"), None);
        assert_eq!(memo.get("heavy"), Some(&4));
        // Each admitted entry is charged its weight plus the ordered key copy.
        assert_eq!(memo.retained_weight, (3 + 1) + (8 + 5));
    }

    #[test]
    fn local_reexport_alias_uses_the_already_parsed_file() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("barrel.ts");
        fs::write(
            &file,
            "const actual = () => 1; export { actual as alias };\n",
        )
        .unwrap();
        REEXPORT_PARSE_COUNT.with(|count| count.set(0));
        let resolved =
            resolve_reexported_symbol(&file, "alias", &mut |_, _| false, &mut |_| None).unwrap();
        assert_eq!(resolved.symbol, "actual");
        REEXPORT_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn module_resolution_hits_do_not_allocate_owned_compound_keys() {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join("target.ts"), "export function target() {}\n").unwrap();
        let disk = DiskFacts::new(&root);
        let facts = FactPaths {
            root: &root,
            facts: &disk,
        };
        let memo = ModuleResolutionMemo::default();
        OWNED_MODULE_KEY_COUNT.with(|count| count.set(0));
        let first = memo.resolve_module_path(&root, "./target", &facts).unwrap();
        let admitted = OWNED_MODULE_KEY_COUNT.with(|count| count.get());
        assert!(admitted > 0);
        assert_eq!(
            memo.resolve_module_path(&root, "./target", &facts),
            Some(first)
        );
        OWNED_MODULE_KEY_COUNT.with(|count| assert_eq!(count.get(), admitted));
    }

    struct CountingFacts {
        inner: DiskFacts,
        cargo_reads: Cell<usize>,
        bin_listings: Cell<usize>,
    }

    impl CountingFacts {
        fn new(root: &Path) -> Self {
            Self {
                inner: DiskFacts::new(root),
                cargo_reads: Cell::new(0),
                bin_listings: Cell::new(0),
            }
        }
    }

    impl ProjectFacts for CountingFacts {
        fn is_file(&self, rel: &[u8]) -> bool {
            self.inner.is_file(rel)
        }

        fn is_dir(&self, rel: &[u8]) -> bool {
            self.inner.is_dir(rel)
        }

        fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
            self.inner.config_bytes(rel)
        }

        fn attributed_config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
            if rel == b"Cargo.toml" {
                self.cargo_reads.set(self.cargo_reads.get() + 1);
            }
            self.inner.attributed_config_bytes(rel)
        }

        fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
            self.inner.symlink_target(rel)
        }

        fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
            self.inner.canonical(rel)
        }

        fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
            if rel == b"src/bin" {
                self.bin_listings.set(self.bin_listings.get() + 1);
            }
            self.inner.list_dir(rel)
        }
    }

    #[test]
    fn rust_crate_root_memo_bounds_one_thousand_per_reference_lookups() {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let root = root.as_path();
        fs::create_dir_all(root.join("src/bin/support")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"memo-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/bin/tool.rs"), "mod support;\n").unwrap();
        let callers = [
            root.join("src/bin/support/admin_client.rs"),
            root.join("src/bin/support/route_client.rs"),
        ];
        for caller in &callers {
            fs::write(caller, "pub fn caller() {}\n").unwrap();
        }

        let counting = CountingFacts::new(root);
        let paths = FactPaths {
            root,
            facts: &counting,
        };
        let memo = RustCrateRootMemo::default();
        for index in 0..1_000 {
            assert_eq!(
                memo.root_file(root, &callers[index % callers.len()], &paths),
                Some(fs::canonicalize(root.join("src/bin/tool.rs")).unwrap())
            );
        }

        assert_eq!(counting.cargo_reads.get(), 1);
        assert_eq!(counting.bin_listings.get(), 1);
    }

    fn collect_calls_by_symbol_reference(
        source: &str,
        root: Node<'_>,
        lang: LangId,
        symbols: &[Symbol],
    ) -> HashMap<String, Vec<CallSite>> {
        let mut calls_by_symbol = HashMap::new();
        for symbol in symbols {
            let byte_start =
                line_col_to_byte(source, symbol.range.start_line, symbol.range.start_col);
            let byte_end = line_col_to_byte(source, symbol.range.end_line, symbol.range.end_col);
            let sites = extract_calls_full(source, root, byte_start, byte_end, lang)
                .into_iter()
                .map(
                    |(full, short, line, call_byte_start, call_byte_end)| CallSite {
                        kind: CallSiteKind::Call,
                        callee_name: short,
                        full_callee: full,
                        line,
                        byte_start: call_byte_start,
                        byte_end: call_byte_end,
                    },
                )
                .collect::<Vec<_>>();
            if !sites.is_empty() {
                calls_by_symbol.insert(symbol_identity(symbol), sites);
            }
        }

        let symbol_ranges = symbols
            .iter()
            .map(|symbol| {
                (
                    line_col_to_byte(source, symbol.range.start_line, symbol.range.start_col),
                    line_col_to_byte(source, symbol.range.end_line, symbol.range.end_col),
                )
            })
            .collect::<Vec<_>>();
        let top_level_sites = collect_calls_full_with_ranges(root, source, 0, source.len(), lang)
            .into_iter()
            .filter(|site| {
                !symbol_ranges
                    .iter()
                    .any(|(start, end)| site.byte_start >= *start && site.byte_end <= *end)
            })
            .map(|site| CallSite {
                kind: CallSiteKind::Call,
                callee_name: site.short,
                full_callee: site.full,
                line: site.line,
                byte_start: site.byte_start,
                byte_end: site.byte_end,
            })
            .collect::<Vec<_>>();
        if !top_level_sites.is_empty() {
            calls_by_symbol.insert(TOP_LEVEL_SYMBOL.to_string(), top_level_sites);
        }
        calls_by_symbol
    }

    fn parse_symbols(source: &str, lang: LangId) -> (tree_sitter::Tree, Vec<Symbol>) {
        let mut parser = Parser::new();
        parser.set_language(&grammar_for(lang).unwrap()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let symbols = crate::parser::extract_symbols_from_tree(source, &tree, lang).unwrap();
        (tree, symbols)
    }

    #[test]
    fn call_site_kind_classification_matches_range_lookup() {
        for (lang, source) in [
            (LangId::TypeScript, "function callback() {} class Service { constructor() {} method() {} } export default function entry() { register(callback); new Service().method(); }"),
            (LangId::JavaScript, "function callback() {} class Service {} export default () => { register(callback); return new Service().method(); };"),
            (LangId::Rust, "fn callback() {} fn entry() { spawn(callback); let saved = callback; spawn(crate::callback); }"),
            (LangId::Go, "package main\nfunc callback() {}\nfunc entry() { spawn(callback); saved := callback; _ = saved }\n"),
        ] {
            let (tree, _) = parse_symbols(source, lang);
            let root = tree.root_node();
            let data = build_file_data_from_tree(
                Path::new("fixture"), source, lang, &tree, &SourceLineIndex::new(source),
            ).unwrap();
            assert!(data.value_refs_by_symbol.values().flatten()
                .any(|site| site.kind == CallSiteKind::CallbackReference));
            let sites = data.calls_by_symbol.values().flatten().map(|site| (site, false))
                .chain(data.value_refs_by_symbol.values().flatten().map(|site| (site, true)));
            for (site, value_ref) in sites {
                let node = root.descendant_for_byte_range(site.byte_start, site.byte_end).unwrap();
                let expected = if value_ref {
                    if lang == LangId::Rust || node.parent().is_some_and(|parent| {
                        matches!(parent.kind(), "arguments" | "argument_list")
                    }) {
                        CallSiteKind::CallbackReference
                    } else {
                        CallSiteKind::Call
                    }
                } else if node.kind() == "new_expression" {
                    CallSiteKind::Construction
                } else {
                    CallSiteKind::Call
                };
                assert_eq!(site.kind, expected, "{lang:?}: {}", site.full_callee);
            }
        }
    }

    #[test]
    fn call_site_kind_classification_is_linear_on_deep_calls() {
        for depth in [64, 8_000] {
            let source = format!(
                "export default function entry() {{ {}0{}; }}",
                "f(".repeat(depth),
                ")".repeat(depth)
            );
            CALL_SITE_KIND_NODE_VISITS.with(|visits| visits.set(0));
            let data = build_file_data_from_source_with_lang(
                Path::new("main.ts"),
                &source,
                LangId::TypeScript,
            )
            .unwrap();
            let visits = CALL_SITE_KIND_NODE_VISITS.with(|visits| visits.get());
            assert_eq!(data.calls_by_symbol["entry"].len(), depth);
            assert!(data.calls_by_symbol["entry"]
                .iter()
                .all(|site| site.kind == CallSiteKind::Call));
            assert!(
                data.value_refs_by_symbol.is_empty(),
                "nested f(...0) calls do not pass callbacks"
            );
            assert!(
                visits >= depth && visits <= 8 * depth + 100,
                "depth={depth}, node_visits={visits}"
            );
            eprintln!("call-site kind work: depth={depth} node_visits={visits}");
        }
    }

    fn test_symbol(name: &str, start_col: u32, end_col: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 0,
                start_col,
                end_line: 0,
                end_col,
            },
            signature: None,
            scope_chain: Vec::new(),
            exported: false,
            parent: None,
        }
    }

    #[test]
    fn source_line_index_matches_shared_line_column_conversion() {
        let source = "a\r\nbb\rc\n";
        let index = SourceLineIndex::new(source);
        for line in 0..=5 {
            for column in 0..=5 {
                assert_eq!(
                    index.byte_offset(line, column),
                    line_col_to_byte(source, line, column),
                    "line={line}, column={column}"
                );
            }
        }
    }

    #[test]
    fn single_pass_call_attribution_matches_per_symbol_reference() {
        let corpora = [
            (
                "typescript",
                LangId::TypeScript,
                r#"bootstrap();
class Worker {
    run() {
        before();
        function nested() { nestedCall(); }
        nested();
    }
    next() { adjacentCall(); }
}
function left() { leftCall(); }
function right() { rightCall(); }
"#,
            ),
            (
                "python",
                LangId::Python,
                r#"bootstrap()
class Worker:
    def run(self):
        before()
        def nested():
            nested_call()
        nested()

    def next(self):
        adjacent_call()

def left():
    left_call()

def right():
    right_call()
"#,
            ),
        ];

        for (name, lang, source) in corpora {
            let (tree, symbols) = parse_symbols(source, lang);
            let reference =
                collect_calls_by_symbol_reference(source, tree.root_node(), lang, &symbols);
            let actual = collect_calls_by_symbol(
                source,
                tree.root_node(),
                lang,
                &symbols,
                &SourceLineIndex::new(source),
                Direct,
            );
            let mut reference = reference;
            reference.remove("Worker");
            assert_eq!(
                actual, reference,
                "non-class call attribution changed for {name}"
            );

            assert!(
                !actual.contains_key("Worker"),
                "method bodies are not class-owned calls"
            );
            let method_sites = actual
                .get("Worker::run")
                .expect("method receives its own calls");
            assert!(method_sites.iter().any(|site| site.callee_name == "before"));
            let nested_sites = actual
                .iter()
                .find(|(symbol, _)| symbol.rsplit("::").next() == Some("nested"))
                .map(|(_, sites)| sites)
                .expect("nested function receives its own calls");
            assert!(nested_sites.iter().any(|site| {
                site.callee_name == "nested_call" || site.callee_name == "nestedCall"
            }));
            assert_eq!(actual[TOP_LEVEL_SYMBOL][0].callee_name, "bootstrap");
        }

        let source = "first();second();third();";
        let (tree, _) = parse_symbols(source, LangId::TypeScript);
        let symbols = vec![
            test_symbol("outer", 0, 17),
            test_symbol("left", 0, 8),
            test_symbol("right", 8, 17),
            test_symbol("empty", 24, 24),
        ];
        let reference = collect_calls_by_symbol_reference(
            source,
            tree.root_node(),
            LangId::TypeScript,
            &symbols,
        );
        let actual = collect_calls_by_symbol(
            source,
            tree.root_node(),
            LangId::TypeScript,
            &symbols,
            &SourceLineIndex::new(source),
            Direct,
        );
        assert_eq!(actual, reference, "overlapping and adjacent ranges changed");
        assert_eq!(
            actual["outer"]
                .iter()
                .map(|site| site.callee_name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(actual["left"][0].callee_name, "first");
        assert_eq!(actual["right"][0].callee_name, "second");
        assert_eq!(actual[TOP_LEVEL_SYMBOL][0].callee_name, "third");
        assert!(!actual.contains_key("empty"));
    }

    #[test]
    fn symbol_metadata_for_recovers_scoped_method_by_bare_name() {
        // exported_symbols carries the bare name; symbol_metadata is keyed by
        // scoped identity (impl method). A plain .get(bare) misses and would
        // force the degraded unknown/line-1 fallback. symbol_metadata_for must
        // recover the scoped entry via unqualified-name match.
        let mut symbol_metadata = HashMap::new();
        symbol_metadata.insert(
            "BackupStore::total_disk_bytes".to_string(),
            SymbolMeta {
                is_test: Some(false),
                kind: SymbolKind::Method,
                exported: true,
                signature: None,
                line: 703,
                range: Range {
                    start_line: 702,
                    start_col: 0,
                    end_line: 705,
                    end_col: 0,
                },
                entry_point_attribute: None,
                program_start: false,
            },
        );
        let file_data = FileCallData {
            calls_by_symbol: HashMap::new(),
            value_refs_by_symbol: HashMap::new(),
            macro_body_calls_by_macro: HashMap::new(),
            macro_mentions_by_symbol: HashMap::new(),
            exported_symbols: vec!["total_disk_bytes".to_string()],
            symbol_metadata,
            default_export_symbol: None,
            import_block: ImportBlock::empty(),
            python_bindings: Vec::new(),
            lang: LangId::Rust,
        };

        let meta = file_data
            .symbol_metadata_for("total_disk_bytes")
            .expect("scoped method recovered by bare name");
        assert_eq!(meta.kind, SymbolKind::Method);
        assert_eq!(
            meta.line, 703,
            "real declaration line, not the line-1 fallback"
        );

        // A genuinely-absent symbol still returns None (no false recovery).
        assert!(file_data.symbol_metadata_for("does_not_exist").is_none());
    }

    /// Create a temp directory with TypeScript files for testing.
    fn setup_ts_project() -> TempDir {
        let dir = TempDir::new().unwrap();

        // main.ts: imports from utils and calls functions
        fs::write(
            dir.path().join("main.ts"),
            r#"import { helper, compute } from './utils';
import * as math from './math';

export function main() {
    const a = helper(1);
    const b = compute(a, 2);
    const c = math.add(a, b);
    return c;
}
"#,
        )
        .unwrap();

        // utils.ts: defines helper and compute, imports from helpers
        fs::write(
            dir.path().join("utils.ts"),
            r#"import { double } from './helpers';

export function helper(x: number): number {
    return double(x);
}

export function compute(a: number, b: number): number {
    return a + b;
}
"#,
        )
        .unwrap();

        // helpers.ts: defines double
        fs::write(
            dir.path().join("helpers.ts"),
            r#"export function double(x: number): number {
    return x * 2;
}

export function triple(x: number): number {
    return x * 3;
}
"#,
        )
        .unwrap();

        // math.ts: defines add (for namespace import test)
        fs::write(
            dir.path().join("math.ts"),
            r#"export function add(a: number, b: number): number {
    return a + b;
}

export function subtract(a: number, b: number): number {
    return a - b;
}
"#,
        )
        .unwrap();

        dir
    }

    // --- Single-file call extraction ---

    #[test]
    fn callgraph_single_file_call_extraction() {
        let dir = setup_ts_project();

        let file_data = build_file_data(&dir.path().join("main.ts")).unwrap();
        let main_calls = &file_data.calls_by_symbol["main"];

        let callee_names: Vec<&str> = main_calls.iter().map(|c| c.callee_name.as_str()).collect();
        assert!(
            callee_names.contains(&"helper"),
            "main should call helper, got: {:?}",
            callee_names
        );
        assert!(
            callee_names.contains(&"compute"),
            "main should call compute, got: {:?}",
            callee_names
        );
        assert!(
            callee_names.contains(&"add"),
            "main should call math.add (short name: add), got: {:?}",
            callee_names
        );
    }

    #[test]
    fn callgraph_file_data_has_exports() {
        let dir = setup_ts_project();

        let file_data = build_file_data(&dir.path().join("utils.ts")).unwrap();
        assert!(
            file_data.exported_symbols.contains(&"helper".to_string()),
            "utils.ts should export helper, got: {:?}",
            file_data.exported_symbols
        );
        assert!(
            file_data.exported_symbols.contains(&"compute".to_string()),
            "utils.ts should export compute, got: {:?}",
            file_data.exported_symbols
        );
    }

    // --- Worktree walker ---

    #[test]
    fn scoped_project_walk_prunes_unrelated_subtrees() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("outside/deep")).unwrap();
        fs::write(dir.path().join("src/main.ts"), "export const main = 1;\n").unwrap();
        fs::write(
            dir.path().join("outside/deep/other.ts"),
            "export const other = 2;\n",
        )
        .unwrap();
        let scope = dir.path().join("src");
        let visited = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observations = visited.clone();
        let files = walk_project_files_filtered(
            dir.path(),
            crate::project_walk::IgnoreFileAccess::Allowed,
            move |path| {
                observations.lock().unwrap().push(path.to_path_buf());
                path.starts_with(&scope) || scope.starts_with(path)
            },
        )
        .collect::<Vec<_>>();
        assert_eq!(files, vec![dir.path().join("src/main.ts")]);
        // Recording the traversal predicate distinguishes pruning a directory
        // before descent from filtering its files after walking the subtree.
        let visited = visited.lock().unwrap();
        assert!(
            !visited.contains(&dir.path().join("outside/deep")),
            "{visited:?}"
        );
        assert!(
            !visited.contains(&dir.path().join("outside/deep/other.ts")),
            "{visited:?}"
        );
    }

    #[test]
    fn callgraph_walker_excludes_gitignored() {
        let dir = TempDir::new().unwrap();

        // Create a .gitignore
        fs::write(dir.path().join(".gitignore"), "ignored_dir/\n").unwrap();

        // Create files
        fs::write(dir.path().join("main.ts"), "export function main() {}").unwrap();
        fs::create_dir(dir.path().join("ignored_dir")).unwrap();
        fs::write(
            dir.path().join("ignored_dir").join("secret.ts"),
            "export function secret() {}",
        )
        .unwrap();

        // Also create node_modules (should always be excluded)
        fs::create_dir(dir.path().join("node_modules")).unwrap();
        fs::write(
            dir.path().join("node_modules").join("dep.ts"),
            "export function dep() {}",
        )
        .unwrap();

        // Init git repo for .gitignore to work
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(dir.path()))
            .args(["init"])
            .output()
            .unwrap();

        let files: Vec<PathBuf> =
            walk_project_files(dir.path(), crate::project_walk::IgnoreFileAccess::Allowed)
                .collect();
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(
            file_names.contains(&"main.ts".to_string()),
            "Should include main.ts, got: {:?}",
            file_names
        );
        assert!(
            !file_names.contains(&"secret.ts".to_string()),
            "Should exclude gitignored secret.ts, got: {:?}",
            file_names
        );
        assert!(
            !file_names.contains(&"dep.ts".to_string()),
            "Should exclude node_modules, got: {:?}",
            file_names
        );
    }

    #[test]
    fn callgraph_walker_honours_gitignore_in_non_git_root_like_git_root() {
        use crate::context::ignore_rules_fixture as fixture;
        let plain = TempDir::new().unwrap();
        let git = TempDir::new().unwrap();
        fixture::write(plain.path(), false);
        fixture::write(git.path(), true);
        let walked = |root: &Path| {
            fixture::relative_set(
                root,
                &walk_project_files(root, crate::project_walk::IgnoreFileAccess::Allowed)
                    .collect::<Vec<_>>(),
            )
        };

        let plain_files = walked(plain.path());
        fixture::assert_honours_ignore_rules(&plain_files, "non-git call graph walk");
        assert_eq!(plain_files, walked(git.path()));
    }

    #[test]
    fn callgraph_walker_excludes_aftignored() {
        let dir = TempDir::new().unwrap();

        // .aftignore is honored without a git repo (custom ignore file).
        fs::write(dir.path().join(".aftignore"), "vendored/\n").unwrap();
        fs::write(dir.path().join("main.ts"), "export function main() {}").unwrap();
        fs::create_dir(dir.path().join("vendored")).unwrap();
        fs::write(
            dir.path().join("vendored").join("sub.ts"),
            "export function sub() {}",
        )
        .unwrap();

        let files: Vec<PathBuf> =
            walk_project_files(dir.path(), crate::project_walk::IgnoreFileAccess::Allowed)
                .collect();
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(
            file_names.contains(&"main.ts".to_string()),
            "Should include main.ts, got: {:?}",
            file_names
        );
        assert!(
            !file_names.contains(&"sub.ts".to_string()),
            "Should exclude .aftignored sub.ts, got: {:?}",
            file_names
        );
    }

    #[test]
    fn callgraph_walker_only_source_files() {
        let dir = TempDir::new().unwrap();

        fs::write(dir.path().join("main.ts"), "export function main() {}").unwrap();
        fs::write(dir.path().join("module.mts"), "export function esm() {}").unwrap();
        fs::write(dir.path().join("common.cts"), "export function cjs() {}").unwrap();
        fs::write(
            dir.path().join("runtime.mjs"),
            "export function runtime() {}",
        )
        .unwrap();
        fs::write(
            dir.path().join("legacy.cjs"),
            "exports.legacy = function() {};",
        )
        .unwrap();
        fs::write(dir.path().join("types.pyi"), "def typed() -> None: ...").unwrap();
        fs::write(dir.path().join("readme.md"), "# Hello").unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();

        let files: Vec<PathBuf> =
            walk_project_files(dir.path(), crate::project_walk::IgnoreFileAccess::Allowed)
                .collect();
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(file_names.contains(&"main.ts".to_string()));
        for modern_ext_file in [
            "module.mts",
            "common.cts",
            "runtime.mjs",
            "legacy.cjs",
            "types.pyi",
        ] {
            assert!(
                file_names.contains(&modern_ext_file.to_string()),
                "walker should include {modern_ext_file}, got: {:?}",
                file_names
            );
        }
        assert!(
            file_names.contains(&"readme.md".to_string()),
            "Markdown is now a supported source language"
        );
        assert!(
            file_names.contains(&"data.json".to_string()),
            "JSON is now a supported source language"
        );
    }

    // --- Reverse callers ---

    #[test]
    fn is_entry_point_exported_function() {
        assert!(is_entry_point(
            "handleRequest",
            &SymbolKind::Function,
            true,
            LangId::TypeScript
        ));
    }

    #[test]
    fn is_entry_point_exported_method_is_not_entry() {
        // Methods are class members, not standalone entry points
        assert!(!is_entry_point(
            "handleRequest",
            &SymbolKind::Method,
            true,
            LangId::TypeScript
        ));
    }

    #[test]
    fn is_entry_point_main_init_patterns() {
        for name in &["main", "Main", "MAIN", "init", "setup", "bootstrap", "run"] {
            assert!(
                is_entry_point(name, &SymbolKind::Function, false, LangId::TypeScript),
                "{} should be an entry point",
                name
            );
        }
    }

    #[test]
    fn is_entry_point_test_patterns_ts() {
        assert!(is_entry_point(
            "describe",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "it",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "test",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "testValidation",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "specHelper",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
    }

    #[test]
    fn is_entry_point_test_patterns_python() {
        assert!(is_entry_point(
            "test_login",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
        assert!(is_entry_point(
            "setUp",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
        assert!(is_entry_point(
            "tearDown",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
        // "testSomething" should NOT match Python (needs test_ prefix)
        assert!(!is_entry_point(
            "testSomething",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
    }

    #[test]
    fn is_entry_point_test_patterns_rust() {
        assert!(is_entry_point(
            "test_parse",
            &SymbolKind::Function,
            false,
            LangId::Rust
        ));
        assert!(!is_entry_point(
            "TestSomething",
            &SymbolKind::Function,
            false,
            LangId::Rust
        ));
    }

    #[test]
    fn is_entry_point_test_patterns_go() {
        assert!(is_entry_point(
            "TestParsing",
            &SymbolKind::Function,
            false,
            LangId::Go
        ));
        // lowercase test should NOT match Go (needs uppercase Test prefix)
        assert!(!is_entry_point(
            "testParsing",
            &SymbolKind::Function,
            false,
            LangId::Go
        ));
    }

    #[test]
    fn is_entry_point_non_exported_non_main_is_not_entry() {
        assert!(!is_entry_point(
            "helperUtil",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
    }

    // --- symbol_metadata ---

    #[test]
    fn callgraph_symbol_metadata_populated() {
        let dir = setup_ts_project();

        let file_data = build_file_data(&dir.path().join("utils.ts")).unwrap();
        assert!(
            file_data.symbol_metadata.contains_key("helper"),
            "symbol_metadata should contain helper"
        );
        let meta = &file_data.symbol_metadata["helper"];
        assert_eq!(meta.kind, SymbolKind::Function);
        assert!(meta.exported, "helper should be exported");
    }

    #[test]
    fn review_opus6_tsconfig_wildcard_rejects_overlap() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("tsconfig.json"),
            r#"{"compilerOptions":{"paths":{"@/*/":["src/*"]}}}"#,
        )
        .unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/ok.ts"), "export {};\n").unwrap();
        assert!(resolve_module_path(dir.path(), "@/").is_none());
        assert!(resolve_module_path(dir.path(), "@/ok/")
            .unwrap()
            .ends_with("src/ok.ts"));
    }

    #[test]
    fn review_opus6_package_export_wildcard_rejects_overlap() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let package = dir.path().join("packages/lib");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@scope/lib","exports":{"./a*a":"./entry-*.ts"}}"#,
        )
        .unwrap();
        fs::write(package.join("entry-.ts"), "export {};\n").unwrap();
        assert!(resolve_module_path(dir.path(), "@scope/lib/a").is_none());
        assert!(resolve_module_path(dir.path(), "@scope/lib/aa")
            .unwrap()
            .ends_with("entry-.ts"));
    }

    #[test]
    fn review_opus7_workspace_cache_notices_new_packages() {
        let dir = TempDir::new().unwrap();
        let root = &fs::canonicalize(dir.path()).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let create_package = |member: &str| {
            let path = root.join("packages").join(member);
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("package.json"),
                format!(r#"{{"name":"@scope/{member}","main":"index.ts"}}"#),
            )
            .unwrap();
            fs::write(path.join("index.ts"), "export {};\n").unwrap();
        };
        create_package("first");
        crate::callgraph_store::work_counts::reset_under(root);
        assert!(resolve_module_path(root, "@scope/first").is_some());
        let (_, _, cold_walks, _) = crate::callgraph_store::work_counts::filesystem_counts(root);
        assert!(cold_walks > 0);
        assert!(resolve_module_path(root, "@scope/first").is_some());
        let (_, _, warm_walks, _) = crate::callgraph_store::work_counts::filesystem_counts(root);
        assert_eq!(
            warm_walks, cold_walks,
            "a warm hit should not repeat the workspace walk"
        );
        eprintln!(
            "workspace directories visited: {cold_walks} cold, {} warm",
            warm_walks - cold_walks
        );
        assert!(resolve_module_path(root, "@scope/new").is_none());
        create_package("new");
        assert!(resolve_module_path(root, "@scope/new")
            .unwrap()
            .ends_with("packages/new/index.ts"));
    }

    #[test]
    fn workspace_package_resolution_prefers_modern_ts_source_extensions() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let package_dir = dir.path().join("packages/lib");
        fs::create_dir_all(package_dir.join("src")).unwrap();
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"@scope/lib","exports":{".":"./dist/index.mjs"}}"#,
        )
        .unwrap();
        fs::write(
            package_dir.join("src/index.mts"),
            "export function helper() {}\n",
        )
        .unwrap();
        fs::write(package_dir.join("dist/index.mjs"), "export{};\n").unwrap();

        let resolved = resolve_module_path(dir.path(), "@scope/lib").unwrap();
        assert!(
            resolved.ends_with("src/index.mts"),
            "dist/index.mjs should map to src/index.mts, got {resolved:?}"
        );
    }

    #[test]
    fn cold_workspace_walk_prunes_unreachable_trees_but_keeps_ignored_members() {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for member in ["packages/a", "packages/a/nested", "vendor/ignored"] {
            fs::create_dir_all(root.join(member)).unwrap();
            fs::write(root.join(member).join("package.json"), "{}").unwrap();
        }
        fs::write(root.join(".gitignore"), "vendor/\nbenchmarks/clones/\n").unwrap();
        let mut clone = root.join("benchmarks/clones");
        for _ in 0..40 {
            clone.push("nested");
        }
        fs::create_dir_all(clone).unwrap();
        for excluded in ["node_modules", ".git", "target", "dist", "build"] {
            fs::create_dir_all(root.join(excluded).join("pkg")).unwrap();
            fs::write(root.join(excluded).join("pkg/package.json"), "{}").unwrap();
        }
        let disk = DiskFacts::new(&root);
        let facts = FactPaths {
            root: &root,
            facts: &disk,
        };
        crate::callgraph_store::work_counts::reset_under(&root);
        let members = expand_workspace_patterns(&root, &["packages/*".into()], &facts);
        assert_eq!(
            members,
            [root.join("packages/a"), root.join("packages/a/nested")],
            "preserve the existing glob match semantics, including '*' across separators"
        );
        let (_, _, visits, _) = crate::callgraph_store::work_counts::filesystem_counts(&root);
        assert_eq!(
            visits, 4,
            "only the root and the packages subtree need enumeration"
        );
        let members = expand_workspace_patterns(&root, &["vendor/*".into()], &facts);
        assert_eq!(
            members,
            [root.join("vendor/ignored")],
            "gitignore cannot veto a workspace manifest"
        );

        // An independent, unpruned enumeration provides the expected candidates.
        fn candidates(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap().map(Result::unwrap) {
                if !entry.file_type().unwrap().is_dir()
                    || matches!(
                        entry.file_name().to_str(),
                        Some("node_modules" | ".git" | "target" | "dist" | "build")
                    )
                {
                    continue;
                }
                let path = entry.path();
                if path.join("package.json").is_file() {
                    out.push(path.clone());
                }
                candidates(&path, out);
            }
        }
        let mut all = Vec::new();
        candidates(&root, &mut all);
        for patterns in [
            vec!["packages/a"],
            vec!["packages/**", "!packages/*/nested"],
            vec!["{packages,vendor}/**"],
            vec!["pack*/a*"],
            vec!["**/ignored"],
            vec!["vendor/*"],
            vec!["[pv]*/**"],
            vec!["packages/a/"],
        ] {
            let positives = build_glob_set(
                &patterns
                    .iter()
                    .copied()
                    .filter(|p| !p.starts_with('!'))
                    .collect::<Vec<_>>(),
            );
            let negatives = build_glob_set(
                &patterns
                    .iter()
                    .filter_map(|p| p.strip_prefix('!'))
                    .collect::<Vec<_>>(),
            );
            let mut expected: Vec<_> = all
                .iter()
                .filter(|p| {
                    let rel = p
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    positives.is_match(&rel) && !negatives.is_match(&rel)
                })
                .cloned()
                .collect();
            let mut actual = expand_workspace_patterns(
                &root,
                &patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                &facts,
            );
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{patterns:?}");
        }
    }

    #[test]
    fn same_named_methods_use_scoped_symbol_identity() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("classes.ts"),
            r#"class A {
    run() { helperA(); }
}

class B {
    run() { helperB(); }
}

function helperA() {}
function helperB() {}
"#,
        )
        .unwrap();

        let path = dir.path().join("classes.ts");
        let data = build_file_data(&path).unwrap();

        assert!(
            data.symbol_metadata.contains_key("A::run"),
            "A::run metadata missing"
        );
        assert!(
            data.symbol_metadata.contains_key("B::run"),
            "B::run metadata missing"
        );
        assert!(
            data.calls_by_symbol["A::run"]
                .iter()
                .any(|call| call.callee_name == "helperA"),
            "A::run calls should not be overwritten"
        );
        assert!(
            data.calls_by_symbol["B::run"]
                .iter()
                .any(|call| call.callee_name == "helperB"),
            "B::run calls should not be overwritten"
        );
    }

    // --- extract_parameters ---

    #[test]
    fn audit_resolution_nested_callback_keeps_later_parameters() {
        assert_eq!(
            extract_parameters("fn run(cb: fn(i32) -> i32, count: i32)", LangId::Rust),
            ["cb", "count"]
        );
        assert_eq!(
            extract_parameters(r#"def run(text=") ,", count=0):"#, LangId::Python),
            ["text", "count"]
        );
    }

    #[test]
    fn extract_parameters_typescript() {
        let params = extract_parameters(
            "function processData(input: string, count: number): void",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["input", "count"]);
    }

    #[test]
    fn extract_parameters_typescript_optional() {
        let params = extract_parameters(
            "function fetch(url: string, options?: RequestInit): Promise<Response>",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["url", "options"]);
    }

    #[test]
    fn extract_parameters_typescript_defaults() {
        let params = extract_parameters(
            "function greet(name: string, greeting: string = \"hello\"): string",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["name", "greeting"]);
    }

    #[test]
    fn extract_parameters_typescript_rest() {
        let params = extract_parameters(
            "function sum(...numbers: number[]): number",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["numbers"]);
    }

    #[test]
    fn extract_parameters_python_self_skipped() {
        let params = extract_parameters(
            "def process(self, data: str, count: int) -> bool",
            LangId::Python,
        );
        assert_eq!(params, vec!["data", "count"]);
    }

    #[test]
    fn extract_parameters_python_no_self() {
        let params = extract_parameters("def validate(input: str) -> bool", LangId::Python);
        assert_eq!(params, vec!["input"]);
    }

    #[test]
    fn extract_parameters_python_star_args() {
        let params = extract_parameters("def func(*args, **kwargs)", LangId::Python);
        assert_eq!(params, vec!["args", "kwargs"]);
    }

    #[test]
    fn extract_parameters_rust_self_skipped() {
        let params = extract_parameters(
            "fn process(&self, data: &str, count: usize) -> bool",
            LangId::Rust,
        );
        assert_eq!(params, vec!["data", "count"]);
    }

    #[test]
    fn extract_parameters_rust_mut_self_skipped() {
        let params = extract_parameters("fn update(&mut self, value: i32)", LangId::Rust);
        assert_eq!(params, vec!["value"]);
    }

    #[test]
    fn extract_parameters_rust_no_self() {
        let params = extract_parameters("fn validate(input: &str) -> bool", LangId::Rust);
        assert_eq!(params, vec!["input"]);
    }

    #[test]
    fn extract_parameters_rust_mut_param() {
        let params = extract_parameters("fn process(mut buf: Vec<u8>, len: usize)", LangId::Rust);
        assert_eq!(params, vec!["buf", "len"]);
    }

    #[test]
    fn extract_parameters_go() {
        let params = extract_parameters(
            "func ProcessData(input string, count int) error",
            LangId::Go,
        );
        assert_eq!(params, vec!["input", "count"]);
    }

    #[test]
    fn extract_parameters_empty() {
        let params = extract_parameters("function noArgs(): void", LangId::TypeScript);
        assert!(
            params.is_empty(),
            "no-arg function should return empty params"
        );
    }

    #[test]
    fn extract_parameters_no_parens() {
        let params = extract_parameters("const x = 42", LangId::TypeScript);
        assert!(params.is_empty(), "no parens should return empty params");
    }

    #[test]
    fn extract_parameters_javascript() {
        let params = extract_parameters("function handleClick(event, target)", LangId::JavaScript);
        assert_eq!(params, vec!["event", "target"]);
    }
}
