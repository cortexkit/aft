//! Hash-pinned, rebuildable grammar artifacts. Parsers may load stored grammars;
//! only an explicit command-level request is allowed to start a download.
mod download;
mod manifest;
mod state;
mod storage;

use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Instant;

use parking_lot::{Condvar, Mutex, RwLock};
use tree_sitter::Language;

pub use manifest::{extractor_version, manifest, GrammarManifest, PRODUCTION_MANIFEST};
pub use state::{FailureCause, GrammarState, GrammarUnavailable};

#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
static ENGINE: LazyLock<Result<tree_sitter::wasmtime::Engine, String>> = LazyLock::new(|| {
    let mut config = tree_sitter::wasmtime::Config::new();
    config.parallel_compilation(false);
    tree_sitter::wasmtime::Engine::new(&config).map_err(|e| e.to_string())
});

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

pub struct GrammarRegistry {
    manifest: GrammarManifest,
    storage_root: RwLock<std::path::PathBuf>,
    entry: Mutex<Entry>,
    changed: Condvar,
    clock: Arc<dyn Clock>,
    transport: OnceLock<Result<Arc<dyn download::Transport>, String>>,
    queries: Vec<String>,
    fault: RwLock<storage::StorageFault>,
    listeners: Mutex<Vec<std::sync::mpsc::Sender<String>>>,
}

static REGISTRY: OnceLock<Arc<GrammarRegistry>> = OnceLock::new();

/// Set the artifact location from the handling context, without reading or
/// compiling a grammar. Call only when the handling context's
/// `views_route_root()` enables the views pipeline; other contexts retain
/// built-in-only parsing and must not consult grammar storage.
pub fn configure(config: &crate::config::Config) -> Arc<GrammarRegistry> {
    let root = crate::root_cache::cache_root(config);
    let registry = REGISTRY.get_or_init(|| GrammarRegistry::new(root.clone()));
    *registry.storage_root.write() = root;
    Arc::clone(registry)
}

pub fn registry() -> &'static Arc<GrammarRegistry> {
    REGISTRY.get_or_init(|| {
        GrammarRegistry::new(crate::root_cache::cache_root(
            &crate::config::Config::default(),
        ))
    })
}

impl GrammarRegistry {
    fn new(root: std::path::PathBuf) -> Arc<Self> {
        #[cfg(feature = "test-grammar-seams")]
        let root = std::env::var_os("AFT_TEST_GRAMMAR_STORAGE")
            .map(std::path::PathBuf::from)
            .unwrap_or(root);
        let registry = Arc::new(Self {
            manifest: manifest().clone(),
            storage_root: RwLock::new(root),
            entry: Mutex::new(Entry::default()),
            changed: Condvar::new(),
            clock: Arc::new(MonotonicClock(Instant::now())),
            transport: OnceLock::new(),
            queries: vec!["(chunk) @root".into()],
            fault: RwLock::new(storage::StorageFault::None),
            listeners: Mutex::new(Vec::new()),
        });
        #[cfg(feature = "test-grammar-seams")]
        return test_seams::from_environment(registry);
        #[cfg(not(feature = "test-grammar-seams"))]
        registry
    }

    pub fn identity(&self) -> &str {
        &self.manifest.sha256
    }

    pub fn compile_ms(&self) -> f64 {
        self.entry.lock().compile_ms
    }

    fn storage(&self) -> storage::Storage {
        storage::Storage {
            root: self.storage_root.read().clone(),
            fault: *self.fault.read(),
        }
    }

    fn unavailable(&self, entry: &Entry) -> GrammarUnavailable {
        let mut result = GrammarUnavailable::new(&self.manifest.language, entry.state);
        if entry.state == GrammarState::AttemptsExhausted {
            result.cause = entry.cause;
        }
        if matches!(
            entry.state,
            GrammarState::NetworkRetry | GrammarState::StorageRetry
        ) {
            result.retry_after_ms = Some(entry.retry_at.saturating_sub(self.clock.now_ms()));
        }
        result
    }

    /// A single-flight synchronous stored load; it never starts a download.
    /// The lock also covers query validation, so no caller sees a partial load.
    pub fn stored(&self) -> Result<Language, GrammarUnavailable> {
        let mut entry = self.entry.lock();
        if let Some(language) = &entry.language {
            return Ok(language.clone());
        }
        #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
        {
            entry.state = GrammarState::UnsupportedPlatform;
            return Err(self.unavailable(&entry));
        }
        #[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
        {
            if entry.state == GrammarState::NotDownloaded {
                if let Some(bytes) = self.storage().read(&self.manifest) {
                    self.load(&mut entry, bytes);
                }
            }
            entry
                .language
                .clone()
                .ok_or_else(|| self.unavailable(&entry))
        }
    }

