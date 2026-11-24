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

#[doc(hidden)]
pub mod tables;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use aft_store::content::OutlineSymbol;
use aft_store::ids::ContentKey;

use super::checkout::{CheckoutView, DataError};
use super::runtime::DataCheckout;
use crate::data_layer::host::IndexHost;
use crate::parser::{SharedSymbolCache, SymbolCache};

/// One file's outline as its stored content record holds it.
#[doc(hidden)]
pub struct StoredOutline {
    /// The content the symbols were extracted from.
    pub(crate) content: ContentKey,
    #[doc(hidden)]
    pub symbols: Vec<OutlineSymbol>,
}

/// What one prewarm did with the checkout's files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct Prewarm {
    /// Already in the cache for the file's current modification time.
    #[doc(hidden)]
    pub cached: u64,
    /// Filled from their records.
    #[doc(hidden)]
    pub loaded: u64,
    /// Left for an ordinary parse: changed since the publication or on disk,
    /// unreadable, or with no outline in their record.
    #[doc(hidden)]
    pub left: u64,
    /// The prewarm stopped before it visited every file.
    #[doc(hidden)]
    pub stopped: bool,
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
#[doc(hidden)]
pub fn prewarm(
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
#[doc(hidden)]
pub fn warm_file(view: &CheckoutView, cache: &SharedSymbolCache, file: &Path) -> bool {
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
    let key = crate::data_layer::content::record_key(record.content, &record.language);
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

#[cfg(any(test, all(unix, feature = "test-support")))]
thread_local! {
    /// Content records read for their outline on this thread.
    static RECORD_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts a content record read for its outline, for tests.
#[cfg(any(test, all(unix, feature = "test-support")))]
pub(crate) fn note_record_read() {
    RECORD_READS.with(|reads| reads.set(reads.get() + 1));
}

#[cfg(all(any(test, feature = "test-support"), unix))]
#[doc(hidden)]
pub fn record_reads() -> u64 {
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
        .or_else(|| crate::parser::detect_language_any(path, cfg!(feature = "wasm-grammars")))
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
#[doc(hidden)]
pub fn root_bound(
    ctx: &dyn IndexHost,
    lifecycle: crate::executor::root_lifecycle::SubcLifecycleAdmission,
) {
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

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn already_claimed_for_test(cache: &SharedSymbolCache, generation: u64, root: &Path) -> bool {
    Claim::take(cache, generation, root).is_none()
}
