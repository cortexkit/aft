//! Outline's symbols from the data layer.
//!
//! Outline, zoom and `trace_data` read a file's symbols through the
//! provider's shared symbol cache, which parses the file on a miss. When the
//! data layer runs, the content record of every parsed file also holds the
//! parser's symbol table for it, so the cache is filled from records instead
//! of parsing: for the whole checkout once a configure binds the root
//! ([`root_bound`]), and for `trace_data`'s own file before it looks a
//! symbol up ([`warm_file`]). A record is used only while the file on disk
//! still holds the content it was extracted from; any other file is left to
//! the ordinary parse on first use.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use aft_store::content::OutlineSymbol;
use aft_store::ids::ContentKey;

use super::checkout::{CheckoutView, DataError};
use super::runtime::DataCheckout;
use crate::context::AppContext;
use crate::parser::{SharedSymbolCache, SymbolCache};

/// One file's outline as its stored content record holds it.
pub(crate) struct StoredOutline {
    /// The content the symbols were extracted from.
    pub(crate) content: ContentKey,
    /// The language the content was parsed as.
    pub(crate) language: String,
    pub(crate) symbols: Vec<OutlineSymbol>,
}

/// What one prewarm did with the checkout's files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Prewarm {
    /// Already in the cache for the file's current modification time.
    pub(crate) cached: u64,
    /// Filled from their records.
    pub(crate) loaded: u64,
    /// Left for an ordinary parse: changed since the publication or on disk,
    /// unreadable, or with no outline in their record.
    pub(crate) left: u64,
    /// The prewarm stopped before it visited every file.
    pub(crate) stopped: bool,
}

/// What became of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Cached,
    Loaded,
    Left,
    /// The cache was reset to another generation: nothing more is kept.
    Superseded,
}

/// Fills `cache`, while it is at `generation`, with the symbols of every
/// file of `view` whose record holds them and whose bytes on disk are still
/// the ones the record was extracted from. `stop` is asked between files.
///
/// # Errors
///
/// When a record cannot be read.
pub(crate) fn prewarm(
    view: &CheckoutView,
    cache: &SharedSymbolCache,
    generation: u64,
    stop: &dyn Fn() -> bool,
) -> Result<Prewarm, DataError> {
    let root = view.root().to_path_buf();
    if let Ok(mut cache) = cache.write() {
        if !cache.set_project_root_for_generation(generation, root.clone()) {
            return Ok(Prewarm {
                stopped: true,
                ..Prewarm::default()
            });
        }
    }
    let mut report = Prewarm::default();
    for record in view.current_state().paths() {
        if record.language.is_empty() || record.language == "config" {
            continue;
        }
        if stop() {
            report.stopped = true;
            break;
        }
        let Some(path) = absolute(&root, &record.path) else {
            report.left += 1;
            continue;
        };
        match load(view, &record.path, &path, cache, generation)? {
            Outcome::Cached => report.cached += 1,
            Outcome::Loaded => report.loaded += 1,
            Outcome::Left => report.left += 1,
            Outcome::Superseded => {
                report.stopped = true;
                break;
            }
        }
    }
    Ok(report)
}

/// Fills `cache` with the symbols of `file` (absolute, or relative to the
/// root) from its record in `view`, unless the cache holds them already.
/// Returns whether the cache now holds them for the file as it is on disk.
/// A file whose record cannot serve is left to the ordinary parse.
pub(crate) fn warm_file(view: &CheckoutView, cache: &SharedSymbolCache, file: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = canonical(view.root());
    let path = canonical(&if file.is_relative() {
        view.root().join(file)
    } else {
        file.to_path_buf()
    });
    let Some(relative) = path
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .map(|relative| relative.replace('\\', "/"))
    else {
        return false;
    };
    let Ok(generation) = cache.read().map(|cache| cache.generation()) else {
        return false;
    };
    matches!(
        load(view, relative.as_bytes(), &path, cache, generation),
        Ok(Outcome::Cached | Outcome::Loaded)
    )
}

