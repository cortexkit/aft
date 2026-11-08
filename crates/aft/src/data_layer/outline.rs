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
//!
//! A table taken from a record is shared with every other checkout of the
//! process ([`tables`]), so a worktree whose files match a sibling's takes
//! the sibling's tables instead of decoding the same records again. When
//! the checkout's recorded stat evidence still proves a file's content, by
//! the rule the store's own binds trust, the file is not read either.

pub(crate) mod tables;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use aft_store::content::{OutlineSymbol, RecordKey};
use aft_store::ids::ContentKey;

use super::checkout::{CheckoutView, DataError};
use super::content::EXTRACTOR_VERSION;
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
    let _alloc_label =
        crate::alloc_trace::scope(crate::alloc_trace::Label::SYMBOL_PREWARM, Some(view.root()));
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
        if !super::content::is_parsed_language(&record.language) {
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
/// The table comes from the process's shared tables when another checkout
/// (or an earlier cache of this one) took it from the same record, and from
/// the record otherwise.
fn load(
    view: &CheckoutView,
    relative: &[u8],
    path: &Path,
    cache: &SharedSymbolCache,
    generation: u64,
) -> Result<Outcome, DataError> {
    let Ok(metadata) = crate::bounded_io::metadata(path) else {
        return Ok(Outcome::Left);
    };
    let Ok(modified) = metadata.modified() else {
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
    let Some(record) = view.unchanged_record(relative) else {
        return Ok(Outcome::Left);
    };
    if !parsed_as(path, &record.language) {
        return Ok(Outcome::Left);
    }
    let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
    // The checkout's stat evidence proves the file still holds the bytes
    // its record was read from: a shared table needs no read of the file.
    if record.kind.has_content()
        && aft_store::stat::observe(path)
            .ok()
            .flatten()
            .is_some_and(|now| record.stat.proves_unchanged(&now))
    {
        if let Some(table) = tables::shared().get(key) {
            return Ok(insert(
                cache,
                generation,
                path,
                modified,
                metadata.len(),
                table.content_hash,
                table.symbols,
                true,
            ));
        }
    }
    // A parse reads the file as UTF-8 text and fails otherwise, so a file
    // that is not text is left to fail the same way.
    let Ok(source) = crate::parser::read_source(path) else {
        return Ok(Outcome::Left);
    };
    // The cache's content hash is the bytes' BLAKE3 hash up to its size cap,
    // and a content key is the first half of the same hash.
    let content_hash = crate::parser::content_hash_for_source(&source);
    let content = if source.len() as u64 > crate::cache_freshness::CONTENT_HASH_SIZE_CAP {
        ContentKey::of(source.as_bytes())
    } else {
        ContentKey::from_digest(aft_store::ids::Digest128::from_hash(&content_hash))
    };
    if content != record.content {
        return Ok(Outcome::Left);
    }
    if let Some(table) = tables::shared().get(key) {
        return Ok(insert(
            cache,
            generation,
            path,
            modified,
            source.len() as u64,
            table.content_hash,
            table.symbols,
            true,
        ));
    }
    let Some(stored) = view.outline_symbols(relative)? else {
        return Ok(Outcome::Left);
    };
    let Some(symbols) = stored
        .symbols
        .iter()
        .map(super::content::symbol_from_outline)
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(Outcome::Left);
    };
    let (symbols, shared) = tables::shared().share(key, symbols, content_hash);
    Ok(insert(
        cache,
        generation,
        path,
        modified,
        source.len() as u64,
        content_hash,
        symbols,
        shared,
    ))
}

/// Inserts a file's table into `cache` while it is at `generation`; with
/// `shared`, as a table the process's shared tables count.
#[allow(clippy::too_many_arguments)]
fn insert(
    cache: &SharedSymbolCache,
    generation: u64,
    path: &Path,
    modified: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
    symbols: Arc<Vec<crate::symbols::Symbol>>,
    shared: bool,
) -> Outcome {
    let Ok(mut cache) = cache.write() else {
        return Outcome::Superseded;
    };
    if cache.generation() != generation {
        return Outcome::Superseded;
    }
    let path = path.to_path_buf();
    if shared {
        cache.insert_shared_table(path, modified, size, content_hash, symbols);
    } else {
        cache.insert(
            path,
            modified,
            size,
            content_hash,
            Arc::unwrap_or_clone(symbols),
        );
    }
    Outcome::Loaded
}

#[cfg(test)]
thread_local! {
    /// Content records read for their outline on this thread.
    static RECORD_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts a content record read for its outline, for tests.
#[cfg(test)]
pub(crate) fn note_record_read() {
    RECORD_READS.with(|reads| reads.set(reads.get() + 1));
}

#[cfg(test)]
fn record_reads() -> u64 {
    RECORD_READS.with(std::cell::Cell::get)
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

    /// The whole path a configure takes: the bind's prewarm fills the cache
    /// once for the root and generation, and a later bind changes nothing.
    #[test]
    fn a_bind_prewarms_once_per_root_and_generation() {
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

    /// Runs `git` in `dir` with an identity of its own, so a commit needs
    /// no configuration from the machine.
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
        assert_eq!(decoded.loaded, files);
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

        // The main checkout's prewarm lists its tables for the process.
        let main_cache = main_ctx.symbol_cache();
        let main_generation = main_cache.read().unwrap().generation();
        let main_report = prewarm(&main_view, &main_cache, main_generation, &|| false).unwrap();
        assert_eq!(main_report.loaded, files);

        // The worktree bound again takes them: no record and no file read.
        let generation = cache.write().unwrap().reset();
        let reads = record_reads();
        let source_reads = work_counters::SOURCE_READS.with(std::cell::Cell::get);
        let shared = prewarm(&worktree_view, &cache, generation, &|| false).unwrap();
        assert_eq!(shared.loaded, files);
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
            if let Some(stats) = tables::shared().stats() {
                break stats;
            }
            std::thread::yield_now();
        };
        assert!(listed.bytes >= table_bytes);

        let after = answers(&worktree_ctx, &worktree, &sources, &symbols);
        assert_eq!(after.len(), before.len());
        for (after, before) in after.iter().zip(&before) {
            assert_eq!(after, before);
        }
    }
}
