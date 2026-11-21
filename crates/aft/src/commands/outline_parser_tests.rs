mod parser;

#[cfg(test)]
mod audit11_regressions {
    use crate::parser::*;
    use crate::symbols::Symbol;
    use std::path::Path;
    use std::sync::{Arc, RwLock};

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
            let ctx = crate::test_context(
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
mod large_source_scope_tests {
    use crate::language::LanguageProvider;
    use crate::parser::{read_source, TreeSitterProvider};
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
        let ctx = crate::test_context(
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
