mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::context::AppContext;
    use crate::data_layer::content;
    use crate::data_layer::outline::{root_bound, warm_file};
    use crate::parser::work_counters;
    use crate::parser::{SharedSymbolCache, SymbolCache};
    use crate::protocol::{RawRequest, Response};
    use aft_engine::test_bridge::display_data::outline::{
        already_claimed_for_test, complete_rust_outline, prewarm, Prewarm,
    };
    #[cfg(unix)]
    use aft_engine::test_bridge::display_data::outline::{record_reads, tables};

    fn write(root: &Path, path: &str, text: &str) -> PathBuf {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn copy(root: &Path, from: &Path, to: &str) {
        write(root, to, &std::fs::read_to_string(from).unwrap());
    }

    /// A small checkout of several languages: the TypeScript call-graph
    /// fixtures, a Rust crate with impl methods, a trait impl and platform
    /// twins, two of AFT's own sources, Python, Go and a C++ header, beside
    /// files no record covers (Markdown and Cargo.toml). Returns the source
    /// files, relative to `root`.
    fn fixture(root: &Path) -> Vec<String> {
        let crate_dir = crate::test_support::manifest_dir();
        let mut sources = Vec::new();
        for entry in std::fs::read_dir(crate_dir.join("tests/fixtures/callgraph")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "ts") {
                let relative = format!("web/{}", path.file_name().unwrap().to_str().unwrap());
                copy(root, &path, &relative);
                sources.push(relative);
            }
        }
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(root, "README.md", "# Demo\n\nNotes.\n");
        let inline = [
            (
                "src/lib.rs",
                r#"mod stat;
mod store;
mod runtime;

use store::Store;

pub fn start(raw: String) -> String {
    let copy = raw;
    let opened = Store::open(copy);
    opened.join("x")
}
"#,
            ),
            (
                "src/store.rs",
                r#"pub struct Store {
    path: String,
}

impl Store {
    pub fn open(path: String) -> Self {
        let owned = path;
        Store { path: owned }
    }

    pub fn join(&self, part: &str) -> String {
        format!("{}/{}", self.path, part)
    }
}

impl std::fmt::Display for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)
    }
}

#[cfg(unix)]
pub fn separator() -> char {
    '/'
}