/// The absolute path of `relative`, a `/`-separated path under `root`,
/// spelled with the platform's separators as the symbol cache's keys are.
fn absolute(root: &Path, relative: &[u8]) -> Option<PathBuf> {
    let relative = std::str::from_utf8(relative).ok()?;
    Some(
        relative
            .split('/')
            .fold(root.to_path_buf(), |path, part| path.join(part)),
    )
}

/// Loads the symbols of the file at `relative` (absolute `path`) into
/// `cache` at `generation`, as a parse would have stored them: under the
/// modification time seen before reading, with the bytes' size and hash.
fn load(
    view: &CheckoutView,
    relative: &[u8],
    path: &Path,
    cache: &SharedSymbolCache,
    generation: u64,
) -> Result<Outcome, DataError> {
    let Ok(modified) = crate::bounded_io::metadata(path).and_then(|metadata| metadata.modified())
    else {
        return Ok(Outcome::Left);
    };
    match cache.read() {
        Ok(cache) if cache.generation() == generation => {
            if cache.contains_path_with_mtime(path, modified) {
                return Ok(Outcome::Cached);
            }
        }
        _ => return Ok(Outcome::Superseded),
    }
    let Some(stored) = view.outline_symbols(relative)? else {
        return Ok(Outcome::Left);
    };
    if !parsed_as(path, &stored.language) {
        return Ok(Outcome::Left);
    }
    // A parse reads the file as UTF-8 text and fails otherwise, so a file
    // that is not text is left to fail the same way.
    let Ok(source) = crate::parser::read_source(path) else {
        return Ok(Outcome::Left);
    };
    if ContentKey::of(source.as_bytes()) != stored.content {
        return Ok(Outcome::Left);
    }
    let Some(symbols) = stored
        .symbols
        .iter()
        .map(super::content::symbol_from_outline)
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(Outcome::Left);
    };
    let Ok(mut cache) = cache.write() else {
        return Ok(Outcome::Superseded);
    };
    if cache.generation() != generation {
        return Ok(Outcome::Superseded);
    }
    cache.insert(
        path.to_path_buf(),
        modified,
        source.len() as u64,
        crate::parser::content_hash_for_source(&source),
        symbols,
    );
    Ok(Outcome::Loaded)
}

/// Whether a parse of `path` would use `language`, the one its record was
/// extracted as. Both choose by the same rules; this guards against them
/// drifting apart. A `.h` header's language comes from parsing its bytes,
/// so any of the languages it can resolve to is accepted.
fn parsed_as(path: &Path, language: &str) -> bool {
    if path.extension().is_some_and(|extension| extension == "h") {
        return matches!(language, "c" | "cpp" | "objc");
    }
    crate::parser::language_for_extension(path)
        .is_some_and(|lang| format!("{lang:?}").to_lowercase() == language)
}

/// How often a prewarm waiting for the checkout's first call graph looks
/// again.
const WAIT_STEP: Duration = Duration::from_millis(250);

