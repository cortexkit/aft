//! Hash-pinned, rebuildable grammar artifacts. Parsers may load stored grammars;
//! only an explicit command-level request is allowed to start a download.
use super::{
    download, manifest, storage, FailureCause, GrammarManifest, GrammarState, GrammarUnavailable,
};

use std::sync::LazyLock;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::{Condvar, Mutex, RwLock};
use tree_sitter::Language;

static ENGINE: LazyLock<Result<tree_sitter::wasmtime::Engine, String>> = LazyLock::new(|| {
    let mut config = tree_sitter::wasmtime::Config::new();
    config.parallel_compilation(false);
    // Cranelift translates source Wasm to Pulley bytecode only. A grammar must
    // never fall back to native code, including when this target is unavailable.
    config
        .target("pulley64")
        .map_err(|e| format!("Pulley target unavailable: {e}"))?;
    let engine = tree_sitter::wasmtime::Engine::new(&config)
        .map_err(|e| format!("Pulley engine unavailable: {e}"))?;
    if !engine.is_pulley() {
        return Err("grammar engine refused a non-Pulley target".into());
    }
    Ok(engine)
});

/// Report the target family from Wasmtime's actual engine, not the requested
/// configuration. Wasmtime's public query distinguishes Pulley from native.
pub fn engine_target() -> Result<&'static str, GrammarUnavailable> {
    let engine = ENGINE.as_ref().map_err(|error| engine_failure(error))?;
    Ok(if engine.is_pulley() {
        "pulley"
    } else {
        "native"
    })
}

fn engine_failure(error: &str) -> GrammarUnavailable {
    log::warn!("Pulley grammar engine unavailable: {error}");
    GrammarUnavailable::new(&manifest().language, GrammarState::UnsupportedPlatform)
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

struct MonotonicClock(Instant);
impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
}

struct Entry {
    root_generation: u64,
    loading: bool,
    language: Option<Language>,
    state: GrammarState,
    failures: u8,
    cause: Option<FailureCause>,
    retry_at: u64,
    compiles: usize,
    compile_ms: f64,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            root_generation: 0,
            loading: false,
            language: None,
            state: GrammarState::NotDownloaded,
            failures: 0,
            cause: None,
            retry_at: 0,
            compiles: 0,
            compile_ms: 0.0,
        }
    }
}

struct Loaded {
    language: Result<Language, GrammarState>,
    compiles: usize,
    milliseconds: f64,
}

pub struct GrammarRegistry {
    manifest: GrammarManifest,
    storage_root: RwLock<Option<std::path::PathBuf>>,
    entry: Mutex<Entry>,
    changed: Condvar,
    clock: Arc<dyn Clock>,
    transport: OnceLock<Result<Arc<dyn download::Transport>, String>>,
    queries: Vec<String>,
    fault: RwLock<storage::StorageFault>,
    listeners: Mutex<Vec<std::sync::mpsc::Sender<String>>>,
    #[cfg(any(test, feature = "test-hooks"))]
    load_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

static REGISTRY: OnceLock<Arc<GrammarRegistry>> = OnceLock::new();

/// Set the artifact location from the handling context, without reading or
/// compiling a grammar. Call only when the caller opts into dynamic parsing;
/// callers using built-in grammars only must not consult grammar storage.
pub fn configure(config: &crate::config::Config) -> Arc<GrammarRegistry> {
    let root = crate::root_cache::cache_root(config);
    let registry = REGISTRY.get_or_init(|| GrammarRegistry::new(None));
    registry.set_storage_root(root);
    Arc::clone(registry)
}

pub fn registry() -> &'static Arc<GrammarRegistry> {
    REGISTRY.get_or_init(|| GrammarRegistry::new(None))
}

impl GrammarRegistry {
    fn new(root: Option<std::path::PathBuf>) -> Arc<Self> {
        #[cfg(feature = "test-hooks")]
        let root = std::env::var_os("AFT_TEST_GRAMMAR_STORAGE")
            .map(std::path::PathBuf::from)
            .map(Some)
            .unwrap_or(root);
        let registry = Arc::new(Self {
            manifest: manifest().clone(),
            storage_root: RwLock::new(root),
            entry: Mutex::new(Entry::default()),
            changed: Condvar::new(),
            clock: Arc::new(MonotonicClock(Instant::now())),
            transport: OnceLock::new(),
            queries: vec![
                crate::luau::EXTRACTION_QUERY.into(),
                crate::luau::COMPLEXITY_QUERY.into(),
                crate::luau::COMMENT_QUERY.into(),
            ],
            fault: RwLock::new(storage::StorageFault::None),
            listeners: Mutex::new(Vec::new()),
            #[cfg(any(test, feature = "test-hooks"))]
            load_hook: Mutex::new(None),
        });
        #[cfg(feature = "test-hooks")]
        return test_seams::from_environment(registry);
        #[cfg(not(feature = "test-hooks"))]
        registry
    }