    /// Called only by named-file command handlers. Stored bytes always win over
    /// a download, including the very first qualifying call after a restart.
    pub fn request(self: &Arc<Self>) -> Result<Language, GrammarUnavailable> {
        if let Ok(language) = self.stored() {
            return Ok(language);
        }
        let mut entry = self.entry.lock();
        if let Some(language) = &entry.language {
            return Ok(language.clone());
        }
        let retry = matches!(
            entry.state,
            GrammarState::NetworkRetry | GrammarState::StorageRetry
        ) && self.clock.now_ms() >= entry.retry_at;
        if entry.state == GrammarState::NotDownloaded || retry {
            entry.state = GrammarState::Downloading;
            let registry = Arc::clone(self);
            if std::thread::Builder::new()
                .name("aft-grammar-download".into())
                .spawn(move || registry.download())
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

    fn download(&self) {
        let transport = self.transport.get_or_init(|| {
            download::HttpsTransport::new().map(|t| Arc::new(t) as Arc<dyn download::Transport>)
        });
        let result = match transport {
            Ok(transport) => download::fetch(&self.manifest, transport.as_ref()),
            Err(_) => Err(download::DownloadFailure::Network),
        };
        let (failure, stored) = match result {
            Ok(bytes) => (None, Some(self.storage().publish(&self.manifest, &bytes))),
            Err(error) => (Some(error), None),
        };
        let mut entry = self.entry.lock();
        match (failure, stored) {
            (Some(download::DownloadFailure::Network), _) => {
                self.failed_attempt(&mut entry, FailureCause::Network)
            }
            (Some(download::DownloadFailure::Verification), _) => {
                entry.state = GrammarState::VerificationFailed
            }
            (_, Some(Err(_))) => self.failed_attempt(&mut entry, FailureCause::Storage),
            (_, Some(Ok(bytes))) => self.load(&mut entry, bytes),
            _ => entry.state = GrammarState::LoadFailed,
        }
        self.changed.notify_all();
    }

    #[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
    fn load(&self, entry: &mut Entry, bytes: Vec<u8>) {
        let start = Instant::now();
        let result = (|| {
            let engine = ENGINE.as_ref().map_err(Clone::clone)?;
            let mut store = tree_sitter::WasmStore::new(engine).map_err(|e| e.to_string())?;
            entry.compiles += 1;
            let language = store
                .load_language(&self.manifest.language, &bytes)
                .map_err(|e| e.to_string())?;
            if language.abi_version() != self.manifest.abi {
                return Err("grammar ABI differs from manifest".into());
            }
            for query in &self.queries {
                tree_sitter::Query::new(&language, query).map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(language)
        })();
        entry.compile_ms = start.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok(language) => {
                entry.language = Some(language);
                self.listeners
                    .lock()
                    .retain(|listener| listener.send(self.identity().into()).is_ok());
            }
            Err(error) => {
                log::warn!("{} grammar load failed: {error}", self.manifest.language);
                entry.state = GrammarState::LoadFailed;
            }
        }
        self.changed.notify_all();
    }

    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    fn load(&self, entry: &mut Entry, _bytes: Vec<u8>) {
        entry.state = GrammarState::UnsupportedPlatform;
    }

    /// Subscribe before discovery; a ready grammar immediately emits its hash.
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
    #[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
    {
        let engine = ENGINE
            .as_ref()
            .map_err(|_| GrammarUnavailable::new(&manifest().language, GrammarState::LoadFailed))?;
        let store = tree_sitter::WasmStore::new(engine)
            .map_err(|_| GrammarUnavailable::new(&manifest().language, GrammarState::LoadFailed))?;
        parser
            .set_wasm_store(store)
            .map_err(|_| GrammarUnavailable::new(&manifest().language, GrammarState::LoadFailed))?;
        Ok(())
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        let _ = parser;
        Err(GrammarUnavailable::new(
            &manifest().language,
            GrammarState::UnsupportedPlatform,
        ))
    }
}

#[cfg(feature = "test-grammar-seams")]
pub mod test_seams;

#[cfg(test)]
mod tests;