/// Fills the root's symbol cache from the data layer's records on a thread
/// of its own, when the data layer runs: once for each root and cache
/// generation, so the configure tails that call this again change nothing.
/// It waits for the checkout's first call graph when none is published yet,
/// since records are stored with it, and stops when the cache is reset, the
/// checkout is replaced or the root is abandoned.
pub(crate) fn root_bound(ctx: &AppContext) {
    let Some(checkout) = ctx.data_checkout() else {
        return;
    };
    let cache = ctx.symbol_cache();
    let Ok(generation) = cache.read().map(|cache| cache.generation()) else {
        return;
    };
    let root = checkout.root().to_path_buf();
    let Some(claim) = Claim::take(&cache, generation, &root) else {
        return;
    };
    let lifecycle = ctx.subc_lifecycle_admission();
    let checkout = Arc::downgrade(&checkout);
    let session = crate::log_ctx::current_session();
    let spawned = std::thread::Builder::new()
        .name("aft-data-symbol-prewarm".to_string())
        .spawn(move || {
            crate::log_ctx::with_session(session, || {
                let stop = || {
                    let mut abandoned = false;
                    lifecycle.cancel_if_abandoned(|| abandoned = true);
                    abandoned
                        || checkout.strong_count() == 0
                        || !cache
                            .read()
                            .is_ok_and(|cache| cache.generation() == generation)
                };
                let Some(view) = first_call_graph(&checkout, &stop) else {
                    return;
                };
                let started = Instant::now();
                match prewarm(&view, &cache, generation, &stop) {
                    Ok(report) => {
                        crate::slog_info!(
                            "pre-warmed symbol cache from the data layer: {} loaded, {} cached, {} left, {:.0} ms root={}",
                            report.loaded,
                            report.cached,
                            report.left,
                            started.elapsed().as_secs_f64() * 1000.0,
                            root.display()
                        );
                        if !report.stopped {
                            claim.keep();
                        }
                    }
                    Err(error) => crate::slog_warn!(
                        "data layer symbol prewarm failed for {}: {error}",
                        root.display()
                    ),
                }
            });
        });
    if let Err(error) = spawned {
        crate::slog_warn!("data layer symbol prewarm could not start: {error}");
    }
}

/// The checkout's view once it holds a published call graph, whose records
/// hold the outlines; `None` when `stop` asks first.
fn first_call_graph(
    checkout: &Weak<DataCheckout>,
    stop: &dyn Fn() -> bool,
) -> Option<CheckoutView> {
    loop {
        if stop() {
            return None;
        }
        let view = checkout.upgrade()?.view();
        if let Some(view) = view.filter(|view| view.status().callgraph_published) {
            return Some(view);
        }
        std::thread::sleep(WAIT_STEP);
    }
}

/// The roots and cache generations a prewarm has run or is running for.
struct Claimed {
    cache: Weak<RwLock<SymbolCache>>,
    generation: u64,
    root: PathBuf,
}

fn claims() -> &'static Mutex<Vec<Claimed>> {
    static CLAIMS: Mutex<Vec<Claimed>> = Mutex::new(Vec::new());
    &CLAIMS
}

/// A prewarm's claim on its root and cache generation. Dropped without
/// [`Claim::keep`], it is given back, so a later bind tries again.
struct Claim {
    cache: Weak<RwLock<SymbolCache>>,
    generation: u64,
    root: PathBuf,
    kept: bool,
}