    pub fn identity(&self) -> &str {
        &self.manifest.sha256
    }

    pub fn compile_ms(&self) -> f64 {
        self.entry.lock().compile_ms
    }

    fn set_storage_root(&self, root: std::path::PathBuf) {
        let mut entry = self.entry.lock();
        let mut current = self.storage_root.write();
        if current.as_ref() != Some(&root) {
            *current = Some(root);
            let generation = entry.root_generation.wrapping_add(1);
            // Every root uses the same manifest hash, so a compiled language
            // remains in memory. Only root-specific attempts and failures reset.
            *entry = Entry {
                root_generation: generation,
                loading: entry.loading,
                language: entry.language.take(),
                compiles: entry.compiles,
                compile_ms: entry.compile_ms,
                ..Entry::default()
            };
            self.changed.notify_all();
        }
    }

    fn storage(&self) -> Option<storage::Storage> {
        self.storage_root
            .read()
            .clone()
            .map(|root| storage::Storage {
                root,
                fault: *self.fault.read(),
            })
    }

    fn unavailable(&self, entry: &Entry) -> GrammarUnavailable {
        let state = if entry.loading {
            GrammarState::Downloading
        } else {
            entry.state
        };
        let mut result = GrammarUnavailable::new(&self.manifest.language, state);
        if state == GrammarState::AttemptsExhausted {
            result.cause = entry.cause;
        }
        if matches!(
            state,
            GrammarState::NetworkRetry | GrammarState::StorageRetry
        ) {
            result.retry_after_ms = Some(entry.retry_at.saturating_sub(self.clock.now_ms()));
        }
        result
    }

