//! Compiled only into explicit test-seam builds, including spawned test binaries.
use super::*;

pub use super::download::{DownloadFailure, Response, Transport};
pub use super::storage::{stored_path, StorageFault};

pub fn publish_verified_artifact(
    root: &std::path::Path,
    manifest: &GrammarManifest,
    bytes: &[u8],
) -> std::io::Result<()> {
    if !super::storage::verified(manifest, bytes) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "test grammar failed verification",
        ));
    }
    super::storage::Storage {
        root: root.to_path_buf(),
        fault: StorageFault::None,
    }
    .publish(manifest, bytes)
    .map(|_| ())
}

pub struct TestOptions {
    pub manifest: GrammarManifest,
    pub storage_root: std::path::PathBuf,
    pub clock: Arc<dyn Clock>,
    pub transport: Arc<dyn Transport>,
    pub queries: Vec<String>,
    pub fault: StorageFault,
}

#[cfg(feature = "test-hooks")]
pub(crate) fn from_environment(mut registry: Arc<GrammarRegistry>) -> Arc<GrammarRegistry> {
    let inner = Arc::get_mut(&mut registry).expect("new test registry is unshared");
    if let Some(path) = std::env::var_os("AFT_TEST_GRAMMAR_CLOCK_FILE") {
        struct FileClock(std::path::PathBuf);
        impl Clock for FileClock {
            fn now_ms(&self) -> u64 {
                std::fs::read_to_string(&self.0)
                    .expect("test grammar clock")
                    .trim()
                    .parse()
                    .expect("test clock milliseconds")
            }
        }
        inner.clock = Arc::new(FileClock(path.into()));
    }
    if let Some(path) = std::env::var_os("AFT_TEST_GRAMMAR_TRANSPORT_FILE") {
        struct FileTransport(std::path::PathBuf);
        impl Transport for FileTransport {
            fn get(
                &self,
                _: &reqwest::Url,
                _: std::time::Duration,
            ) -> Result<Response, DownloadFailure> {
                let bytes = std::fs::read(&self.0).map_err(|_| DownloadFailure::Network)?;
                Ok(Response {
                    status: 200,
                    location: None,
                    body: Box::new(std::io::Cursor::new(bytes)),
                })
            }
        }
        let _ = inner
            .transport
            .set(Ok(Arc::new(FileTransport(path.into()))));
    }
    if let Ok(queries) = std::env::var("AFT_TEST_GRAMMAR_QUERIES") {
        inner.queries = serde_json::from_str(&queries).expect("test grammar queries");
    }
    if let Ok(fault) = std::env::var("AFT_TEST_GRAMMAR_FAULT") {
        *inner.fault.write() = match fault.as_str() {
            "create" => StorageFault::Create,
            "admit" => StorageFault::Admit,
            "write" => StorageFault::Write,
            "publish" => StorageFault::Publish,
            "read" => StorageFault::Read,
            _ => panic!("unknown test grammar fault"),
        };
    }
    registry
}

impl GrammarRegistry {
    pub fn with_test_options(options: TestOptions) -> Arc<Self> {
        let transport = OnceLock::new();
        let _ = transport.set(Ok(options.transport));
        Arc::new(Self {
            manifest: options.manifest,
            storage_root: RwLock::new(Some(options.storage_root)),
            entry: Mutex::new(Entry::default()),
            changed: Condvar::new(),
            clock: options.clock,
            transport,
            queries: options.queries,
            fault: RwLock::new(options.fault),
            listeners: Mutex::new(Vec::new()),
        })
    }

    pub fn compile_count(&self) -> usize {
        self.entry.lock().compiles
    }

    pub fn set_storage_fault(&self, fault: StorageFault) {
        *self.fault.write() = fault;
    }

    pub fn wait_until_settled(&self, timeout: std::time::Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut entry = self.entry.lock();
        while entry.state == GrammarState::Downloading && entry.language.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            self.changed.wait_for(&mut entry, remaining);
        }
        true
    }
}