#[cfg(windows)]
pub fn separator() -> char {
    '\\'
}
"#,
            ),
            // `process`, not `run`: today's store also marks callers named
            // like entry points (`main`, `run`, ...) as entry points, which
            // the data layer, like the views route, does not; `impact` below
            // compares the answers whole.
            (
                "tool/main.py",
                "from tool.helpers import clean\n\n\nclass Runner:\n    def execute(self, value):\n        copy = value\n        return clean(copy)\n\n\ndef process(value):\n    copy = value\n    return clean(copy)\n",
            ),
            (
                "tool/helpers.py",
                "def clean(text):\n    stripped = text\n    return stripped\n",
            ),
            (
                "include/widget.h",
                "namespace demo {\nclass Widget {\npublic:\n    int size() const;\n};\n}\n",
            ),
        ];
        for (path, text) in inline {
            write(root, path, text);
            sources.push(path.to_string());
        }
        for (from, to) in [
            ("../aft-store/src/stat.rs", "src/stat.rs"),
            ("../aft-engine/src/data_layer/runtime.rs", "src/runtime.rs"),
            ("tests/fixtures/sample.go", "pkg/sample.go"),
        ] {
            copy(root, &crate_dir.join(from), to);
            sources.push(to.to_string());
        }
        sources.sort();
        sources
    }

    fn context(root: &Path, storage: &Path) -> AppContext {
        let config = crate::config::Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        };
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx
    }

    fn request(command: &str, params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: format!("data-layer-{command}"),
            command: command.into(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    fn outline(ctx: &AppContext, file: &Path) -> Response {
        crate::commands::outline::handle_outline(
            &request("outline", serde_json::json!({ "file": file })),
            ctx,
        )
    }

    /// A first process builds and publishes the checkout at `root`; the
    /// context returned is a later process that has not bound it yet.
    fn published_then_restarted(root: &Path) -> (AppContext, tempfile::TempDir) {
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root, storage.path());
        let outcome = crate::data_layer::surfaces::index_root(
            root,
            &ctx.storage_dir(),
            crate::data_layer::checkout::Planes::ALL,
        )
        .unwrap();
        assert!(outcome.published);
        (ctx, storage)
    }

    fn fresh_cache() -> SharedSymbolCache {
        Arc::new(std::sync::RwLock::new(SymbolCache::new()))
    }

    #[test]
    fn rust_trait_methods_agree_in_stored_and_live_navigation_without_reindexing() {
        use crate::symbols::SymbolKind;
        use aft_store::content::RecordBody;

        let source = include_str!("../../../aft/tests/fixtures/rust_trait_methods.rs");
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = write(&root, "host.rs", source);
        let record = content::extract(source.as_bytes(), "rust").unwrap();
        let encoded = record.encode();
        let RecordBody::Parsed { surface, body } = &record.data else {
            panic!("Rust fixture must have a parsed record");
        };
        let raw = body.outline.as_ref().unwrap();
        assert!(!raw
            .iter()
            .any(|symbol| symbol.parent.as_deref() == Some("Host")));
        assert!(surface
            .symbols
            .iter()
            .any(|symbol| symbol.scoped_name == "Host::held_calls"));
        let completed = complete_rust_outline(raw.clone(), surface);
        assert_eq!(
            record.encode(),
            encoded,
            "navigation must not rewrite the record"
        );
        assert_eq!(
            complete_rust_outline(completed.clone(), surface),
            completed,
            "members must not be duplicated when already present"
        );

        let (stored, _storage) = published_then_restarted(&root);
        let view = stored.data_checkout().unwrap().view().unwrap();
        let cache = stored.symbol_cache();
        let generation = cache.read().unwrap().generation();
        assert_eq!(
            prewarm(&view, &cache, generation, &|| false)
                .unwrap()
                .loaded(),
            1
        );
        let before = work_counters::tree_parses();
        let warm = crate::parser::FileParser::with_symbol_cache(cache.clone())
            .extract_symbols(&file)
            .unwrap();
        assert_eq!(
            work_counters::tree_parses(),
            before,
            "stored members must need no parse"
        );
        let parsed = crate::parser::FileParser::new()
            .extract_symbols(&file)
            .unwrap();
        assert_eq!(warm, parsed);
        for (owner, name, scope) in [
            ("Host", "context_ceiling", vec!["Host"]),
            ("Host", "held_calls", vec!["Host"]),
            ("Nested", "default_method", vec!["Nested"]),
            ("Nested", "required_method", vec!["Nested"]),
            ("Runner", "new", vec!["Runner"]),
            ("Runner", "held_calls", vec!["Host for Runner"]),
            ("NestedRunner", "create", vec!["NestedRunner"]),
        ] {
            let matches = warm
                .iter()
                .filter(|symbol| symbol.name == name && symbol.parent.as_deref() == Some(owner))
                .collect::<Vec<_>>();
            assert_eq!(matches.len(), 1, "{owner}::{name}: {warm:?}");
            assert_eq!(matches[0].kind, SymbolKind::Method);
            assert_eq!(matches[0].scope_chain, scope);
            if owner == "Host" || owner == "Nested" {
                assert!(
                    !matches[0].exported,
                    "trait members are reached through their owner"
                );
            }
        }

        let live_storage = tempfile::tempdir().unwrap();
        let live = context(&root, live_storage.path());
        let answer = outline(&stored, &file);
        assert!(answer.success, "{:?}", answer.data);
        assert_eq!(answer.data, outline(&live, &file).data);
        let text = answer.data["text"].as_str().unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        for (owner, methods) in [
            ("pub trait Host", vec!["context_ceiling", "held_calls"]),
            (
                "pub trait Nested",
                vec!["default_method", "required_method"],
            ),
            ("pub struct Runner", vec!["new", "held_calls"]),
        ] {
            let start = lines
                .iter()
                .position(|line| line.trim_start().trim_start_matches('.').starts_with(owner))
                .unwrap_or_else(|| panic!("{owner}: {text}"));
            let indent = lines[start].len() - lines[start].trim_start().len();
            let member_indent = " ".repeat(indent + 2);
            for method in methods {
                assert!(
                    lines[start + 1..]
                        .iter()
                        .take_while(|line| line.starts_with(&member_indent))
                        .any(|line| line.contains(method)
                            && (indent != 2 || line.starts_with("    ."))),
                    "{owner} must nest {method}: {text}"
                );
            }
        }
        for symbol in [
            "context_ceiling",
            "Host::context_ceiling",
            "held_calls",
            "Host::held_calls",
            "default_method",
            "Nested::default_method",
            "required_method",
            "Nested::required_method",
            "create",
            "NestedRunner::create",
            "Runner::new",
        ] {
            let request = request("zoom", serde_json::json!({"file": file, "symbol": symbol}));
            let warm_answer = crate::commands::zoom::handle_zoom(&request, &stored);
            let live_answer = crate::commands::zoom::handle_zoom(&request, &live);
            assert!(warm_answer.success, "{symbol}: {:?}", warm_answer.data);
            assert_eq!(warm_answer.data, live_answer.data, "{symbol}");
            assert!(
                warm_answer.data["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(symbol.rsplit("::").next().unwrap())),
                "{symbol}: {:?}",
                warm_answer.data
            );
        }

        write(&root, "host.rs", &source.replace("4096", "81920"));
        assert!(
            !warm_file(&view, &fresh_cache(), &file),
            "an unpublished edit must use live symbols"
        );
        let request = request(
            "zoom",
            serde_json::json!({"file": file, "symbol": "Host::context_ceiling"}),
        );
        let edited = crate::commands::zoom::handle_zoom(&request, &stored);
        assert!(edited.success, "{:?}", edited.data);
        assert!(edited.data["content"].as_str().unwrap().contains("81920"));
    }

    /// After a restart the checkout's records fill the symbol cache with
    /// the tables a parse gives, and outline then answers as a context that
    /// parses every file does, without parsing.
    #[test]
    fn a_restart_fills_the_symbol_cache_from_records_with_todays_tables() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let sources = fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);

        let view = ctx
            .data_checkout()
            .expect("the data layer binds")
            .view()
            .expect("a restart adopts the publication");
        assert!(view.status().callgraph_published);
        let cache = ctx.symbol_cache();
        let generation = cache.read().unwrap().generation();
        let report = prewarm(&view, &cache, generation, &|| false).unwrap();
        assert_eq!(report, Prewarm::with_loaded(sources.len() as u64));

        let cold = context(&root, tempfile::tempdir().unwrap().path());
        for source in &sources {
            let path = root.join(source);
            let parsed = crate::parser::FileParser::new()
                .extract_symbols(&path)
                .unwrap();
            let before = work_counters::tree_parses();
            let warm = crate::parser::FileParser::with_symbol_cache(Arc::clone(&cache))
                .extract_symbols(&path)
                .unwrap();
            assert_eq!(work_counters::tree_parses(), before, "{source} was parsed");
            assert_eq!(warm, parsed, "{source}");

            // Rust outlines also show the module layout, which no store
            // keeps: they parse once, as after a load of the disk cache.
            let rust = source.ends_with(".rs");
            let before = work_counters::cached_parser_parses();
            let answer = outline(&ctx, &path);
            if !rust {
                assert_eq!(
                    work_counters::cached_parser_parses(),
                    before,
                    "{source}: outline parsed"
                );
            }
            let today = outline(&cold, &path);
            assert!(answer.success, "{source}: {:?}", answer.data);
            assert_eq!(answer.data, today.data, "{source}");
        }

        // A second prewarm finds every file in the cache already.
        let again = prewarm(&view, &cache, generation, &|| false).unwrap();
        assert_eq!(again.cached(), sources.len() as u64);
        assert_eq!(again.loaded(), 0);
    }

    /// A file that moved on from its published content, on disk only or
    /// recorded as changed too, is left to a parse; so is every file once
    /// the cache is reset.
    #[test]
    fn files_changed_since_their_record_are_left_to_a_parse() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let sources = fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);
        let checkout = ctx.data_checkout().expect("the data layer binds");

        // Changed on disk, not yet reported by the watcher.
        let store = write(&root, "src/store.rs", "pub fn only() {}\n");
        let view = checkout.view().unwrap();
        assert!(view.outline_symbols(b"src/store.rs").unwrap().is_some());
        let cache = fresh_cache();
        let report = prewarm(&view, &cache, 0, &|| false).unwrap();
        assert_eq!(report.left(), 1);
        assert_eq!(report.loaded(), sources.len() as u64 - 1);
        assert!(!cache.read().unwrap().contains_key(&store));
        assert!(!warm_file(&view, &cache, &store));

        // Reported: the view no longer offers its old record.
        checkout.note_paths(&[store]);
        let view = checkout.view().unwrap();
        assert!(view.outline_symbols(b"src/store.rs").unwrap().is_none());
        // Files no record covers.
        assert!(view.outline_symbols(b"README.md").unwrap().is_none());
        assert!(view.outline_symbols(b"Cargo.toml").unwrap().is_none());

        // A reset cache is not filled for the generation it left.
        let stale = cache.write().unwrap().reset() - 1;
        let report = prewarm(&view, &cache, stale, &|| false).unwrap();
        assert!(report.stopped());
        assert_eq!(cache.read().unwrap().len(), 0);
    }

    /// The whole path a configure takes: the bind's prewarm fills the cache
    /// once for the root and generation, and a later bind changes nothing.
    #[test]
    fn a_bind_prewarms_once_per_root_and_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let sources = fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);
        let cache = ctx.symbol_cache();
        root_bound(&ctx, ctx.subc_lifecycle_admission());
        let deadline = Instant::now() + Duration::from_secs(60);
        while cache.read().unwrap().len() < sources.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(cache.read().unwrap().len(), sources.len());
        let generation = cache.read().unwrap().generation();
        assert!(already_claimed_for_test(&cache, generation, &root));

        // A reset (a reconfigure, an idle eviction) is filled again.
        cache.write().unwrap().reset();
        root_bound(&ctx, ctx.subc_lifecycle_admission());
        let deadline = Instant::now() + Duration::from_secs(60);
        while cache.read().unwrap().len() < sources.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(cache.read().unwrap().len(), sources.len());
    }

    /// Runs `git` in `dir` with an identity of its own, so a commit needs
    /// no configuration from the machine.
    #[cfg(unix)]
    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=AFT",
                "-c",
                "user.email=aft@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// AFT's own sources for a checkout of real size: the store crate's
    /// Rust files and the bridge's TypeScript files, relative to `root`
    /// once copied there.
    #[cfg(unix)]
    fn aft_sources(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        let crate_dir = crate::test_support::manifest_dir();
        let repo = crate_dir.parent().unwrap().parent().unwrap();
        let mut found = Vec::new();
        walk(&repo.join("crates/aft-store/src"), &mut found);
        for entry in std::fs::read_dir(repo.join("packages/aft-bridge/src")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "ts") {
                found.push(path);
            }
        }
        let mut sources = found
            .iter()
            .map(|path| {
                let relative = path.strip_prefix(repo).unwrap().to_str().unwrap();
                let relative = relative.replace('\\', "/");
                copy(root, path, &relative);
                relative
            })
            .collect::<Vec<_>>();
        sources.sort();
        sources
    }

    /// `value` without the keys that time an answer, which differ between
    /// any two runs.
    #[cfg(unix)]
    fn untimed(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => map
                .into_iter()
                .filter(|(key, _)| !key.ends_with("_ms"))
                .map(|(key, value)| (key, untimed(value)))
                .collect(),
            serde_json::Value::Array(items) => items.into_iter().map(untimed).collect(),
            other => other,
        }
    }

    /// What outline, zoom and aft_search answer in `ctx`: an outline of
    /// every file, a zoom on each of `symbols`, and a search for each of
    /// their names. Searches are asked again until two rounds agree, so an
    /// index still building in the background does not count as a change.
    #[cfg(unix)]
    fn answers(
        ctx: &AppContext,
        root: &Path,
        sources: &[String],
        symbols: &[(String, String)],
    ) -> Vec<serde_json::Value> {
        let mut answers = Vec::new();
        for source in sources {
            let answer = outline(ctx, &root.join(source));
            answers.push(untimed(serde_json::to_value(answer).unwrap()));
        }
        for (source, name) in symbols {
            let answer = crate::commands::zoom::handle_zoom(
                &request(
                    "zoom",
                    serde_json::json!({ "file": root.join(source), "symbol": name }),
                ),
                ctx,
            );
            answers.push(untimed(serde_json::to_value(answer).unwrap()));
        }
        let search = || {
            symbols
                .iter()
                .map(|(_, name)| {
                    let answer = crate::commands::semantic_search::handle_semantic_search(
                        &request(
                            "semantic_search",
                            serde_json::json!({ "query": name, "top_k": 5 }),
                        ),
                        ctx,
                    );
                    untimed(serde_json::to_value(answer).unwrap())
                })
                .collect::<Vec<_>>()
        };
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut last = search();
        loop {
            let next = search();
            if next == last || Instant::now() > deadline {
                break;
            }
            last = next;
        }
        answers.extend(last);
        answers
    }

    /// A worktree whose files match its sibling's takes the sibling's
    /// symbol tables: it reads none of its records and none of its files,
    /// its cache leaves the shared tables to the process's count, and
    /// outline, zoom and aft_search answer exactly as when the worktree
    /// decodes every record itself, as it did before tables were shared.
    /// Stat evidence proves content only on platforms whose file times
    /// change on every write, so the test runs on Unix.
    #[cfg(unix)]
    #[test]
    fn a_matching_worktree_takes_its_siblings_tables_and_answers_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let main = base.join("main");
        let worktree = base.join("worktree");
        std::fs::create_dir_all(&main).unwrap();
        let sources = aft_sources(&main);
        git(&main, &["init", "-q"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-q", "-m", "sources"]);
        git(
            &main,
            &["worktree", "add", "-q", worktree.to_str().unwrap()],
        );
        // A file modified within the store's racy window of its scan proves
        // nothing by its stat: let every file age past it first.
        std::thread::sleep(Duration::from_nanos(
            aft_store::stat::RACY_WINDOW_NS as u64 + 200_000_000,
        ));
        let storage = tempfile::tempdir().unwrap();
        for root in [&main, &worktree] {
            let outcome = crate::data_layer::surfaces::index_root(
                root,
                storage.path(),
                crate::data_layer::checkout::Planes::ALL,
            )
            .unwrap();
            assert!(outcome.published);
        }
        let main_ctx = context(&main, storage.path());
        let worktree_ctx = context(&worktree, storage.path());
        let view = |ctx: &AppContext| {
            let view = ctx.data_checkout().unwrap().view().unwrap();
            assert!(view.status().callgraph_published);
            view
        };
        let (main_view, worktree_view) = (view(&main_ctx), view(&worktree_ctx));
        let files = sources.len() as u64;

        // Before tables were shared, the worktree decoded every record.
        let cache = worktree_ctx.symbol_cache();
        let generation = cache.read().unwrap().generation();
        let reads = record_reads();
        let decoded = tables::without_sharing(|| {
            prewarm(&worktree_view, &cache, generation, &|| false).unwrap()
        });
        assert_eq!(decoded.loaded(), files);
        assert_eq!(record_reads() - reads, files);
        let decoded_bytes = cache.read().unwrap().estimated_memory().estimated_bytes;
        // Up to four top-level symbols of each file to zoom on and search.
        let mut symbols = Vec::new();
        let mut table_bytes = 0;
        for source in &sources {
            let table = crate::parser::FileParser::with_symbol_cache(Arc::clone(&cache))
                .extract_symbols(&worktree.join(source))
                .unwrap();
            table_bytes += crate::parser::symbols_estimated_bytes(&table);
            symbols.extend(
                table
                    .iter()
                    .filter(|symbol| symbol.parent.is_none())
                    .take(4)
                    .map(|symbol| (source.clone(), symbol.name.clone())),
            );
        }
        assert!(symbols.len() > 100, "{} symbols", symbols.len());
        let before = answers(&worktree_ctx, &worktree, &sources, &symbols);
        // The answers compared are real ones: every outline and search
        // succeeds, and nearly every zoom finds its symbol.
        let succeeded = |answers: &[serde_json::Value]| {
            answers
                .iter()
                .filter(|answer| answer["success"] == true)
                .count()
        };
        let (outlines, rest) = before.split_at(sources.len());
        let (zooms, searches) = rest.split_at(symbols.len());
        assert_eq!(succeeded(outlines), outlines.len());
        assert!(
            succeeded(zooms) * 10 >= zooms.len() * 9,
            "{} of {} zooms found their symbol",
            succeeded(zooms),
            zooms.len()
        );
        assert_eq!(
            succeeded(searches),
            searches.len(),
            "{:?}",
            searches.first()
        );

        // The main checkout's prewarm lists its tables for the process.
        let main_cache = main_ctx.symbol_cache();
        let main_generation = main_cache.read().unwrap().generation();
        let main_report = prewarm(&main_view, &main_cache, main_generation, &|| false).unwrap();
        assert_eq!(main_report.loaded(), files);

        // The worktree bound again takes them: no record and no file read.
        let generation = cache.write().unwrap().reset();
        let reads = record_reads();
        let source_reads = work_counters::SOURCE_READS.with(std::cell::Cell::get);
        let shared = prewarm(&worktree_view, &cache, generation, &|| false).unwrap();
        assert_eq!(shared.loaded(), files);
        assert_eq!(record_reads() - reads, 0, "records read");
        assert_eq!(
            work_counters::SOURCE_READS.with(std::cell::Cell::get) - source_reads,
            0,
            "files read"
        );
        // Its cache counts every table but the shared ones, which the
        // process counts once.
        let shared_bytes = cache.read().unwrap().estimated_memory().estimated_bytes;
        assert_eq!(shared_bytes.map(|bytes| bytes + table_bytes), decoded_bytes);
        let listed = loop {
            // Another test's prewarm may hold the tables for a moment.
            if let Some(stats) = tables::shared_bytes() {
                break stats;
            }
            std::thread::yield_now();
        };
        assert!(listed >= table_bytes);

        let after = answers(&worktree_ctx, &worktree, &sources, &symbols);
        assert_eq!(after.len(), before.len());
        for (after, before) in after.iter().zip(&before) {
            assert_eq!(after, before);
        }
    }

    #[cfg(test)]
    use aft_engine::test_bridge::display_data::CheckoutViewTestExt as _;
    #[cfg(test)]
    use aft_engine::test_bridge::display_data::DataCheckoutTestExt as _;
}