impl Claim {
    /// Claims `root` at `generation` of `cache`, unless a prewarm has.
    fn take(cache: &SharedSymbolCache, generation: u64, root: &Path) -> Option<Self> {
        let mut claims = claims()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        claims.retain(|claimed| claimed.cache.strong_count() > 0);
        let weak = Arc::downgrade(cache);
        if claims.iter().any(|claimed| {
            claimed.cache.ptr_eq(&weak) && claimed.generation == generation && claimed.root == root
        }) {
            return None;
        }
        claims.push(Claimed {
            cache: weak.clone(),
            generation,
            root: root.to_path_buf(),
        });
        Some(Self {
            cache: weak,
            generation,
            root: root.to_path_buf(),
            kept: false,
        })
    }

    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        claims()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|claimed| {
                !(claimed.cache.ptr_eq(&self.cache)
                    && claimed.generation == self.generation
                    && claimed.root == self.root)
            });
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::context::AppContext;
    use crate::parser::work_counters;
    use crate::protocol::{RawRequest, Response};

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
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            ("src/data_layer/runtime.rs", "src/runtime.rs"),
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
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
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

    /// After a restart the checkout's records fill the symbol cache with
    /// the tables a parse gives, and outline then answers as a context that
    /// parses every file does, without parsing.
    #[test]
    fn a_restart_fills_the_symbol_cache_from_records_with_todays_tables() {
        crate::data_layer::runtime::force_on_for_test();
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
        assert_eq!(
            report,
            Prewarm {
                loaded: sources.len() as u64,
                ..Prewarm::default()
            }
        );

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
        assert_eq!(again.cached, sources.len() as u64);
        assert_eq!(again.loaded, 0);
    }

    /// A file that moved on from its published content, on disk only or
    /// recorded as changed too, is left to a parse; so is every file once
    /// the cache is reset.
    #[test]
    fn files_changed_since_their_record_are_left_to_a_parse() {
        crate::data_layer::runtime::force_on_for_test();
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
        assert_eq!(report.left, 1);
        assert_eq!(report.loaded, sources.len() as u64 - 1);
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
        assert!(report.stopped);
        assert_eq!(cache.read().unwrap().len(), 0);
    }

    fn trace_data(ctx: &AppContext, file: &Path, symbol: &str, expression: &str) -> Response {
        crate::commands::trace_data::handle_trace_data(
            &request(
                "trace_data",
                serde_json::json!({
                    "file": file,
                    "symbol": symbol,
                    "expression": expression,
                    "depth": 5,
                }),
            ),
            ctx,
        )
    }

    /// The answer's own fields, without the list envelope around them.
    fn trace_fields(value: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "expression": value["expression"],
            "origin_file": value["origin_file"],
            "origin_symbol": value["origin_symbol"],
            "hops": value["hops"],
            "depth_limited": value["depth_limited"],
        })
    }

    /// Today's call-graph store of `root`, built cold.
    fn todays_store(root: &Path) -> (crate::callgraph_store::CallGraphStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::callgraph_store::CallGraphStore::open(
            dir.path().join("store"),
            root.to_path_buf(),
        )
        .unwrap();
        store
            .cold_build(&crate::callgraph::walk_project_files(root).collect::<Vec<_>>())
            .unwrap();
        (store, dir)
    }

    /// After a restart `trace_data` takes its file's symbols from the
    /// file's record, parsing nothing to find them, and answers as it does
    /// when it parses the file, and as today's store does.
    #[test]
    fn trace_data_looks_its_symbol_up_in_the_record_with_todays_answers() {
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);
        let (store, _store_dir) = todays_store(&root);

        let cases = [
            ("web/data_flow.ts", "transformData", "rawInput"),
            ("web/data_flow.ts", "complexFlow", "data"),
            ("src/lib.rs", "start", "raw"),
            ("src/store.rs", "Store::open", "path"),
            ("tool/main.py", "process", "value"),
        ];
        let (mut hops, mut exact) = (0, 0);
        for (file, symbol, expression) in cases {
            let path = root.join(file);
            assert_eq!(
                ctx.symbol_cache().read().unwrap().contains_key(&path),
                file == "web/data_flow.ts" && symbol != "transformData",
                "{file}: only a file traced before is in the cache"
            );
            let before = work_counters::tree_parses();
            let answer = trace_data(&ctx, &path, symbol, expression);
            assert!(answer.success, "{file} {symbol}: {:?}", answer.data);
            assert_eq!(
                work_counters::tree_parses(),
                before,
                "{file} {symbol}: the symbol lookup parsed"
            );
            assert!(ctx.symbol_cache().read().unwrap().contains_key(&path));
            let answer = trace_fields(&answer.data);
            for hop in answer["hops"].as_array().into_iter().flatten() {
                hops += 1;
                exact += usize::from(hop["approximate"] == false);
            }

            let view = ctx.data_view().unwrap();
            let parsing = crate::commands::callgraph_store_adapter::trace_data_result(
                &view.callgraph(),
                &path,
                symbol,
                expression,
                5,
                fresh_cache(),
            )
            .unwrap();
            assert_eq!(
                answer,
                trace_fields(&serde_json::to_value(&parsing).unwrap()),
                "{file} {symbol}: against a parse"
            );
            let today = crate::commands::callgraph_store_adapter::trace_data_result(
                &store,
                &path,
                symbol,
                expression,
                5,
                fresh_cache(),
            )
            .unwrap();
            assert_eq!(
                answer,
                trace_fields(&serde_json::to_value(&today).unwrap()),
                "{file} {symbol}: against today's store"
            );
        }
        assert!(hops > 0, "no case traced anything");
        // Unchanged files are fresh: their hops are exact.
        assert!(exact > 0, "every hop was approximate");
    }

    fn impact_fields(
        store: &impl crate::callgraph_store::CallGraphRead,
        file: &Path,
        symbol: &str,
    ) -> serde_json::Value {
        let result =
            crate::commands::callgraph_store_adapter::impact_result(store, file, symbol, 5, true)
                .unwrap_or_else(|error| panic!("impact of {symbol}: {error}"));
        serde_json::to_value(&result).unwrap()
    }

    /// `impact` and `trace_data` on the data layer's graph answer as today's
    /// store does: exact for files that hold the content the graph was built
    /// from, and, after an edit no one reported, stale for the edited file:
    /// call expressions withheld and every hop and caller approximate.
    #[test]
    fn impact_and_trace_data_see_fresh_and_edited_files_as_today() {
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);
        let (store, _store_dir) = todays_store(&root);
        let checkout = ctx.data_checkout().expect("the data layer binds");

        let targets = [
            ("web/data_processor.ts", "processInput"),
            ("src/store.rs", "Store::open"),
            ("tool/helpers.py", "clean"),
        ];
        let callers = |answer: &serde_json::Value| -> Vec<serde_json::Value> {
            answer["callers"].as_array().cloned().unwrap_or_default()
        };
        let mut shown = 0;
        for (file, symbol) in targets {
            let path = root.join(file);
            let view = checkout.view().unwrap();
            let ours = impact_fields(&view.callgraph(), &path, symbol);
            assert_eq!(
                ours,
                impact_fields(&store, &path, symbol),
                "{file} {symbol}"
            );
            shown += callers(&ours)
                .iter()
                .filter(|caller| caller["call_expression"].is_string())
                .count();
        }
        // Unchanged files are fresh: their calls are shown.
        assert!(shown > 0, "no caller showed its call");

        // Edit a caller's file on disk; nothing records the change.
        let edited = root.join("web/data_flow.ts");
        let text = std::fs::read_to_string(&edited).unwrap();
        std::fs::write(&edited, format!("// edited\n{text}")).unwrap();
        let view = checkout.view().unwrap();
        let target = root.join("web/data_processor.ts");
        let ours = impact_fields(&view.callgraph(), &target, "processInput");
        assert_eq!(ours, impact_fields(&store, &target, "processInput"));
        let stale = callers(&ours)
            .into_iter()
            .filter(|caller| caller["caller_file"] == "web/data_flow.ts")
            .collect::<Vec<_>>();
        assert!(!stale.is_empty(), "{ours}");
        assert!(
            stale
                .iter()
                .all(|caller| caller["call_expression"].is_null() && caller["approximate"] == true),
            "{ours}"
        );

        let answer = trace_data(&ctx, &edited, "transformData", "rawInput");
        assert!(answer.success, "{:?}", answer.data);
        let answer = trace_fields(&answer.data);
        let today = crate::commands::callgraph_store_adapter::trace_data_result(
            &store,
            &edited,
            "transformData",
            "rawInput",
            5,
            fresh_cache(),
        )
        .unwrap();
        assert_eq!(answer, trace_fields(&serde_json::to_value(&today).unwrap()));
        assert!(
            answer["hops"].as_array().is_some_and(
                |hops| !hops.is_empty() && hops.iter().all(|hop| hop["approximate"] == true)
            ),
            "{answer}"
        );
    }

    /// The whole path a configure takes: the bind's prewarm fills the cache
    /// once for the root and generation, and a later bind changes nothing.
    #[test]
    fn a_bind_prewarms_once_per_root_and_generation() {
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let sources = fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);
        let cache = ctx.symbol_cache();
        root_bound(&ctx);
        let deadline = Instant::now() + Duration::from_secs(60);
        while cache.read().unwrap().len() < sources.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(cache.read().unwrap().len(), sources.len());
        let generation = cache.read().unwrap().generation();
        assert!(Claim::take(&cache, generation, &root).is_none());

        // A reset (a reconfigure, an idle eviction) is filled again.
        cache.write().unwrap().reset();
        root_bound(&ctx);
        let deadline = Instant::now() + Duration::from_secs(60);
        while cache.read().unwrap().len() < sources.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(cache.read().unwrap().len(), sources.len());
    }

    /// With the data layer off, a bind reads no record and `trace_data`
    /// has no view to read one from, even when a publication exists.
    #[test]
    fn with_the_data_layer_off_nothing_reads_records() {
        assert!(!crate::data_layer::runtime::enabled());
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        fixture(&root);
        let (ctx, _storage) = published_then_restarted(&root);
        let cache = ctx.symbol_cache();
        root_bound(&ctx);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(cache.read().unwrap().len(), 0);
        let generation = cache.read().unwrap().generation();
        assert!(Claim::take(&cache, generation, &root).is_some());
        assert!(ctx.data_checkout().is_none());
        assert!(ctx.data_view().is_none());
    }

    /// The process's resident memory in KiB, where the system reports it.
    fn resident_kib() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("VmRSS:"))
                    .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
            })
            .unwrap_or(0)
    }

    fn zlib_fast(bytes: &[u8]) -> usize {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        let packed = encoder.finish().unwrap().len();
        // The store keeps a body packed only when that saves a tenth.
        if packed * 10 <= bytes.len() * 9 {
            packed
        } else {
            bytes.len()
        }
    }

    /// Measures on this repository: the records' bytes and extraction time
    /// with and without outlines, and a prewarm from records against a load
    /// of today's disk symbol cache, checking both give the same tables.
    /// Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "measures the whole repository"]
    fn measure_outline_records_and_prewarm_on_this_repository() {
        use aft_store::content::RecordBody;
        use aft_store::scan::{ContentKind, ContentSink};

        use crate::data_layer::content::{
            extract_parsed, extract_with_tree, language_for, record_from_blob, EXTRACTOR_VERSION,
        };

        let repo =
            std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();

        #[derive(Default)]
        struct Contents(Mutex<Vec<(String, String, Vec<u8>)>>);
        impl ContentSink for Contents {
            fn read(&self, path: &[u8], _key: ContentKey, kind: ContentKind, bytes: &[u8]) {
                if kind == ContentKind::Binary {
                    return;
                }
                let Ok(path) = std::str::from_utf8(path) else {
                    return;
                };
                if let Some(language) = language_for(path, bytes).filter(|l| l != "config") {
                    self.0
                        .lock()
                        .unwrap()
                        .push((path.to_string(), language, bytes.to_vec()));
                }
            }
        }
        let contents = Contents::default();
        let scanned = aft_store::scan::scan(
            &repo,
            &crate::data_layer::content::scan_policy(),
            None,
            &contents,
        )
        .unwrap();
        let mut contents = contents.0.into_inner().unwrap();
        contents.sort();
        let mut seen = std::collections::HashSet::new();
        contents
            .retain(|(_, language, bytes)| seen.insert((ContentKey::of(bytes), language.clone())));

        // Extraction with and without the outline, from the same trees.
        let (mut old_ns, mut new_ns) = (0u128, 0u128);
        let (mut surface, mut body, mut body_packed) = (0usize, 0usize, 0usize);
        let (mut outline_body, mut outline_body_packed) = (0usize, 0usize);
        let (mut largest_growth, mut largest_file) = (0i64, String::new());
        let mut outline_symbols = 0usize;
        for round in 0..2 {
            for (path, language, bytes) in &contents {
                let Ok((_, Some(tree))) = extract_with_tree(bytes, language) else {
                    continue;
                };
                let text = std::str::from_utf8(bytes).unwrap_or("");
                let old = || {
                    let started = Instant::now();
                    let blob = crate::callgraph_store::join::CallgraphBlob::extract_tree(
                        text,
                        language,
                        EXTRACTOR_VERSION,
                        tree_of(&tree),
                    )
                    .unwrap();
                    let record = record_from_blob(&blob);
                    (started.elapsed().as_nanos(), record)
                };
                let new = || {
                    let started = Instant::now();
                    let record = extract_parsed(bytes, language, &tree).unwrap();
                    (started.elapsed().as_nanos(), record)
                };
                // Alternate which runs first, so neither always finds the
                // caches warm.
                let ((old_time, plain), (new_time, record)) = if round == 0 {
                    let old = old();
                    (old, new())
                } else {
                    let new = new();
                    (old(), new)
                };
                old_ns += old_time;
                new_ns += new_time;
                if round == 1 {
                    let (s, b) = record.encode();
                    let (s0, b0) = plain.encode();
                    assert_eq!(s.len(), s0.len(), "{path}");
                    surface += s.len();
                    body += b0.len();
                    outline_body += b.len();
                    body_packed += zlib_fast(&b0);
                    outline_body_packed += zlib_fast(&b);
                    // Adding the outline can tip a body into being stored
                    // packed, so it can also shrink.
                    let growth = zlib_fast(&b) as i64 - zlib_fast(&b0) as i64;
                    if growth > largest_growth {
                        largest_growth = growth;
                        largest_file = path.clone();
                    }
                    if let RecordBody::Parsed { body, .. } = &record.data {
                        outline_symbols += body.outline.as_ref().map_or(0, Vec::len);
                    }
                }
            }
        }
        let records = contents.len();
        println!("records: {records} parsed contents, {outline_symbols} outline symbols");
        println!(
            "extraction (two rounds, parse excluded): {:.0} ms without outline, {:.0} ms with",
            old_ns as f64 / 1e6,
            new_ns as f64 / 1e6
        );
        println!(
            "record bytes without outline: surface {surface}, body {body} ({body_packed} stored)"
        );
        println!(
            "record bytes with outline: surface {surface}, body {outline_body} ({outline_body_packed} stored)"
        );
        println!(
            "stored growth: {} bytes total, {:.0} per file, largest {largest_growth} ({largest_file})",
            outline_body_packed as i64 - body_packed as i64,
            (outline_body_packed as f64 - body_packed as f64) / records as f64
        );

        // The data layer's checkout of the repository, built, then bound
        // again as a restart would.
        let storage = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let outcome = crate::data_layer::surfaces::index_root(
            &repo,
            storage.path(),
            crate::data_layer::checkout::Planes::ALL,
        )
        .unwrap();
        println!(
            "build: {} files, {} parsed, {:.1} s",
            outcome.files,
            outcome.parsed_files,
            started.elapsed().as_secs_f64()
        );
        let checkout = crate::data_layer::runtime::bind_foreground(
            &repo,
            storage.path(),
            crate::data_layer::checkout::Planes::ALL,
        )
        .unwrap();
        let view = checkout.view().unwrap();

        let from_records = fresh_cache();
        let before = resident_kib();
        let started = Instant::now();
        let report = prewarm(&view, &from_records, 0, &|| false).unwrap();
        let records_ms = started.elapsed().as_secs_f64() * 1000.0;
        let records_kib = resident_kib().saturating_sub(before);
        let records_memory = from_records.read().unwrap().estimated_memory();
        println!(
            "prewarm from records: {report:?}, {records_ms:.0} ms, cache {:?} bytes, resident +{records_kib} KiB",
            records_memory.estimated_bytes
        );

        // Where the prewarm's time went, measured after it, so neither part
        // warms a cache for it: reading and hashing the files, and reading
        // their records.
        let parsed_paths = view
            .current_state()
            .paths()
            .iter()
            .filter(|record| !record.language.is_empty() && record.language != "config")
            .collect::<Vec<_>>();
        let started = Instant::now();
        for record in &parsed_paths {
            if let Some(path) = absolute(&repo, &record.path) {
                let _ = crate::parser::read_source(&path)
                    .map(|source| ContentKey::of(source.as_bytes()));
            }
        }
        let reads_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        for record in &parsed_paths {
            let _ = view.outline_symbols(&record.path).unwrap();
        }
        let records_read_ms = started.elapsed().as_secs_f64() * 1000.0;
        println!(
            "prewarm parts, run again: file reads and hashes {reads_ms:.0} ms, record reads {records_read_ms:.0} ms"
        );

        // Today: every file the search index lists with a parser language,
        // parsed and written to the disk symbol cache, then loaded into a
        // fresh cache as a restart's prewarm does.
        let parsed = fresh_cache();
        parsed.write().unwrap().set_project_root(repo.clone());
        let mut parser = crate::parser::FileParser::with_symbol_cache(Arc::clone(&parsed));
        for record in scanned.state.paths() {
            if !record.kind.has_content() {
                continue;
            }
            if let Some(path) = absolute(&repo, &record.path) {
                if crate::parser::detect_language(&path).is_some() {
                    let _ = parser.extract_symbols(&path);
                }
            }
        }
        drop(parser);
        let key = "outline-measure";
        crate::symbol_cache_disk::write_to_disk(&parsed.read().unwrap(), storage.path(), key)
            .unwrap();
        let disk_bytes =
            std::fs::metadata(crate::symbol_cache_disk::cache_path(storage.path(), key))
                .map_or(0, |metadata| metadata.len());
        drop(parsed);
        let from_disk = fresh_cache();
        let before = resident_kib();
        let started = Instant::now();
        let loaded = crate::parser::load_shared_symbol_cache_for_generation(
            &from_disk,
            0,
            storage.path(),
            key,
            &repo,
        );
        let disk_ms = started.elapsed().as_secs_f64() * 1000.0;
        let disk_kib = resident_kib().saturating_sub(before);
        let disk_memory = from_disk.read().unwrap().estimated_memory();
        println!(
            "today's load of symbols.bin ({disk_bytes} bytes): {} files, {disk_ms:.0} ms, cache {:?} bytes, resident +{disk_kib} KiB",
            loaded.loaded, disk_memory.estimated_bytes
        );

        // Both give every file they share the same table.
        let (mut same, mut differing) = (0usize, Vec::new());
        for record in view.current_state().paths() {
            let Some(path) = absolute(&repo, &record.path) else {
                continue;
            };
            let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
                continue;
            };
            let warm = from_records.read().unwrap().get(&path, modified);
            let disk = from_disk.read().unwrap().get(&path, modified);
            match (warm, disk) {
                (Some(warm), Some(disk)) if warm == disk => same += 1,
                (Some(_), Some(_)) => differing.push(path.display().to_string()),
                _ => {}
            }
        }
        println!(
            "tables compared: {same} equal, {} differing",
            differing.len()
        );
        assert!(differing.is_empty(), "{differing:?}");
        assert!(same > 1000, "only {same} files compared");
    }

    /// The tree-sitter tree of an extraction's syntax tree.
    fn tree_of(tree: &crate::data_layer::content::SyntaxTree) -> &tree_sitter::Tree {
        tree.tree_for_test()
    }
}