    /// Verify and compile a stored grammar without holding the state mutex.
    /// Waiters release that mutex and have a finite wait; command-level request
    /// calls report an in-progress load immediately instead of waiting for it.
    pub fn stored(&self) -> Result<Language, GrammarUnavailable> {
        let mut entry = self.entry.lock();
        let wait_until = Instant::now() + std::time::Duration::from_secs(30);
        while entry.loading {
            let remaining = wait_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.unavailable(&entry));
            }
            self.changed.wait_for(&mut entry, remaining);
        }
        if let Some(language) = &entry.language {
            return Ok(language.clone());
        }
        if matches!(
            entry.state,
            GrammarState::LoadFailed
                | GrammarState::UnsupportedPlatform
                | GrammarState::Downloading
        ) {
            return Err(self.unavailable(&entry));
        }
        let Some(storage) = self.storage() else {
            return Err(self.unavailable(&entry));
        };
        let generation = entry.root_generation;
        entry.loading = true;
        drop(entry);
        let read = storage.read(&self.manifest);
        let loaded = match &read {
            Ok(Some(bytes)) => Some(self.compile_language(bytes)),
            _ => None,
        };
        let mut entry = self.entry.lock();
        entry.loading = false;
        if let Some(loaded) = loaded {
            self.publish_load(&mut entry, loaded);
        } else if read.is_err() && entry.root_generation == generation {
            let cooling_down =
                entry.state == GrammarState::StorageRetry && self.clock.now_ms() < entry.retry_at;
            if !cooling_down && entry.state != GrammarState::AttemptsExhausted {
                self.failed_attempt(&mut entry, FailureCause::Storage);
            }
        }
        self.changed.notify_all();
        entry
            .language
            .clone()
            .ok_or_else(|| self.unavailable(&entry))
    }

    /// Called only by named-file command handlers. Stored bytes always win over
    /// a download, including the very first qualifying call after a restart.
    pub fn request(self: &Arc<Self>) -> Result<Language, GrammarUnavailable> {
        {
            let entry = self.entry.lock();
            if entry.loading {
                return Err(self.unavailable(&entry));
            }
        }
        if let Ok(language) = self.stored() {
            return Ok(language);
        }
        let mut entry = self.entry.lock();
        if let Some(language) = &entry.language {
            return Ok(language.clone());
        }
        let Some(storage) = self.storage() else {
            return Err(self.unavailable(&entry));
        };
        let retry = matches!(
            entry.state,
            GrammarState::NetworkRetry | GrammarState::StorageRetry
        ) && self.clock.now_ms() >= entry.retry_at;
        if entry.state == GrammarState::NotDownloaded || retry {
            entry.state = GrammarState::Downloading;
            let generation = entry.root_generation;
            let registry = Arc::clone(self);
            if std::thread::Builder::new()
                .name("aft-grammar-download".into())
                .spawn(move || registry.download(storage, generation))
                .is_err()
            {
                self.failed_attempt(&mut entry, FailureCause::Network);
                self.changed.notify_all();
            }
        }
        Err(self.unavailable(&entry))
    }

    fn failed_attempt(&self, entry: &mut Entry, cause: FailureCause) {
        entry.failures += 1;
        entry.cause = Some(cause);
        if entry.failures >= 5 {
            entry.state = GrammarState::AttemptsExhausted;
        } else {
            entry.state = match cause {
                FailureCause::Network => GrammarState::NetworkRetry,
                FailureCause::Storage => GrammarState::StorageRetry,
            };
            entry.retry_at = self
                .clock
                .now_ms()
                .saturating_add(30_000u64 << (entry.failures - 1));
        }
    }

    fn download(&self, storage: storage::Storage, generation: u64) {
        let transport = self.transport.get_or_init(|| {
            download::HttpsTransport::new()
                .map(|transport| Arc::new(transport) as Arc<dyn download::Transport>)
        });
        let fetched = match transport {
            Ok(transport) => download::fetch(&self.manifest, transport.as_ref()),
            Err(_) => Err(download::DownloadFailure::Network),
        };
        let (failure, stored) = match fetched {
            Ok(bytes) => (None, Some(storage.publish(&self.manifest, &bytes))),
            Err(error) => (Some(error), None),
        };
        let mut entry = self.entry.lock();
        if entry.root_generation != generation {
            self.changed.notify_all();
            return;
        }
        match (failure, stored) {
            (Some(download::DownloadFailure::Network), _) => {
                self.failed_attempt(&mut entry, FailureCause::Network)
            }
            (Some(download::DownloadFailure::Verification), _) => {
                entry.state = GrammarState::VerificationFailed
            }
            (_, Some(Err(error))) => {
                log::warn!(
                    "grammar storage unavailable at {}: {error}",
                    storage.folder().display()
                );
                self.failed_attempt(&mut entry, FailureCause::Storage);
            }
            (_, Some(Ok(bytes))) => {
                if entry.language.is_none() {
                    entry.loading = true;
                    drop(entry);
                    let loaded = self.compile_language(&bytes);
                    entry = self.entry.lock();
                    entry.loading = false;
                    self.publish_load(&mut entry, loaded);
                }
            }
            _ => entry.state = GrammarState::LoadFailed,
        }
        self.changed.notify_all();
    }

    fn compile_language(&self, bytes: &[u8]) -> Loaded {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(hook) = self.load_hook.lock().take() {
            hook();
        }
        let engine = match ENGINE.as_ref() {
            Ok(engine) => engine,
            Err(error) => {
                return Loaded {
                    language: Err(engine_failure(error).state),
                    compiles: 0,
                    milliseconds: 0.0,
                }
            }
        };
        let mut compiles = 0;
        let mut milliseconds = 0.0;
        let result = (|| {
            let mut store =
                tree_sitter::WasmStore::new(engine).map_err(|error| error.to_string())?;
            compiles = 1;
            let start = Instant::now();
            let language = store.load_language(&self.manifest.language, bytes);
            milliseconds = start.elapsed().as_secs_f64() * 1000.0;
            let language = language.map_err(|error| error.to_string())?;
            if language.abi_version() != self.manifest.abi {
                return Err("grammar ABI differs from manifest".into());
            }
            for query in &self.queries {
                tree_sitter::Query::new(&language, query).map_err(|error| error.to_string())?;
            }
            Ok::<_, String>(language)
        })();
        let language = result.map_err(|error| {
            log::warn!("{} grammar load failed: {error}", self.manifest.language);
            GrammarState::LoadFailed
        });
        Loaded {
            language,
            compiles,
            milliseconds,
        }
    }

    fn publish_load(&self, entry: &mut Entry, loaded: Loaded) {
        entry.compiles += loaded.compiles;
        entry.compile_ms = loaded.milliseconds;
        match loaded.language {
            Ok(language) => {
                entry.language = Some(language);
                self.listeners
                    .lock()
                    .retain(|listener| listener.send(self.identity().into()).is_ok());
            }
            Err(state) => entry.state = state,
        }
    }

    /// Subscribe before the grammar is loaded; a ready grammar immediately emits its hash.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let entry = self.entry.lock();
        if entry.language.is_some() {
            let _ = tx.send(self.identity().into());
        }
        self.listeners.lock().push(tx);
        rx
    }
}

pub fn prepare_parser(parser: &mut tree_sitter::Parser) -> Result<(), GrammarUnavailable> {
    {
        let engine = ENGINE.as_ref().map_err(|error| engine_failure(error))?;
        let store = tree_sitter::WasmStore::new(engine).map_err(|error| {
            log::warn!("grammar parser store failed: {error}");
            GrammarUnavailable::new(&manifest().language, GrammarState::LoadFailed)
        })?;
        parser.set_wasm_store(store).map_err(|error| {
            log::warn!("grammar parser store assignment failed: {error}");
            GrammarUnavailable::new(&manifest().language, GrammarState::LoadFailed)
        })?;
        Ok(())
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[path = "test_seams.rs"]
pub mod test_seams;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
